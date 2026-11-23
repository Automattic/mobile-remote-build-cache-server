//! HTTP Basic authentication with `read` and `readwrite` roles.
//!
//! Passwords are verified with argon2id. Because credentials arrive on every
//! request and argon2 is deliberately slow, a successful verification is
//! remembered under a keyed hash of the credentials; the key is random per
//! process and never leaves memory. The slow path is protected twice: a
//! semaphore bounds concurrent verifications, and a per-client throttle stops
//! running argon2 for an address that keeps failing.
//!
//! Nothing here logs credentials or the `Authorization` header.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use blake2::Blake2bMac;
use blake2::digest::consts::U32;
use blake2::digest::{FixedOutput, KeyInit, Update};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::config::{Access, Cidr, Config, canonical_ip};
use crate::metrics::Metrics;

/// Upper bound on tracked client addresses in the failure throttle.
const THROTTLE_CAPACITY: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Read,
    ReadWrite,
}

impl Role {
    pub fn allows(self, needed: Role) -> bool {
        self == Role::ReadWrite || needed == Role::Read
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::ReadWrite => "readwrite",
        }
    }
}

fn anonymous_allows(level: Access, needed: Role) -> bool {
    match level {
        Access::None => false,
        Access::Read => needed == Role::Read,
        Access::ReadWrite => true,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed; carries the username unless the request was anonymous.
    Allow(Option<Arc<str>>),
    /// Missing, malformed or wrong credentials: `401`.
    Unauthorized,
    /// Valid credentials, insufficient role: `403`.
    Forbidden,
    /// Too many failures from this client: `429` with `Retry-After` seconds.
    Throttled(u64),
    /// No verification slot became free in time: `503`.
    Busy,
}

#[derive(Debug)]
pub struct User {
    pub role: Role,
    hash: String,
}

type Users = HashMap<String, User>;

/// Parses the users file: one `username:role:argon2id-hash` per line. Blank
/// lines and lines starting with `#` are ignored.
pub fn parse_users(text: &str) -> Result<Users, String> {
    let mut users = Users::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Never echo the line itself: it contains a password hash.
        let at = format!("users file line {}", i + 1);
        let mut parts = line.splitn(3, ':');
        let (Some(name), Some(role), Some(hash)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("{at}: expected username:role:password-hash"));
        };
        if name.is_empty() {
            return Err(format!("{at}: empty username"));
        }
        let role = match role {
            "read" => Role::Read,
            "readwrite" => Role::ReadWrite,
            _ => return Err(format!("{at}: role must be read or readwrite")),
        };
        let parsed =
            PasswordHash::new(hash).map_err(|e| format!("{at}: invalid password hash: {e}"))?;
        if parsed.algorithm.as_str() != "argon2id" {
            return Err(format!("{at}: password hash must be argon2id"));
        }
        Params::try_from(&parsed).map_err(|e| format!("{at}: invalid argon2 parameters: {e}"))?;
        let user = User {
            role,
            hash: hash.to_string(),
        };
        if users.insert(name.to_string(), user).is_some() {
            return Err(format!("{at}: duplicate username {name:?}"));
        }
    }
    Ok(users)
}

/// Hashes a password with the default argon2id parameters and a random salt,
/// returning the PHC string for the users file.
pub fn hash_password(password: &[u8]) -> Result<String, String> {
    hash_password_with(password, Params::default())
}

pub fn hash_password_with(password: &[u8], params: Params) -> Result<String, String> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password)
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing failed: {e}"))
}

/// Hashes with explicit argon2id costs (memory in KiB, iterations, lanes).
/// Tests use it to make cheap hashes; production hashes use the defaults.
pub fn hash_password_with_cost(
    password: &[u8],
    m_kib: u32,
    t: u32,
    p: u32,
) -> Result<String, String> {
    let params =
        Params::new(m_kib, t, p, None).map_err(|e| format!("invalid argon2 parameters: {e}"))?;
    hash_password_with(password, params)
}

/// Verifies a password against a PHC string, using the parameters stored in
/// the hash. Comparison of the digest is constant-time inside the library.
pub fn verify_password(password: &[u8], hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(password, &parsed).is_ok(),
        Err(_) => false,
    }
}

