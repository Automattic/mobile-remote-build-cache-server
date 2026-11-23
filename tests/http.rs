//! Protocol behaviour of the cache and admin listeners.

mod common;

use std::time::Duration;

use common::*;

#[tokio::test]
async fn put_then_get_round_trip() {
    let s = TestServer::start(|_| {}).await;
    let k = key(1);
    let data = pattern(300_000, 1);

    let miss = get(s.cache, &k).await;
    assert_eq!(miss.status, 404);
    assert_eq!(miss.header("x-max-entry-size-bytes"), Some("104857600"));

    let stored = put(s.cache, &k, &data).await;
    assert_eq!(stored.status, 200);
    assert_eq!(stored.header("x-max-entry-size-bytes"), Some("104857600"));

    let hit = get(s.cache, &k).await;
    assert_eq!(hit.status, 200);
    assert_eq!(hit.content_length(), Some(data.len() as u64));
    assert_eq!(hit.header("x-max-entry-size-bytes"), Some("104857600"));
    assert!(
        hit.header("content-encoding").is_none(),
        "entries are never compressed"
    );
    assert_eq!(hit.body, data);

    // On disk: the old node's layout, the body byte for byte.
    assert_eq!(std::fs::read(entry_path(&s.dir, &k)).unwrap(), data);
    assert_eq!(tmp_files(&s.dir), 0);

    assert_eq!(s.metric("hits_total").await, 1.0);
    assert_eq!(s.metric("misses_total").await, 1.0);
    assert_eq!(s.metric("entries").await, 1.0);
    assert_eq!(s.metric("size_bytes").await, data.len() as f64);
    assert_eq!(s.metric("bytes_received_total").await, data.len() as f64);
    assert_eq!(s.metric("bytes_sent_total").await, data.len() as f64);
    assert_eq!(
        s.metric("requests_total{method=\"PUT\",status=\"200\"}")
            .await,
        1.0
    );
    assert_eq!(
        s.metric("requests_total{method=\"GET\",status=\"404\"}")
            .await,
        1.0
    );
}

#[tokio::test]
async fn overwrite_replaces_the_entry() {
    let s = TestServer::start(|_| {}).await;
    let k = key(2);
    assert_eq!(put(s.cache, &k, b"first version").await.status, 200);
    assert_eq!(put(s.cache, &k, b"second").await.status, 200);
    assert_eq!(get(s.cache, &k).await.body, b"second");
    assert_eq!(s.app().index.len(), 1);
    assert_eq!(s.app().index.total_bytes(), 6);
}

#[tokio::test]
async fn empty_and_large_entries() {
    let s = TestServer::start(|_| {}).await;
    assert_eq!(put(s.cache, &key(3), b"").await.status, 200);
    let empty = get(s.cache, &key(3)).await;
    assert_eq!((empty.status, empty.content_length()), (200, Some(0)));

    // Several read chunks and several write buffers.
    let big = pattern(3 * MIB as usize + 12_345, 7);
    assert_eq!(put(s.cache, &key(4), &big).await.status, 200);
    assert!(
        get(s.cache, &key(4)).await.body == big,
        "large body differs"
    );
}

#[tokio::test]
async fn head_reports_size_without_a_body() {
    let s = TestServer::start(|_| {}).await;
    let k = key(5);
    put(s.cache, &k, &pattern(5000, 2)).await;

    let mut c = Conn::open(s.cache).await;
    let head = c.request("HEAD", &format!("/cache/{k}"), &[], b"").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.content_length(), Some(5000));
    // The next response follows immediately: no body was sent for HEAD.
    let next = c.request("GET", "/ping", &[], b"").await;
    assert_eq!((next.status, next.body.as_slice()), (200, &b"SUCCESS"[..]));

    let missing = c
        .request("HEAD", &format!("/cache/{}", key(6)), &[], b"")
        .await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn keep_alive_serves_many_requests_on_one_connection() {
    let s = TestServer::start(|_| {}).await;
    let mut c = Conn::open(s.cache).await;
    for i in 0..50u64 {
        let k = key(100 + i);
        let path = format!("/cache/{k}");
        assert_eq!(c.request("GET", &path, &[], b"").await.status, 404);
        let body = pattern(1000 + i as usize, i as u8);
        assert_eq!(c.request("PUT", &path, &[], &body).await.status, 200);
        assert_eq!(c.request("GET", &path, &[], b"").await.body, body);
    }
}

