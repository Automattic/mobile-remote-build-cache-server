//! Configuration from environment variables, parsed by hand.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Access level of the anonymous (no credentials) client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    None,
    Read,
    ReadWrite,
}

/// Free space to keep on the volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FreeSpace {
    Bytes(u64),
    Percent(f64),
}

impl FreeSpace {
    /// Resolves to bytes given the volume's total size.
    pub fn resolve(&self, disk_total: u64) -> u64 {
        match *self {
            FreeSpace::Bytes(b) => b,
            FreeSpace::Percent(p) => (disk_total as f64 * p / 100.0) as u64,
        }
    }
}

/// An IPv4 or IPv6 network in CIDR notation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("invalid network address {s:?}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("invalid prefix length in {s:?}"))?,
            None => max,
        };
        Ok(Cidr { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical_ip(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = mask_u128(self.prefix, 32) as u32;
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = mask_u128(self.prefix, 128);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

fn mask_u128(prefix: u8, bits: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        let full: u128 = if bits == 32 {
            u32::MAX as u128
        } else {
            u128::MAX
        };
        (full << (bits - prefix)) & full
    }
}

/// Maps IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) to plain IPv4.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub admin_listen: Option<SocketAddr>,
    pub data_dir: PathBuf,
    pub max_size: Option<u64>,
    pub max_age: Option<Duration>,
    pub min_free_space: Option<FreeSpace>,
    pub max_artifact_size: u64,
    /// Hard ceiling for draining a rejected upload before answering 413.
    pub discard_ceiling: u64,
    pub eviction_interval: Duration,
    /// Size eviction stops at this fraction of `max_size`.
    pub eviction_low_watermark: f64,
    pub fsync: bool,
    /// Minimum time between persisting an entry's last-use time to disk.
    pub touch_interval: Duration,
    pub users_file: Option<PathBuf>,
    pub anonymous_access: Access,
    pub auth_concurrency: usize,
    pub auth_wait_timeout: Duration,
    pub auth_failure_limit: u32,
    pub auth_failure_window: Duration,
    pub auth_cache_ttl: Duration,
    pub trusted_proxies: Vec<Cidr>,
    pub max_connections: usize,
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub access_log: bool,
}

/// Every variable the server reads. Anything else starting with `CACHE_` is a
/// startup error, so a typo cannot silently leave a limit unset.
pub const KNOWN_VARS: &[&str] = &[
    "CACHE_LISTEN",
    "CACHE_ADMIN_LISTEN",
    "CACHE_DATA_DIR",
    "CACHE_MAX_SIZE",
    "CACHE_MAX_AGE",
    "CACHE_MIN_FREE_SPACE",
    "CACHE_MAX_ARTIFACT_SIZE",
    "CACHE_DISCARD_CEILING",
    "CACHE_EVICTION_INTERVAL",
    "CACHE_EVICTION_LOW_WATERMARK",
    "CACHE_FSYNC",
    "CACHE_TOUCH_INTERVAL",
    "CACHE_USERS_FILE",
    "CACHE_ANONYMOUS_ACCESS",
    "CACHE_AUTH_CONCURRENCY",
    "CACHE_AUTH_WAIT_TIMEOUT",
    "CACHE_AUTH_FAILURE_LIMIT",
    "CACHE_AUTH_CACHE_TTL",
    "CACHE_TRUSTED_PROXIES",
    "CACHE_MAX_CONNECTIONS",
    "CACHE_HEADER_TIMEOUT",
    "CACHE_IDLE_TIMEOUT",
    "CACHE_SHUTDOWN_TIMEOUT",
    "CACHE_ACCESS_LOG",
];

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 5071)),
            admin_listen: Some(SocketAddr::from(([0, 0, 0, 0], 9090))),
            data_dir: PathBuf::from("/data"),
            max_size: None,
            max_age: None,
            min_free_space: Some(FreeSpace::Bytes(GIB)),
            max_artifact_size: 100 * MIB,
            discard_ceiling: 400 * MIB,
            eviction_interval: Duration::from_secs(60),
            eviction_low_watermark: 0.98,
            fsync: true,
            touch_interval: Duration::from_secs(3600),
            users_file: None,
            anonymous_access: Access::None,
            auth_concurrency: 2,
            auth_wait_timeout: Duration::from_secs(5),
            auth_failure_limit: 10,
            auth_failure_window: Duration::from_secs(60),
            auth_cache_ttl: Duration::from_secs(3600),
            trusted_proxies: Vec::new(),
            max_connections: 1024,
            header_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(75),
            shutdown_timeout: Duration::from_secs(30),
            access_log: false,
        }
    }
}

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Config, String> {
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && name.starts_with("CACHE_")
                && !KNOWN_VARS.contains(&name)
            {
                return Err(format!("unknown configuration variable {name}"));
            }
        }
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    /// Builds a configuration from any name-to-value lookup. A variable that
    /// is unset or empty takes its default, except where noted.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let raw = |name: &str| get(name).map(|v| v.trim().to_string());
        let var = |name: &str| raw(name).filter(|v| !v.is_empty());
        fn field<T>(name: &str, r: Result<T, String>) -> Result<T, String> {
            r.map_err(|e| format!("{name}: {e}"))
        }
        let mut c = Config::default();

        if let Some(v) = var("CACHE_LISTEN") {
            c.listen = field("CACHE_LISTEN", parse_addr(&v))?;
        }
        // An explicitly empty value disables the admin listener.
        match raw("CACHE_ADMIN_LISTEN") {
            Some(v) if v.is_empty() => c.admin_listen = None,
            Some(v) => c.admin_listen = Some(field("CACHE_ADMIN_LISTEN", parse_addr(&v))?),
            None => {}
        }
        if let Some(v) = var("CACHE_DATA_DIR") {
            c.data_dir = PathBuf::from(v);
        }
        if let Some(v) = var("CACHE_MAX_SIZE") {
            c.max_size = field("CACHE_MAX_SIZE", parse_optional(&v, parse_size))?;
            if c.max_size == Some(0) {
                return Err("CACHE_MAX_SIZE: must be greater than zero".into());
            }
        }
        if let Some(v) = var("CACHE_MAX_AGE") {
            c.max_age = field("CACHE_MAX_AGE", parse_optional(&v, parse_duration))?;
            if c.max_age == Some(Duration::ZERO) {
                return Err("CACHE_MAX_AGE: must be greater than zero".into());
            }
        }
        if let Some(v) = var("CACHE_MIN_FREE_SPACE") {
            c.min_free_space = field("CACHE_MIN_FREE_SPACE", parse_optional(&v, parse_free_space))?
                .filter(|f| !matches!(f, FreeSpace::Bytes(0)) && *f != FreeSpace::Percent(0.0));
        }
        if let Some(v) = var("CACHE_MAX_ARTIFACT_SIZE") {
            c.max_artifact_size = field("CACHE_MAX_ARTIFACT_SIZE", parse_size(&v))?;
            if c.max_artifact_size == 0 {
                return Err("CACHE_MAX_ARTIFACT_SIZE: must be greater than zero".into());
            }
        }
        c.discard_ceiling = match var("CACHE_DISCARD_CEILING") {
            Some(v) => field("CACHE_DISCARD_CEILING", parse_size(&v))?,
            None => c.max_artifact_size.saturating_mul(4),
        };
        if c.discard_ceiling < c.max_artifact_size {
            return Err("CACHE_DISCARD_CEILING: must not be below CACHE_MAX_ARTIFACT_SIZE".into());
        }
        if let Some(v) = var("CACHE_EVICTION_INTERVAL") {
            c.eviction_interval = field("CACHE_EVICTION_INTERVAL", parse_positive_duration(&v))?;
        }
        if let Some(v) = var("CACHE_EVICTION_LOW_WATERMARK") {
            c.eviction_low_watermark =
                field("CACHE_EVICTION_LOW_WATERMARK", parse_percent(&v))? / 100.0;
            if !(0.01..=1.0).contains(&c.eviction_low_watermark) {
                return Err("CACHE_EVICTION_LOW_WATERMARK: must be between 1% and 100%".into());
            }
        }
        if let Some(v) = var("CACHE_FSYNC") {
            c.fsync = field("CACHE_FSYNC", parse_bool(&v))?;
        }
        if let Some(v) = var("CACHE_TOUCH_INTERVAL") {
            c.touch_interval = field("CACHE_TOUCH_INTERVAL", parse_duration(&v))?;
        }
        if let Some(v) = var("CACHE_USERS_FILE") {
            c.users_file = Some(PathBuf::from(v));
        }
        if let Some(v) = var("CACHE_ANONYMOUS_ACCESS") {
            c.anonymous_access = match v.as_str() {
                "none" => Access::None,
                "read" => Access::Read,
                "readwrite" => Access::ReadWrite,
                _ => {
                    return Err(format!(
                        "CACHE_ANONYMOUS_ACCESS: expected none, read, or readwrite, got {v:?}"
                    ));
                }
            };
        }
        if let Some(v) = var("CACHE_AUTH_CONCURRENCY") {
            c.auth_concurrency = field("CACHE_AUTH_CONCURRENCY", parse_count(&v))?;
        }
        if let Some(v) = var("CACHE_AUTH_WAIT_TIMEOUT") {
            c.auth_wait_timeout = field("CACHE_AUTH_WAIT_TIMEOUT", parse_duration(&v))?;
        }
        if let Some(v) = var("CACHE_AUTH_FAILURE_LIMIT") {
            (c.auth_failure_limit, c.auth_failure_window) =
                field("CACHE_AUTH_FAILURE_LIMIT", parse_rate(&v))?;
        }
        if let Some(v) = var("CACHE_AUTH_CACHE_TTL") {
            c.auth_cache_ttl = field("CACHE_AUTH_CACHE_TTL", parse_positive_duration(&v))?;
        }
        if let Some(v) = var("CACHE_TRUSTED_PROXIES") {
            c.trusted_proxies = v
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(Cidr::parse)
                .collect::<Result<_, _>>()
                .map_err(|e| format!("CACHE_TRUSTED_PROXIES: {e}"))?;
        }
        if let Some(v) = var("CACHE_MAX_CONNECTIONS") {
            c.max_connections = field("CACHE_MAX_CONNECTIONS", parse_count(&v))?;
        }
        if let Some(v) = var("CACHE_HEADER_TIMEOUT") {
            c.header_timeout = field("CACHE_HEADER_TIMEOUT", parse_positive_duration(&v))?;
        }
        if let Some(v) = var("CACHE_IDLE_TIMEOUT") {
            c.idle_timeout = field("CACHE_IDLE_TIMEOUT", parse_positive_duration(&v))?;
        }
        if let Some(v) = var("CACHE_SHUTDOWN_TIMEOUT") {
            c.shutdown_timeout = field("CACHE_SHUTDOWN_TIMEOUT", parse_duration(&v))?;
        }
        if let Some(v) = var("CACHE_ACCESS_LOG") {
            c.access_log = field("CACHE_ACCESS_LOG", parse_bool(&v))?;
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_size.is_none() && self.max_age.is_none() && self.min_free_space.is_none() {
            return Err(
                "no eviction limit configured: set at least one of CACHE_MAX_SIZE, \
                 CACHE_MAX_AGE, CACHE_MIN_FREE_SPACE (an unbounded cache fills the disk)"
                    .into(),
            );
        }
        if self.max_connections > (u32::MAX >> 3) as usize {
            return Err("CACHE_MAX_CONNECTIONS: too large".into());
        }
        Ok(())
    }
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.parse()
        .map_err(|_| format!("expected an address like 0.0.0.0:5071, got {s:?}"))
}

