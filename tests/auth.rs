//! Authentication, authorization and the protections around the slow
//! password-verification path.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use apps_infra_mobile_build_cache_server::auth::hash_password_with_cost;
use apps_infra_mobile_build_cache_server::config::{Access, Cidr, Config};
use common::*;

const CI: (&str, &str) = ("ci-user", "ci-secret");
const DEV: (&str, &str) = ("developer", "dev-secret");

fn write_users(path: &PathBuf, users: &[(&str, &str, &str)]) {
    let mut text = String::from("# test users\n");
    for (name, role, password) in users {
        // Cheap parameters: these tests are about behaviour, not cost.
        let hash = hash_password_with_cost(password.as_bytes(), 64, 1, 1).unwrap();
        text.push_str(&format!("{name}:{role}:{hash}\n"));
    }
    std::fs::write(path, text).unwrap();
}

async fn start(configure: impl FnOnce(&mut Config)) -> TestServer {
    let dir = temp_dir("auth");
    let users = dir.join("users.txt");
    write_users(&users, &[(CI.0, "readwrite", CI.1), (DEV.0, "read", DEV.1)]);
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    TestServer::start_in(data, |c| {
        c.users_file = Some(users);
        c.anonymous_access = Access::None;
        configure(c);
    })
    .await
}

fn users_path(s: &TestServer) -> PathBuf {
    s.dir.parent().unwrap().join("users.txt")
}

async fn with_auth(s: &TestServer, method: &str, k: &str, user: (&str, &str), body: &[u8]) -> Resp {
    let auth = basic(user.0, user.1);
    once(
        s.cache,
        method,
        &format!("/cache/{k}"),
        &[("Authorization", &auth)],
        body,
    )
    .await
}

#[tokio::test]
async fn roles() {
    let s = start(|_| {}).await;
    let k = key(1);

    // No credentials: 401 with a challenge.
    let r = get(s.cache, &k).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Basic realm=\"cache\""));
    assert_eq!(put(s.cache, &k, b"x").await.status, 401);

    // readwrite: everything.
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(with_auth(&s, "PUT", &k, CI, b"artifact").await.status, 200);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.body, b"artifact");

    // read: GET and HEAD, but PUT is 403 (a client misconfiguration).
    assert_eq!(with_auth(&s, "GET", &k, DEV, b"").await.body, b"artifact");
    assert_eq!(with_auth(&s, "HEAD", &k, DEV, b"").await.status, 200);
    let r = with_auth(&s, "PUT", &k, DEV, b"poison").await;
    assert_eq!(r.status, 403);
    assert!(r.header("www-authenticate").is_none());
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.body, b"artifact");

    // Wrong password, unknown user, malformed header: all 401.
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "wrong"), b"").await.status,
        401
    );
    assert_eq!(
        with_auth(&s, "GET", &k, ("nobody", CI.1), b"").await.status,
        401
    );
    assert_eq!(with_auth(&s, "GET", &k, (CI.0, ""), b"").await.status, 401);
    for header in ["Basic", "Basic !!!", "Bearer abcdef", "Basic Y2ktdXNlcg=="] {
        let r = once(
            s.cache,
            "GET",
            &format!("/cache/{k}"),
            &[("Authorization", header)],
            b"",
        )
        .await;
        assert_eq!(r.status, 401, "{header}");
    }
    // Passwords are compared exactly.
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "ci-secret "), b"")
            .await
            .status,
        401
    );
    assert_eq!(
        with_auth(&s, "GET", &k, ("CI-USER", CI.1), b"")
            .await
            .status,
        401
    );
    assert!(s.metric("auth_failures_total").await >= 9.0);
}

#[tokio::test]
async fn anonymous_levels() {
    let s = start(|c| c.anonymous_access = Access::Read).await;
    let k = key(2);
    assert_eq!(with_auth(&s, "PUT", &k, CI, b"data").await.status, 200);
    // Anonymous may read but not write.
    assert_eq!(get(s.cache, &k).await.status, 200);
    assert_eq!(put(s.cache, &k, b"x").await.status, 401);
    // Wrong credentials are rejected even though anonymous access would have
    // been enough: a misconfigured password must not go unnoticed.
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "wrong"), b"").await.status,
        401
    );
    assert_eq!(
        with_auth(&s, "GET", &k, ("nobody", "x"), b"").await.status,
        401
    );
}