#[tokio::test]
async fn gradle_shaped_requests() {
    // Headers exactly as Gradle 8.13 / 9.9.1 send them.
    let s = TestServer::start(|_| {}).await;
    let k = "5f2c8a0e9d3b4c1f7a6e5d4c3b2a1908";
    let common = [
        ("X-Gradle-Version", "9.9.1"),
        (
            "User-Agent",
            "Gradle/9.9.1 (Mac OS X;15.0;aarch64) (Amazon.com Inc.;21.0.6;21.0.6+7-LTS)",
        ),
        ("Accept-Encoding", "gzip,deflate"),
    ];
    let mut c = Conn::open(s.cache).await;
    let mut h = common.to_vec();
    h.push((
        "Accept",
        "application/vnd.gradle.build-cache-artifact.v2, */*",
    ));
    assert_eq!(
        c.request("GET", &format!("/cache/{k}"), &h, b"")
            .await
            .status,
        404
    );

    let gz = [&[0x1f, 0x8b, 8, 0][..], &pattern(4000, 3)].concat();
    let mut h = common.to_vec();
    h.push((
        "Content-Type",
        "application/vnd.gradle.build-cache-artifact.v2",
    ));
    assert_eq!(
        c.request("PUT", &format!("/cache/{k}"), &h, &gz)
            .await
            .status,
        200
    );

    let hit = c.request("GET", &format!("/cache/{k}"), &common, b"").await;
    assert_eq!(hit.status, 200);
    assert!(hit.header("content-encoding").is_none());
    assert_eq!(hit.body, gz);
}

#[tokio::test]
async fn health_and_ping() {
    // A long interval, so the eviction task does not refresh the flag mid-test.
    let s = TestServer::start(|c| c.eviction_interval = Duration::from_secs(3600)).await;
    let ping = once(s.cache, "GET", "/ping", &[], b"").await;
    assert_eq!((ping.status, ping.body.as_slice()), (200, &b"SUCCESS"[..]));
    assert_eq!(once(s.cache, "GET", "/health", &[], b"").await.status, 200);
    // The path the production load balancer checks.
    assert_eq!(
        once(s.cache, "GET", "/cache-node-info/health", &[], b"")
            .await
            .status,
        200
    );
    assert_eq!(s.metric("healthy").await, 1.0);

    // Unhealthy once the data directory stops accepting writes.
    s.app()
        .writable
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(once(s.cache, "GET", "/health", &[], b"").await.status, 503);
    assert_eq!(
        once(s.cache, "GET", "/cache-node-info/health", &[], b"")
            .await
            .status,
        503
    );
    // Liveness is unaffected.
    assert_eq!(once(s.cache, "GET", "/ping", &[], b"").await.status, 200);
}

#[tokio::test]
async fn health_endpoints_need_no_credentials() {
    let s = TestServer::start(|c| {
        c.anonymous_access = apps_infra_mobile_build_cache_server::config::Access::None
    })
    .await;
    assert_eq!(once(s.cache, "GET", "/ping", &[], b"").await.status, 200);
    assert_eq!(once(s.cache, "GET", "/health", &[], b"").await.status, 200);
    assert_eq!(once(s.admin, "GET", "/metrics", &[], b"").await.status, 200);
    assert_eq!(get(s.cache, &key(1)).await.status, 401);
}

#[tokio::test]
async fn unknown_paths_and_methods() {
    let s = TestServer::start(|_| {}).await;
    for path in [
        "/",
        "/cache",
        "/metrics",
        "/admin",
        "/wp-login.php",
        "/cache-node-info/version",
        "/.env",
    ] {
        assert_eq!(
            once(s.cache, "GET", path, &[], b"").await.status,
            404,
            "{path}"
        );
    }
    let k = key(1);
    for method in ["DELETE", "POST", "PATCH", "OPTIONS"] {
        let r = once(s.cache, method, &format!("/cache/{k}"), &[], b"").await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("GET, HEAD, PUT"));
    }
    // Method is checked before the key.
    assert_eq!(
        once(s.cache, "DELETE", "/cache/not-a-key", &[], b"")
            .await
            .status,
        405
    );
    let r = once(s.cache, "POST", "/ping", &[], b"").await;
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, HEAD")));
    assert_eq!(once(s.cache, "PUT", "/health", &[], b"x").await.status, 405);
}