fn parse_optional<T>(
    s: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        Ok(None)
    } else {
        parse(s).map(Some)
    }
}

fn split_number(s: &str) -> Result<(u64, &str), String> {
    let s = s.trim();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(format!("expected a number, got {s:?}"));
    }
    let n = s[..digits]
        .parse::<u64>()
        .map_err(|_| format!("number out of range in {s:?}"))?;
    Ok((n, s[digits..].trim()))
}

/// Parses sizes such as `200GiB`, `100MiB`, `512k`, `1GB`, or plain bytes.
/// `K`/`M`/`G`/`T` and the `iB` forms are powers of 1024; `KB`/`MB`/`GB`/`TB`
/// are powers of 1000.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let (n, unit) = split_number(s)?;
    let mult: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => {
            return Err(format!(
                "unknown size unit in {s:?} (use B, KiB, MiB, GiB, TiB)"
            ));
        }
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("size out of range: {s:?}"))
}

/// Parses durations such as `500ms`, `60s`, `5m`, `2h`, `30d`. A unit is
/// required so that a bare number is never misread.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let (n, unit) = split_number(s)?;
    let ms: u64 = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => {
            return Err(format!(
                "expected a duration with a unit (ms, s, m, h, d), got {s:?}"
            ));
        }
    };
    // Capped so that adding a configured duration to a point in time can
    // never overflow.
    const MAX_MS: u64 = 100 * 365 * 86_400_000;
    n.checked_mul(ms)
        .filter(|ms| *ms <= MAX_MS)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration out of range: {s:?}"))
}