/// Decodes standard base64 (RFC 4648 alphabet, padding optional).
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let unpadded_len = input.iter().rposition(|b| *b != b'=').map_or(0, |i| i + 1);
    let (data, padding) = input.split_at(unpadded_len);
    if padding.len() > 2
        || (!padding.is_empty() && !input.len().is_multiple_of(4))
        || data.len() % 4 == 1
    {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in data {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero, so each value has exactly one encoding.
    (acc == 0).then_some(out)
}

/// Parses an `Authorization: Basic …` header value into username and password.
pub fn parse_basic(header: &[u8]) -> Option<(String, Vec<u8>)> {
    let split = header.iter().position(|b| *b == b' ')?;
    let (scheme, rest) = header.split_at(split);
    if !scheme.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let decoded = base64_decode(rest.trim_ascii())?;
    let colon = decoded.iter().position(|b| *b == b':')?;
    let username = String::from_utf8(decoded[..colon].to_vec()).ok()?;
    Some((username, decoded[colon + 1..].to_vec()))
}

/// Works out the client address for throttling. `X-Forwarded-For` is trusted
/// only when the TCP peer is a configured proxy, and then only its last entry:
/// the one that proxy appended. Anything a client put in front is ignored.
pub fn client_ip(peer: IpAddr, forwarded_for: Option<&[u8]>, trusted: &[Cidr]) -> IpAddr {
    let peer = canonical_ip(peer);
    if !trusted.iter().any(|net| net.contains(peer)) {
        return peer;
    }
    forwarded_for
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|last| parse_forwarded_entry(last.trim()))
        .map_or(peer, canonical_ip)
}

fn parse_forwarded_entry(s: &str) -> Option<IpAddr> {
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }
    // Load balancers can be configured to append the client port.
    if let Ok(addr) = s.parse::<std::net::SocketAddr>() {
        return Some(addr.ip());
    }
    None
}

/// IPv6 clients are throttled per /64, since one host controls a whole one.
fn throttle_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        v4 => v4,
    }
}

struct Verified {
    username: Arc<str>,
    role: Role,
    expires: Instant,
}

#[derive(Default)]
struct VerifiedCache {
    entries: HashMap<[u8; 32], Verified>,
    /// Bumped on every users-file reload, so a verification that started
    /// before a reload cannot repopulate the cache afterwards.
    generation: u64,
}

struct Failures {
    window_start: Instant,
    count: u32,
}

pub struct Auth {
    users_file: Option<PathBuf>,
    users: RwLock<Arc<Users>>,
    anonymous: Access,
    mac_key: [u8; 32],
    cache: Mutex<VerifiedCache>,
    cache_ttl: Duration,
    slots: Arc<Semaphore>,
    wait_timeout: Duration,
    failure_limit: u32,
    failure_window: Duration,
    throttle: Mutex<HashMap<IpAddr, Failures>>,
    /// Verified for unknown usernames, so they cost the same as a wrong password.
    dummy_hash: RwLock<Arc<str>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn load_users(path: &PathBuf) -> Result<Users, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read users file {}: {e}", path.display()))?;
    parse_users(&text)
}

/// A hash to burn time on for unknown usernames, with the same cost
/// parameters as the real hashes.
fn make_dummy_hash(users: &Users) -> Result<Arc<str>, String> {
    let params = users
        .values()
        .next()
        .and_then(|u| PasswordHash::new(&u.hash).ok())
        .and_then(|h| Params::try_from(&h).ok())
        .unwrap_or_default();
    hash_password_with(b"no-such-user", params).map(Arc::from)
}

impl Auth {
    pub fn new(cfg: &Config) -> Result<Auth, String> {
        let users = match &cfg.users_file {
            Some(path) => load_users(path)?,
            None => Users::new(),
        };
        let mut mac_key = [0u8; 32];
        getrandom::fill(&mut mac_key).map_err(|e| format!("cannot get random bytes: {e}"))?;
        Ok(Auth {
            users_file: cfg.users_file.clone(),
            dummy_hash: RwLock::new(make_dummy_hash(&users)?),
            users: RwLock::new(Arc::new(users)),
            anonymous: cfg.anonymous_access,
            mac_key,
            cache: Mutex::new(VerifiedCache::default()),
            cache_ttl: cfg.auth_cache_ttl,
            slots: Arc::new(Semaphore::new(cfg.auth_concurrency)),
            wait_timeout: cfg.auth_wait_timeout,
            failure_limit: cfg.auth_failure_limit,
            failure_window: cfg.auth_failure_window,
            throttle: Mutex::new(HashMap::new()),
        })
    }