#[tokio::test]
async fn correct_password_is_verified_once_then_served_from_the_cache() {
    let s = start(|_| {}).await;
    let k = key(3);
    assert_eq!(with_auth(&s, "PUT", &k, CI, b"data").await.status, 200);
    assert_eq!(s.metric("auth_verifications_total").await, 1.0);
    for _ in 0..20 {
        assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 200);
    }
    assert_eq!(s.metric("auth_verifications_total").await, 1.0);
    // A second user is verified separately.
    assert_eq!(with_auth(&s, "GET", &k, DEV, b"").await.status, 200);
    assert_eq!(with_auth(&s, "GET", &k, DEV, b"").await.status, 200);
    assert_eq!(s.metric("auth_verifications_total").await, 2.0);
    // Failures are never cached.
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "wrong"), b"").await.status,
        401
    );
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "wrong"), b"").await.status,
        401
    );
    assert_eq!(s.metric("auth_verifications_total").await, 4.0);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 200);
}

#[tokio::test]
async fn many_cold_requests_at_once_share_one_verification() {
    // A CI build opens several connections at once with credentials the server
    // has not seen yet. They must queue for the slot, not fail.
    let s = start(|c| c.auth_concurrency = 1).await;
    let mut tasks = Vec::new();
    for i in 0..24u64 {
        let addr = s.cache;
        tasks.push(tokio::spawn(async move {
            let auth = basic(CI.0, CI.1);
            once(
                addr,
                "GET",
                &format!("/cache/{}", key(i)),
                &[("Authorization", &auth)],
                b"",
            )
            .await
            .status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 404);
    }
    assert_eq!(s.metric("auth_verifications_total").await, 1.0);
    assert_eq!(s.metric("auth_busy_total").await, 0.0);
}

#[tokio::test]
async fn verification_cache_expires() {
    let s = start(|c| c.auth_cache_ttl = Duration::from_millis(300)).await;
    let k = key(4);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(s.metric("auth_verifications_total").await, 1.0);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(s.metric("auth_verifications_total").await, 2.0);
}

#[tokio::test]
async fn reloading_the_users_file_invalidates_remembered_credentials() {
    let s = start(|_| {}).await;
    let k = key(5);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(with_auth(&s, "GET", &k, DEV, b"").await.status, 404);

    // ci-user gets a new password, developer is removed, a new user appears.
    write_users(
        &users_path(&s),
        &[
            (CI.0, "readwrite", "rotated"),
            ("new-ci", "readwrite", "fresh"),
        ],
    );
    // Until the reload, nothing changes.
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
    assert_eq!(s.app().auth.reload(), Ok(2));

    assert_eq!(
        with_auth(&s, "GET", &k, CI, b"").await.status,
        401,
        "old password"
    );
    assert_eq!(
        with_auth(&s, "GET", &k, DEV, b"").await.status,
        401,
        "removed user"
    );
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "rotated"), b"")
            .await
            .status,
        404
    );
    assert_eq!(
        with_auth(&s, "PUT", &k, ("new-ci", "fresh"), b"x")
            .await
            .status,
        200
    );

    // A broken file is refused and the current users stay in effect.
    std::fs::write(users_path(&s), "ci-user:readwrite:not-a-hash\n").unwrap();
    assert!(s.app().auth.reload().is_err());
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "rotated"), b"")
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn unknown_user_costs_the_same_as_a_wrong_password() {
    // Both run one argon2 verification with the same parameters, so response
    // time does not reveal which usernames exist.
    let s = start(|_| {}).await;
    let k = key(6);
    assert_eq!(
        with_auth(&s, "GET", &k, (CI.0, "wrong"), b"").await.status,
        401
    );
    assert_eq!(s.metric("auth_verifications_total").await, 1.0);
    assert_eq!(
        with_auth(&s, "GET", &k, ("no-such-user", "wrong"), b"")
            .await
            .status,
        401
    );
    assert_eq!(s.metric("auth_verifications_total").await, 2.0);
}