fn parse_positive_duration(s: &str) -> Result<Duration, String> {
    let d = parse_duration(s)?;
    if d.is_zero() {
        return Err("must be greater than zero".into());
    }
    Ok(d)
}

fn parse_percent(s: &str) -> Result<f64, String> {
    let num = s.trim().strip_suffix('%').unwrap_or(s).trim();
    let ok_chars = !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    match num.parse::<f64>() {
        Ok(p) if ok_chars && (0.0..=100.0).contains(&p) => Ok(p),
        _ => Err(format!(
            "expected a percentage between 0% and 100%, got {s:?}"
        )),
    }
}

/// Parses `CACHE_MIN_FREE_SPACE`: a size, or a percentage of the volume.
pub fn parse_free_space(s: &str) -> Result<FreeSpace, String> {
    if s.trim().ends_with('%') {
        let p = parse_percent(s)?;
        if p >= 100.0 {
            return Err("must be below 100%".into());
        }
        Ok(FreeSpace::Percent(p))
    } else {
        parse_size(s).map(FreeSpace::Bytes)
    }
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!("expected true or false, got {s:?}")),
    }
}

fn parse_count(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("expected a positive integer, got {s:?}")),
    }
}

/// Parses `CACHE_AUTH_FAILURE_LIMIT`, for example `10/1m`.
pub fn parse_rate(s: &str) -> Result<(u32, Duration), String> {
    let (count, window) = s
        .split_once('/')
        .ok_or_else(|| format!("expected COUNT/WINDOW such as 10/1m, got {s:?}"))?;
    let count = count
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|c| *c > 0)
        .ok_or_else(|| format!("expected a positive count in {s:?}"))?;
    Ok((count, parse_positive_duration(window)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("1KiB"), Ok(1024));
        assert_eq!(parse_size("100MiB"), Ok(100 << 20));
        assert_eq!(parse_size("200GiB"), Ok(200 << 30));
        assert_eq!(parse_size("200 gib"), Ok(200 << 30));
        assert_eq!(parse_size("2T"), Ok(2 << 40));
        assert_eq!(parse_size("1GB"), Ok(1_000_000_000));
        assert_eq!(parse_size("5kb"), Ok(5_000));
        assert!(parse_size("").is_err());
        assert!(parse_size("GiB").is_err());
        assert!(parse_size("-1").is_err());
        assert!(parse_size("1.5GiB").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("99999999999999999999").is_err());
        assert!(parse_size("99999999999TiB").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("60s"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_duration("1m"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("30d"), Ok(Duration::from_secs(30 * 86400)));
        assert!(parse_duration("60").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("36500d").is_ok());
        assert!(parse_duration("36501d").is_err());
        assert!(parse_duration("99999999999999999d").is_err());
    }

    #[test]
    fn free_space_and_rate() {
        assert_eq!(parse_free_space("8GiB"), Ok(FreeSpace::Bytes(8 << 30)));
        assert_eq!(parse_free_space("10%"), Ok(FreeSpace::Percent(10.0)));
        assert_eq!(parse_free_space("2.5%"), Ok(FreeSpace::Percent(2.5)));
        assert!(parse_free_space("100%").is_err());
        assert!(parse_free_space("-1%").is_err());
        assert!(parse_free_space("abc%").is_err());
        assert_eq!(FreeSpace::Percent(10.0).resolve(1000), 100);
        assert_eq!(FreeSpace::Bytes(7).resolve(1000), 7);
        assert_eq!(parse_rate("10/1m"), Ok((10, Duration::from_secs(60))));
        assert!(parse_rate("10").is_err());
        assert!(parse_rate("0/1m").is_err());
        assert!(parse_rate("10/0s").is_err());
    }

    #[test]
    fn cidrs() {
        let c = Cidr::parse("10.0.3.0/24").unwrap();
        assert!(c.contains("10.0.3.39".parse().unwrap()));
        assert!(!c.contains("10.0.6.231".parse().unwrap()));
        assert!(c.contains("::ffff:10.0.3.1".parse().unwrap()));
        assert!(!c.contains("::1".parse().unwrap()));
        let all = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
        let host = Cidr::parse("192.0.2.7").unwrap();
        assert!(host.contains("192.0.2.7".parse().unwrap()));
        assert!(!host.contains("192.0.2.8".parse().unwrap()));
        let v6 = Cidr::parse("fd00::/8").unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(!v6.contains("fe80::1".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nonsense").is_err());
    }

    #[test]
    fn defaults_and_overrides() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.listen.port(), 5071);
        assert_eq!(c.admin_listen.unwrap().port(), 9090);
        assert_eq!(c.min_free_space, Some(FreeSpace::Bytes(1 << 30)));
        assert_eq!(c.max_artifact_size, 100 << 20);
        assert_eq!(c.discard_ceiling, 400 << 20);
        assert_eq!(c.anonymous_access, Access::None);

        let c = cfg(&[
            ("CACHE_MAX_SIZE", "90GiB"),
            ("CACHE_MIN_FREE_SPACE", "8GiB"),
            ("CACHE_MAX_ARTIFACT_SIZE", "1000MiB"),
            ("CACHE_ADMIN_LISTEN", ""),
            ("CACHE_TRUSTED_PROXIES", "10.0.3.0/24, 10.0.6.0/24"),
            ("CACHE_AUTH_FAILURE_LIMIT", "5/30s"),
            ("CACHE_ANONYMOUS_ACCESS", "read"),
            ("CACHE_FSYNC", "false"),
        ])
        .unwrap();
        assert_eq!(c.max_size, Some(90 << 30));
        assert_eq!(c.discard_ceiling, 4000 << 20);
        assert_eq!(c.admin_listen, None);
        assert_eq!(c.trusted_proxies.len(), 2);
        assert_eq!(c.auth_failure_limit, 5);
        assert_eq!(c.auth_failure_window, Duration::from_secs(30));
        assert_eq!(c.anonymous_access, Access::Read);
        assert!(!c.fsync);
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(cfg(&[("CACHE_LISTEN", "nope")]).is_err());
        assert!(cfg(&[("CACHE_MAX_SIZE", "lots")]).is_err());
        assert!(cfg(&[("CACHE_MAX_SIZE", "0")]).is_err());
        assert!(cfg(&[("CACHE_FSYNC", "maybe")]).is_err());
        assert!(cfg(&[("CACHE_ANONYMOUS_ACCESS", "admin")]).is_err());
        assert!(cfg(&[("CACHE_MAX_CONNECTIONS", "0")]).is_err());
        assert!(cfg(&[("CACHE_IDLE_TIMEOUT", "75")]).is_err());
        assert!(cfg(&[("CACHE_DISCARD_CEILING", "1MiB")]).is_err());
        let err = cfg(&[("CACHE_MAX_AGE", "soon")]).unwrap_err();
        assert!(err.starts_with("CACHE_MAX_AGE:"), "{err}");
    }

    #[test]
    fn refuses_to_start_without_any_limit() {
        assert!(cfg(&[("CACHE_MIN_FREE_SPACE", "none")]).is_err());
        assert!(cfg(&[("CACHE_MIN_FREE_SPACE", "0")]).is_err());
        assert!(cfg(&[("CACHE_MIN_FREE_SPACE", "none"), ("CACHE_MAX_AGE", "30d")]).is_ok());
        assert!(cfg(&[("CACHE_MIN_FREE_SPACE", "none"), ("CACHE_MAX_SIZE", "1GiB")]).is_ok());
    }
}