    pub fn user_count(&self) -> usize {
        self.users.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Re-reads the users file (SIGHUP). On error the current users stay in
    /// effect. On success every remembered verification is dropped, so removed
    /// users and changed passwords stop working immediately.
    pub fn reload(&self) -> Result<usize, String> {
        let Some(path) = &self.users_file else {
            return Ok(0);
        };
        let users = load_users(path)?;
        let dummy = make_dummy_hash(&users)?;
        let count = users.len();
        *self.users.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(users);
        *self.dummy_hash.write().unwrap_or_else(|e| e.into_inner()) = dummy;
        let mut cache = lock(&self.cache);
        cache.entries.clear();
        cache.generation += 1;
        Ok(count)
    }

    fn mac(&self, username: &str, password: &[u8]) -> [u8; 32] {
        // The key length is fixed and valid, so construction cannot fail; the
        // fallback keeps this panic-free regardless.
        let Ok(mut mac) = Blake2bMac::<U32>::new_from_slice(&self.mac_key) else {
            return [0; 32];
        };
        // Length-prefix the username so (user, password) pairs cannot collide.
        mac.update(&(username.len() as u64).to_le_bytes());
        mac.update(username.as_bytes());
        mac.update(password);
        mac.finalize_fixed().into()
    }

    fn cached(&self, mac: &[u8; 32]) -> Option<(Arc<str>, Role)> {
        let mut cache = lock(&self.cache);
        match cache.entries.get(mac) {
            Some(v) if v.expires > Instant::now() => Some((v.username.clone(), v.role)),
            Some(_) => {
                cache.entries.remove(mac);
                None
            }
            None => None,
        }
    }

    fn throttled(&self, client: IpAddr) -> Option<u64> {
        let throttle = lock(&self.throttle);
        let f = throttle.get(&throttle_key(client))?;
        let elapsed = f.window_start.elapsed();
        (f.count >= self.failure_limit && elapsed < self.failure_window)
            .then(|| (self.failure_window - elapsed).as_secs().max(1))
    }

    fn record_failure(&self, client: IpAddr, metrics: &Metrics) {
        metrics.auth_failures.fetch_add(1, Relaxed);
        let now = Instant::now();
        let mut throttle = lock(&self.throttle);
        let key = throttle_key(client);
        if let Some(f) = throttle.get_mut(&key) {
            if now.duration_since(f.window_start) >= self.failure_window {
                *f = Failures {
                    window_start: now,
                    count: 1,
                };
            } else {
                f.count = f.count.saturating_add(1);
            }
            return;
        }
        if throttle.len() >= THROTTLE_CAPACITY {
            let window = self.failure_window;
            throttle.retain(|_, f| now.duration_since(f.window_start) < window);
        }
        // If the table is still full, this failure goes unrecorded; the
        // verification semaphore remains as the backstop.
        if throttle.len() < THROTTLE_CAPACITY {
            throttle.insert(
                key,
                Failures {
                    window_start: now,
                    count: 1,
                },
            );
        }
    }

    /// Decides whether a request may proceed. `header` is the raw
    /// `Authorization` value, `client` the address from [`client_ip`].
    pub async fn check(
        &self,
        header: Option<&[u8]>,
        client: IpAddr,
        needed: Role,
        metrics: &Metrics,
    ) -> Decision {
        let Some(header) = header else {
            if anonymous_allows(self.anonymous, needed) {
                return Decision::Allow(None);
            }
            metrics.auth_failures.fetch_add(1, Relaxed);
            return Decision::Unauthorized;
        };
        // Credentials that are present must be right, even when anonymous
        // access would have been enough: this surfaces a misconfigured password.
        let Some((username, password)) = parse_basic(header) else {
            self.record_failure(client, metrics);
            return Decision::Unauthorized;
        };
        let grant = |name: Arc<str>, role: Role| {
            if role.allows(needed) {
                Decision::Allow(Some(name))
            } else {
                Decision::Forbidden
            }
        };

        // Fast path: no argon2, not subject to the throttle or the semaphore.
        let mac = self.mac(&username, &password);
        if let Some((name, role)) = self.cached(&mac) {
            return grant(name, role);
        }

        if let Some(retry_after) = self.throttled(client) {
            metrics.auth_throttled.fetch_add(1, Relaxed);
            return Decision::Throttled(retry_after);
        }
        let permit =
            match tokio::time::timeout(self.wait_timeout, self.slots.clone().acquire_owned()).await
            {
                Ok(Ok(permit)) => permit,
                _ => {
                    metrics.auth_busy.fetch_add(1, Relaxed);
                    return Decision::Busy;
                }
            };
        // Another request with the same credentials may have verified them
        // while this one waited for a slot.
        if let Some((name, role)) = self.cached(&mac) {
            return grant(name, role);
        }

        let generation = lock(&self.cache).generation;
        let users = self.users.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (hash, role): (Arc<str>, Option<Role>) = match users.get(&username) {
            Some(user) => (Arc::from(user.hash.as_str()), Some(user.role)),
            None => (
                self.dummy_hash
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                None,
            ),
        };
        // The permit moves into the blocking task, so the slot stays taken for
        // as long as the CPU work runs even if this request is abandoned. It
        // comes back with the result and is released only after the result is
        // in the cache, so the next waiter finds it there instead of running
        // argon2 again for the same credentials.
        metrics.auth_verifications.fetch_add(1, Relaxed);
        let (verified, permit) =
            tokio::task::spawn_blocking(move || (verify_password(&password, &hash), permit))
                .await
                .map_or((false, None), |(verified, permit)| (verified, Some(permit)));

        match role {
            Some(role) if verified => {
                let name: Arc<str> = Arc::from(username.as_str());
                let mut cache = lock(&self.cache);
                if cache.generation == generation {
                    let capacity = users.len() * 4 + 16;
                    if cache.entries.len() >= capacity {
                        let now = Instant::now();
                        cache.entries.retain(|_, v| v.expires > now);
                        if cache.entries.len() >= capacity {
                            cache.entries.clear();
                        }
                    }
                    cache.entries.insert(
                        mac,
                        Verified {
                            username: name.clone(),
                            role,
                            expires: Instant::now() + self.cache_ttl,
                        },
                    );
                }
                drop(cache);
                drop(permit);
                grant(name, role)
            }
            _ => {
                // Failures are never cached: a later correct password is
                // always evaluated on its own merits.
                self.record_failure(client, metrics);
                Decision::Unauthorized
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_hash(password: &str) -> String {
        hash_password_with(password.as_bytes(), Params::new(64, 1, 1, None).unwrap()).unwrap()
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert_eq!(base64_decode(b"Zg==").unwrap(), b"f");
        assert_eq!(base64_decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode(b"Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode(b"Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode(b"Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode(b"Zg").unwrap(), b"f");
        assert_eq!(base64_decode(b"+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
        assert_eq!(base64_decode(b"Z"), None);
        assert_eq!(base64_decode(b"Zm9v="), None);
        assert_eq!(base64_decode(b"Zg==="), None);
        assert_eq!(base64_decode(b"Zh=="), None, "non-canonical trailing bits");
        assert_eq!(base64_decode(b"Zm 9v"), None);
        assert_eq!(base64_decode(b"Zm9v\n"), None);
        assert_eq!(
            base64_decode(b"Zm-_"),
            None,
            "URL-safe alphabet is not accepted"
        );
        assert_eq!(base64_decode(b"=Zm9"), None);
    }

    #[test]
    fn basic_header() {
        // "ci-user:s3cret"
        assert_eq!(
            parse_basic(b"Basic Y2ktdXNlcjpzM2NyZXQ="),
            Some(("ci-user".to_string(), b"s3cret".to_vec()))
        );
        assert_eq!(
            parse_basic(b"basic   Y2ktdXNlcjpzM2NyZXQ=  "),
            Some(("ci-user".to_string(), b"s3cret".to_vec()))
        );
        // "user:pa:ss" — only the first colon separates.
        assert_eq!(
            parse_basic(b"Basic dXNlcjpwYTpzcw=="),
            Some(("user".to_string(), b"pa:ss".to_vec()))
        );
        // "user:" — empty password is a syntactically valid pair.
        assert_eq!(
            parse_basic(b"Basic dXNlcjo="),
            Some(("user".to_string(), vec![]))
        );
        assert_eq!(parse_basic(b"Basic dXNlcg=="), None, "no colon");
        assert_eq!(parse_basic(b"Bearer abc"), None);
        assert_eq!(parse_basic(b"Basic"), None);
        assert_eq!(parse_basic(b"Basic !!!"), None);
        assert_eq!(parse_basic(b""), None);
        // Username that is not UTF-8.
        assert_eq!(parse_basic(b"Basic /zpw"), None);
    }

    #[test]
    fn roles() {
        assert!(Role::Read.allows(Role::Read));
        assert!(!Role::Read.allows(Role::ReadWrite));
        assert!(Role::ReadWrite.allows(Role::Read));
        assert!(Role::ReadWrite.allows(Role::ReadWrite));
        assert!(!anonymous_allows(Access::None, Role::Read));
        assert!(anonymous_allows(Access::Read, Role::Read));
        assert!(!anonymous_allows(Access::Read, Role::ReadWrite));
        assert!(anonymous_allows(Access::ReadWrite, Role::ReadWrite));
    }

    #[test]
    fn users_file() {
        let h = fast_hash("pw");
        let text = format!("# comment\n\nci-user:readwrite:{h}\ndeveloper:read:{h}\n");
        let users = parse_users(&text).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users["ci-user"].role, Role::ReadWrite);
        assert_eq!(users["developer"].role, Role::Read);

        assert!(parse_users("ci-user:readwrite").is_err());
        assert!(parse_users(&format!("ci-user:admin:{h}")).is_err());
        assert!(parse_users(&format!(":read:{h}")).is_err());
        assert!(parse_users("ci-user:read:plaintext").is_err());
        assert!(parse_users(&format!("a:read:{h}\na:read:{h}")).is_err());
        // Other algorithms are refused, including the old node's format.
        assert!(
            parse_users("a:read:$argon2i$v=19$m=64,t=1,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA")
                .is_err()
        );
        assert!(parse_users("a:read:c29tZXNhbHQ=:c29tZWhhc2g=").is_err());
        // Errors point at the line but never repeat the hash.
        let err = parse_users(&format!("ok:read:{h}\nbad:nope:{h}")).unwrap_err();
        assert!(err.contains("line 2") && !err.contains("argon2id"), "{err}");
    }

    #[test]
    fn hashing_round_trip() {
        let h = fast_hash("correct horse");
        assert!(h.starts_with("$argon2id$v=19$m=64,t=1,p=1$"));
        assert!(verify_password(b"correct horse", &h));
        assert!(!verify_password(b"wrong", &h));
        assert!(!verify_password(b"correct horse", "garbage"));
        assert_ne!(h, fast_hash("correct horse"), "salts must differ");
    }

    #[test]
    fn client_address() {
        let lb: Vec<Cidr> = vec![Cidr::parse("10.0.3.0/24").unwrap()];
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // Direct connection: the header is ignored entirely.
        assert_eq!(
            client_ip(ip("10.0.9.9"), Some(b"1.2.3.4"), &lb),
            ip("10.0.9.9")
        );
        assert_eq!(client_ip(ip("10.0.9.9"), None, &lb), ip("10.0.9.9"));
        // Through the load balancer: only the entry it appended counts.
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"203.0.113.7"), &lb),
            ip("203.0.113.7")
        );
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"1.1.1.1, 2.2.2.2,203.0.113.7"), &lb),
            ip("203.0.113.7")
        );
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"203.0.113.7:51234"), &lb),
            ip("203.0.113.7")
        );
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"2001:db8::1"), &lb),
            ip("2001:db8::1")
        );
        // Unusable header from a trusted proxy falls back to the proxy itself.
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"unknown"), &lb),
            ip("10.0.3.39")
        );
        assert_eq!(client_ip(ip("10.0.3.39"), None, &lb), ip("10.0.3.39"));
        // No proxies configured: never trust the header.
        assert_eq!(
            client_ip(ip("10.0.3.39"), Some(b"203.0.113.7"), &[]),
            ip("10.0.3.39")
        );
        // IPv4-mapped peers match IPv4 networks.
        assert_eq!(
            client_ip(ip("::ffff:10.0.3.39"), Some(b"203.0.113.7"), &lb),
            ip("203.0.113.7")
        );
    }

    #[test]
    fn ipv6_clients_are_throttled_per_64() {
        let a = throttle_key("2001:db8:1:2:aaaa::1".parse().unwrap());
        let b = throttle_key("2001:db8:1:2:bbbb::2".parse().unwrap());
        let c = throttle_key("2001:db8:1:3::1".parse().unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