#[tokio::test]
async fn invalid_keys_are_rejected_before_anything_else() {
    // No anonymous access: a 400 (not 401) shows the key check runs first.
    let s = TestServer::start(|c| {
        c.anonymous_access = apps_infra_mobile_build_cache_server::config::Access::None
    })
    .await;
    // A file outside the data directory that traversal would reach.
    let secret = s
        .dir
        .parent()
        .unwrap()
        .join(format!("secret-{}", std::process::id()));
    std::fs::write(&secret, "secret").unwrap();
    let secret_name = secret.file_name().unwrap().to_str().unwrap().to_string();

    let long = "a".repeat(129);
    let traversal = format!("../{secret_name}");
    let encoded = format!("..%2f{secret_name}");
    let bad = [
        "",
        "abc",
        "0123456789abcdef0123456789abcde",
        long.as_str(),
        "0123456789ABCDEF0123456789ABCDEF",
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        "0123456789abcdef0123456789abcdef.artifact",
        "0123456789abcdef0123456789abcdef/",
        "01/0123456789abcdef0123456789abcdef",
        "0123456789abcdef%2f123456789abcdef",
        "%30123456789abcdef0123456789abcdef",
        "..",
        ".tmp",
        traversal.as_str(),
        encoded.as_str(),
        "..%2f..%2f..%2f..%2fetc%2fpasswd",
        "....//....//etc/passwd",
    ];
    for k in bad {
        for method in ["GET", "HEAD", "PUT"] {
            let r = once(s.cache, method, &format!("/cache/{k}"), &[], b"").await;
            // Dot segments may be normalised away by nothing here: the raw
            // path is matched as sent, so all of these are plain bad keys.
            assert_eq!(r.status, 400, "{method} /cache/{k}");
        }
    }
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
    std::fs::remove_file(&secret).unwrap();
    // Nothing was created by the PUT attempts.
    let names: Vec<_> = std::fs::read_dir(&s.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [".tmp"]);
}