#[tokio::test]
async fn unknown_user_and_wrong_password_take_comparable_time() {
    // Production-strength hashes, so the timing is dominated by argon2.
    let dir = temp_dir("auth-timing");
    let users = dir.join("users.txt");
    let hash = apps_infra_mobile_build_cache_server::auth::hash_password(b"real-password").unwrap();
    std::fs::write(&users, format!("ci-user:readwrite:{hash}\n")).unwrap();
    let s = TestServer::start_in(dir.join("data"), |c| {
        c.users_file = Some(users);
        c.anonymous_access = Access::None;
        c.auth_failure_limit = 1000;
    })
    .await;
    let k = key(7);
    let mut wrong = Vec::new();
    let mut unknown = Vec::new();
    for i in 0..5 {
        let t = std::time::Instant::now();
        assert_eq!(
            with_auth(&s, "GET", &k, ("ci-user", &format!("wrong{i}")), b"")
                .await
                .status,
            401
        );
        wrong.push(t.elapsed());
        let t = std::time::Instant::now();
        assert_eq!(
            with_auth(&s, "GET", &k, (&format!("ghost{i}"), "wrong"), b"")
                .await
                .status,
            401
        );
        unknown.push(t.elapsed());
    }
    wrong.sort();
    unknown.sort();
    let (w, u) = (wrong[2].as_secs_f64(), unknown[2].as_secs_f64());
    assert!(w > 0.002, "argon2 should dominate, got {w}s");
    assert!(
        u / w > 0.5 && u / w < 2.0,
        "median wrong-password {w}s vs unknown-user {u}s"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn auth_failure_with_expect_continue_is_answered_before_the_upload() {
    let s = start(|_| {}).await;
    let k = key(8);
    for (auth, status) in [
        (None, 401),
        (Some(basic(CI.0, "wrong")), 401),
        (Some(basic(DEV.0, DEV.1)), 403),
    ] {
        let mut headers = vec![("Expect", "100-continue")];
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let mut c = Conn::open(s.cache).await;
        c.send(&req_head(
            "PUT",
            &format!("/cache/{k}"),
            &headers,
            Some(50 * MIB),
        ))
        .await;
        let r = c.response().await;
        assert_eq!(r.status, status, "final status, never 100 Continue");
        assert!(r.closes());
    }
    assert_eq!(s.metric("bytes_received_total").await, 0.0);

    // A small rejected upload without Expect is drained and the connection lives on.
    let mut c = Conn::open(s.cache).await;
    let dev = basic(DEV.0, DEV.1);
    let r = c
        .request(
            "PUT",
            &format!("/cache/{k}"),
            &[("Authorization", &dev)],
            &[1u8; 2000],
        )
        .await;
    assert_eq!(r.status, 403);
    assert!(!r.closes());
    assert_eq!(
        c.request(
            "GET",
            &format!("/cache/{k}"),
            &[("Authorization", &dev)],
            b""
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn password_guessing_is_throttled_and_cached_clients_are_unaffected() {
    let s = start(|c| {
        c.auth_failure_limit = 5;
        c.auth_failure_window = Duration::from_secs(60);
    })
    .await;
    let k = key(9);
    // A legitimate client whose credentials are already remembered.
    assert_eq!(with_auth(&s, "PUT", &k, CI, b"data").await.status, 200);

    let mut statuses = Vec::new();
    for i in 0..40 {
        let r = with_auth(&s, "GET", &k, (CI.0, &format!("guess-{i}")), b"").await;
        if r.status == 429 {
            let retry: u64 = r
                .header("retry-after")
                .expect("Retry-After")
                .parse()
                .unwrap();
            assert!((1..=60).contains(&retry));
        }
        statuses.push(r.status);
        // The legitimate client keeps working throughout the flood.
        if i % 10 == 0 {
            assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 200);
        }
    }
    assert_eq!(&statuses[..5], [401; 5]);
    assert!(statuses[5..].iter().all(|s| *s == 429), "{statuses:?}");
    // argon2 ran for the first five guesses only (plus the legitimate login).
    assert_eq!(s.metric("auth_verifications_total").await, 6.0);
    assert_eq!(s.metric("auth_throttled_total").await, 35.0);
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 200);
}

#[tokio::test]
async fn throttle_window_expires() {
    let s = start(|c| {
        c.auth_failure_limit = 2;
        c.auth_failure_window = Duration::from_millis(400);
    })
    .await;
    let k = key(10);
    assert_eq!(with_auth(&s, "GET", &k, (CI.0, "a"), b"").await.status, 401);
    assert_eq!(with_auth(&s, "GET", &k, (CI.0, "b"), b"").await.status, 401);
    assert_eq!(with_auth(&s, "GET", &k, (CI.0, "c"), b"").await.status, 429);
    // While throttled, even the right password is not evaluated for a client
    // that has not authenticated before...
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 429);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // ...and afterwards it works again: failures never lock an account out.
    assert_eq!(with_auth(&s, "GET", &k, CI, b"").await.status, 404);
}

#[tokio::test]
async fn forged_forwarded_for_does_not_evade_the_throttle_on_direct_connections() {
    // No trusted proxies: the header is ignored and the TCP peer is the client.
    let s = start(|c| c.auth_failure_limit = 3).await;
    let k = key(11);
    let mut statuses = Vec::new();
    for i in 0..8 {
        let auth = basic(CI.0, "guess");
        let forged = format!("198.51.100.{i}");
        let r = once(
            s.cache,
            "GET",
            &format!("/cache/{k}"),
            &[("Authorization", &auth), ("X-Forwarded-For", &forged)],
            b"",
        )
        .await;
        statuses.push(r.status);
    }
    assert_eq!(statuses, [401, 401, 401, 429, 429, 429, 429, 429]);
}

#[tokio::test]
async fn forged_forwarded_for_does_not_evade_the_throttle_through_a_trusted_proxy() {
    // The test client plays the load balancer: its address is trusted, and the
    // last X-Forwarded-For entry is the address the "load balancer" saw.
    let s = start(|c| {
        c.auth_failure_limit = 3;
        c.trusted_proxies = vec![Cidr::parse("127.0.0.0/8").unwrap()];
    })
    .await;
    let k = key(12);
    let attempt = |forwarded: String, password: &'static str| {
        let addr = s.cache;
        let k = k.clone();
        async move {
            let auth = basic(CI.0, password);
            once(
                addr,
                "GET",
                &format!("/cache/{k}"),
                &[("Authorization", &auth), ("X-Forwarded-For", &forwarded)],
                b"",
            )
            .await
            .status
        }
    };
    // The attacker (203.0.113.9) prepends a different forged address each time.
    let mut statuses = Vec::new();
    for i in 0..6 {
        statuses.push(attempt(format!("192.0.2.{i}, 203.0.113.9"), "guess").await);
    }
    assert_eq!(statuses, [401, 401, 401, 429, 429, 429]);
    // Putting the victim's address first does not get the victim throttled...
    assert_eq!(
        attempt("203.0.113.9, 198.51.100.77".into(), "guess").await,
        401
    );
    // ...and a different real client is unaffected by the attacker's failures.
    assert_eq!(attempt("198.51.100.50".into(), CI.1).await, 404);
    // Several header lines: the last line's last entry counts.
    let auth = basic(CI.0, "guess");
    let r = once(
        s.cache,
        "GET",
        &format!("/cache/{k}"),
        &[
            ("Authorization", &auth),
            ("X-Forwarded-For", "198.51.100.60"),
            ("X-Forwarded-For", "203.0.113.9"),
        ],
        b"",
    )
    .await;
    assert_eq!(r.status, 429);
}

#[tokio::test]
async fn verification_slots_bound_cpu_and_overflow_gets_503() {
    // One slot, a short wait, and a hash expensive enough to hold the slot.
    let dir = temp_dir("auth-busy");
    let users = dir.join("users.txt");
    let hash = hash_password_with_cost(b"pw", 128 * 1024, 4, 1).unwrap();
    std::fs::write(&users, format!("ci-user:readwrite:{hash}\n")).unwrap();
    let s = TestServer::start_in(dir.join("data"), |c| {
        c.users_file = Some(users);
        c.anonymous_access = Access::None;
        c.auth_concurrency = 1;
        c.auth_wait_timeout = Duration::from_millis(50);
        c.auth_failure_limit = 1000;
    })
    .await;
    // A legitimate client that is already remembered.
    assert_eq!(
        with_auth(&s, "GET", &key(1), ("ci-user", "pw"), b"")
            .await
            .status,
        404
    );

    let mut tasks = Vec::new();
    for i in 0..6 {
        let addr = s.cache;
        tasks.push(tokio::spawn(async move {
            let auth = basic("ci-user", &format!("guess-{i}"));
            once(
                addr,
                "GET",
                &format!("/cache/{}", key(1)),
                &[("Authorization", &auth)],
                b"",
            )
            .await
        }));
    }
    // Cached credentials skip the queue entirely while the slot is busy.
    let t = std::time::Instant::now();
    assert_eq!(
        with_auth(&s, "GET", &key(1), ("ci-user", "pw"), b"")
            .await
            .status,
        404
    );
    let cached_latency = t.elapsed();

    let mut statuses = Vec::new();
    for t in tasks {
        let r = t.await.unwrap();
        if r.status == 503 {
            assert_eq!(r.header("retry-after"), Some("1"));
        }
        statuses.push(r.status);
    }
    assert!(statuses.contains(&401), "{statuses:?}");
    assert!(statuses.contains(&503), "{statuses:?}");
    assert!(
        statuses.iter().all(|s| *s == 401 || *s == 503),
        "{statuses:?}"
    );
    assert!(s.metric("auth_busy_total").await >= 1.0);
    assert!(
        cached_latency < Duration::from_millis(50),
        "cached client waited {cached_latency:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