#[tokio::test]
async fn metrics_are_only_on_the_admin_listener() {
    let s = TestServer::start(|_| {}).await;
    let m = once(s.admin, "GET", "/metrics", &[], b"").await;
    assert_eq!(m.status, 200);
    assert_eq!(
        m.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = String::from_utf8(m.body).unwrap();
    assert!(text.contains("apps_infra_mobile_build_cache_build_info{version=\""));
    assert!(
        text.contains("# TYPE apps_infra_mobile_build_cache_request_duration_seconds histogram")
    );
    assert!(s.metric("disk_total_bytes").await > 0.0);
    assert!(s.metric("startup_scan_duration_seconds").await > 0.0);
    assert_eq!(s.metric("max_size_bytes").await, (1u64 << 30) as f64);

    assert_eq!(once(s.cache, "GET", "/metrics", &[], b"").await.status, 404);
    // The admin listener serves nothing else.
    assert_eq!(
        once(s.admin, "GET", &format!("/cache/{}", key(1)), &[], b"")
            .await
            .status,
        404
    );
    assert_eq!(once(s.admin, "GET", "/ping", &[], b"").await.status, 404);
    assert_eq!(
        once(s.admin, "POST", "/metrics", &[], b"").await.status,
        405
    );
}

// ---- Uploads the server will not keep ----

fn small_limits(c: &mut apps_infra_mobile_build_cache_server::config::Config) {
    c.max_artifact_size = 100_000;
    c.discard_ceiling = 400_000;
}

#[tokio::test]
async fn oversize_by_content_length_is_drained_and_answered_200() {
    let s = TestServer::start(small_limits).await;
    let k = key(10);
    let mut c = Conn::open(s.cache).await;
    let r = c
        .request("PUT", &format!("/cache/{k}"), &[], &pattern(100_001, 1))
        .await;
    assert_eq!(
        r.status, 200,
        "an error status would disable the client's cache"
    );
    assert_eq!(r.header("x-max-entry-size-bytes"), Some("100000"));
    assert!(!r.closes());
    // The body was consumed: the connection is in sync and still usable.
    assert_eq!(
        c.request("GET", &format!("/cache/{k}"), &[], b"")
            .await
            .status,
        404
    );
    // Exactly at the limit is accepted.
    assert_eq!(
        c.request("PUT", &format!("/cache/{k}"), &[], &pattern(100_000, 1))
            .await
            .status,
        200
    );
    assert_eq!(
        c.request("GET", &format!("/cache/{k}"), &[], b"")
            .await
            .status,
        200
    );

    assert_eq!(
        s.metric("rejected_uploads_total{reason=\"too_large\"}")
            .await,
        1.0
    );
    assert_eq!(s.app().index.len(), 1);
    assert_eq!(tmp_files(&s.dir), 0);
}

fn chunked(total: usize, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let data = pattern(total, 9);
    for piece in data.chunks(chunk) {
        out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        out.extend_from_slice(piece);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

#[tokio::test]
async fn chunked_upload_round_trip_and_overflow() {
    let s = TestServer::start(small_limits).await;
    let mut c = Conn::open(s.cache).await;
    let te = [("Transfer-Encoding", "chunked")];

    // Within the limit: stored, even though no length was declared.
    let ok = key(11);
    c.send(&req_head("PUT", &format!("/cache/{ok}"), &te, None))
        .await;
    c.send(&chunked(50_000, 7000)).await;
    assert_eq!(c.response().await.status, 200);
    assert_eq!(
        c.request("GET", &format!("/cache/{ok}"), &[], b"")
            .await
            .body,
        pattern(50_000, 9)
    );

    // Crossing the limit mid-stream: writing stops, the rest is drained, 200.
    let over = key(12);
    c.send(&req_head("PUT", &format!("/cache/{over}"), &te, None))
        .await;
    c.send(&chunked(250_000, 7000)).await;
    let r = c.response().await;
    assert_eq!(r.status, 200);
    assert!(!r.closes());
    assert_eq!(
        c.request("GET", &format!("/cache/{over}"), &[], b"")
            .await
            .status,
        404
    );

    assert_eq!(
        s.metric("rejected_uploads_total{reason=\"too_large\"}")
            .await,
        1.0
    );
    assert_eq!(s.app().index.len(), 1);
    assert_eq!(tmp_files(&s.dir), 0);
}

#[tokio::test]
async fn expect_continue_is_sent_only_when_the_body_will_be_read() {
    let s = TestServer::start(small_limits).await;
    let expect = [("Expect", "100-continue")];

    // Accepted upload: 100 Continue first, then the final status.
    let k = key(13);
    let body = pattern(60_000, 4);
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head(
        "PUT",
        &format!("/cache/{k}"),
        &expect,
        Some(body.len() as u64),
    ))
    .await;
    assert_eq!(c.head().await.status, 100);
    c.send(&body).await;
    assert_eq!(c.response().await.status, 200);
    assert_eq!(
        c.request("GET", &format!("/cache/{k}"), &[], b"")
            .await
            .body,
        body
    );

    // Oversized upload: the final 200 comes instead of 100 Continue and the
    // body is never sent. This is the path real Gradle takes.
    let big = key(14);
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head(
        "PUT",
        &format!("/cache/{big}"),
        &expect,
        Some(100_001),
    ))
    .await;
    let r = c.response().await;
    assert_eq!(r.status, 200, "must be the final status, not 100 Continue");
    assert!(
        r.closes(),
        "an unread body leaves the connection unusable; say so"
    );
    assert!(c.closed_within(Duration::from_secs(5)).await);
    assert_eq!(get(s.cache, &big).await.status, 404);
    assert_eq!(
        s.metric("rejected_uploads_total{reason=\"too_large\"}")
            .await,
        1.0
    );
    assert_eq!(s.metric("bytes_received_total").await, body.len() as f64);

    // Even absurd sizes get the soft answer when nothing has to be read.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head(
        "PUT",
        &format!("/cache/{big}"),
        &expect,
        Some(50_000_000_000),
    ))
    .await;
    assert_eq!(c.response().await.status, 200);
}

#[tokio::test]
async fn far_oversize_uploads_get_413_and_a_closed_connection() {
    let s = TestServer::start(small_limits).await;
    let k = key(15);

    // Declared size past the drain ceiling: refused without reading the body.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head("PUT", &format!("/cache/{k}"), &[], Some(400_001)))
        .await;
    let r = c.response().await;
    assert_eq!(r.status, 413);
    assert!(r.closes());
    assert!(c.closed_within(Duration::from_secs(5)).await);

    // Up to the ceiling the server still drains politely.
    let mut c = Conn::open(s.cache).await;
    let r = c
        .request("PUT", &format!("/cache/{k}"), &[], &pattern(400_000, 1))
        .await;
    assert_eq!(r.status, 200);

    // Chunked stream that keeps going past the ceiling.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head(
        "PUT",
        &format!("/cache/{k}"),
        &[("Transfer-Encoding", "chunked")],
        None,
    ))
    .await;
    let piece = [b"2000\r\n".as_slice(), &[b'x'; 0x2000], b"\r\n"].concat();
    let mut status = None;
    for _ in 0..200 {
        if c.try_send(&piece).await.is_err() {
            break;
        }
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_millis(1), c.try_head()).await {
            status = Some(r);
            break;
        }
    }
    let r = match status {
        Some(r) => r,
        None => c.head().await,
    };
    assert_eq!(r.status, 413);
    assert!(r.closes());

    assert_eq!(get(s.cache, &k).await.status, 404);
    assert_eq!(tmp_files(&s.dir), 0);
    assert_eq!(
        s.metric("rejected_uploads_total{reason=\"too_large\"}")
            .await,
        3.0
    );
}

#[tokio::test]
async fn upload_that_can_never_fit_is_soft_rejected() {
    let s = TestServer::start(|c| {
        c.max_size = Some(50_000);
        c.max_artifact_size = 100_000;
        c.discard_ceiling = 400_000;
    })
    .await;
    let r = put(s.cache, &key(16), &pattern(60_000, 1)).await;
    assert_eq!(r.status, 200);
    assert_eq!(get(s.cache, &key(16)).await.status, 404);
    assert_eq!(
        s.metric("rejected_uploads_total{reason=\"no_space\"}")
            .await,
        1.0
    );
}

#[tokio::test]
async fn length_mismatch_and_disconnects_leave_nothing_behind() {
    let s = TestServer::start(|_| {}).await;
    let k = key(17);

    // Client disconnects mid-upload, after enough bytes that a temp file exists.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head("PUT", &format!("/cache/{k}"), &[], Some(2 * MIB)))
        .await;
    c.send(&pattern(MIB as usize, 1)).await;
    wait_for("temp file to appear", || tmp_files(&s.dir) == 1).await;
    drop(c);
    wait_for("temp file to be removed", || tmp_files(&s.dir) == 0).await;
    assert_eq!(get(s.cache, &k).await.status, 404);
    assert_eq!(s.app().index.len(), 0);

    // Same for a small upload that never reached the disk.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head("PUT", &format!("/cache/{k}"), &[], Some(1000)))
        .await;
    c.send(&[1u8; 10]).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(get(s.cache, &k).await.status, 404);
    assert_eq!(tmp_files(&s.dir), 0);
}

#[tokio::test]
async fn concurrent_puts_of_one_key() {
    let s = TestServer::start(|_| {}).await;
    let k = key(18);
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let (addr, k) = (s.cache, k.clone());
        tasks.push(tokio::spawn(async move {
            // Each writer sends a body filled with its own byte.
            put(addr, &k, &vec![b'a' + i; 700_000]).await.status
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    // Whichever rename came last won, and it is one writer's body, intact.
    let body = get(s.cache, &k).await.body;
    assert_eq!(body.len(), 700_000);
    assert!(
        body.iter().all(|b| *b == body[0]),
        "bodies of two uploads were mixed"
    );
    assert_eq!(s.app().index.len(), 1);
    assert_eq!(s.app().index.total_bytes(), 700_000);
    assert_eq!(tmp_files(&s.dir), 0);
}

#[tokio::test]
async fn unread_bodies_on_early_responses() {
    let s = TestServer::start(|_| {}).await;
    // A small body on a rejected request is drained; the connection stays in sync.
    let mut c = Conn::open(s.cache).await;
    let r = c.request("PUT", "/cache/bad-key", &[], &[7u8; 1000]).await;
    assert_eq!(r.status, 400);
    assert!(!r.closes());
    assert_eq!(c.request("GET", "/ping", &[], b"").await.status, 200);

    // A large one is not read: the response announces the close.
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head("PUT", "/cache/bad-key", &[], Some(10 * MIB)))
        .await;
    let r = c.response().await;
    assert_eq!(r.status, 400);
    assert!(r.closes());
}

#[tokio::test]
async fn graceful_shutdown_finishes_requests_in_flight() {
    let mut s = TestServer::start(|_| {}).await;
    let k = key(19);
    let body = pattern(500_000, 5);
    let addr = s.cache;

    let mut c = Conn::open(addr).await;
    c.send(&req_head(
        "PUT",
        &format!("/cache/{k}"),
        &[],
        Some(body.len() as u64),
    ))
    .await;
    c.send(&body[..200_000]).await;
    // An idle keep-alive connection must not hold shutdown up.
    let mut idle = Conn::open(addr).await;
    assert_eq!(idle.request("GET", "/ping", &[], b"").await.status, 200);

    let finish = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.send(&body[200_000..]).await;
        c.response().await.status
    });
    assert!(
        s.shutdown().await,
        "shutdown should drain within the deadline"
    );
    assert_eq!(finish.await.unwrap(), 200, "the upload in flight completed");
    assert!(idle.closed_within(Duration::from_secs(2)).await);
    assert!(entry_path(&s.dir, &k).exists());
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "listener is closed"
    );
}
