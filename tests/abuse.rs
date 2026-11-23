//! Hostile and broken clients. The server
//! is internet-facing: it must shed these cheaply and keep serving.

mod common;

use std::time::{Duration, Instant};

use common::*;

async fn assert_alive(s: &TestServer) {
    let r = once(s.cache, "GET", "/ping", &[], b"").await;
    assert_eq!((r.status, r.body.as_slice()), (200, &b"SUCCESS"[..]));
}

#[tokio::test]
async fn slow_or_silent_request_heads_are_cut_off() {
    let s = TestServer::start(|c| c.header_timeout = Duration::from_millis(300)).await;

    // Connects and never sends anything.
    let mut silent = Conn::open(s.cache).await;
    // Slowloris: a head that trickles in and never finishes.
    let mut slow = Conn::open(s.cache).await;
    slow.send(b"GET /ping HTTP/1.1\r\nHost: x\r\n").await;
    let started = Instant::now();
    let drip = tokio::spawn(async move {
        for _ in 0..40 {
            if slow.try_send(b"X-Drip: 1\r\n").await.is_err() {
                break;
            }
            if slow.closed_within(Duration::from_millis(50)).await {
                return Some(slow);
            }
        }
        None
    });
    assert!(
        silent.closed_within(Duration::from_secs(3)).await,
        "silent connection was kept open"
    );
    assert!(
        drip.await.unwrap().is_some(),
        "dripping headers kept the connection open"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_alive(&s).await;
}

#[tokio::test]
async fn keep_alive_connections_outlive_the_header_timeout() {
    // The header timeout must not count idle time between requests, or the
    // load balancer's pooled connections would be closed under it.
    let s = TestServer::start(|c| {
        c.header_timeout = Duration::from_millis(200);
        c.idle_timeout = Duration::from_secs(5);
    })
    .await;
    let mut c = Conn::open(s.cache).await;
    for _ in 0..3 {
        assert_eq!(c.request("GET", "/ping", &[], b"").await.status, 200);
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    assert_eq!(c.request("GET", "/ping", &[], b"").await.status, 200);

    // But a head that starts on a kept-alive connection still has to finish in time.
    c.send(b"GET /ping HTTP/1.1\r\n").await;
    assert!(c.closed_within(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn idle_connections_are_closed_after_the_idle_timeout() {
    let s = TestServer::start(|c| c.idle_timeout = Duration::from_millis(400)).await;
    let mut c = Conn::open(s.cache).await;
    assert_eq!(c.request("GET", "/ping", &[], b"").await.status, 200);
    let started = Instant::now();
    assert!(c.closed_within(Duration::from_secs(3)).await);
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "closed too early: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn stalled_upload_body_is_abandoned() {
    let s = TestServer::start(|c| c.idle_timeout = Duration::from_millis(400)).await;
    let k = key(1);
    let mut c = Conn::open(s.cache).await;
    c.send(&req_head("PUT", &format!("/cache/{k}"), &[], Some(2 * MIB)))
        .await;
    c.send(&pattern(MIB as usize, 1)).await;
    wait_for("temp file", || tmp_files(&s.dir) == 1).await;
    // ...and then nothing. The server gives up and cleans up.
    let _ = c.try_head().await;
    assert!(c.closed_within(Duration::from_secs(3)).await);
    wait_for("temp file removal", || tmp_files(&s.dir) == 0).await;
    assert_eq!(get(s.cache, &k).await.status, 404);
}

#[tokio::test]
async fn stalled_download_is_abandoned() {
    let s = TestServer::start(|c| c.idle_timeout = Duration::from_millis(400)).await;
    let k = key(2);
    put(s.cache, &k, &pattern(48 * MIB as usize, 3)).await;
    // Ask for the entry and never read it: the server's writes stall.
    let mut c = Conn::open(s.cache).await;
    c.send(&req("GET", &format!("/cache/{k}"), &[], b"")).await;
    wait_for("connection to be dropped", || {
        s.app()
            .metrics
            .connections_active
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
    })
    .await;
    drop(c);
    assert_alive(&s).await;
}

#[tokio::test]
async fn oversized_and_malformed_requests() {
    let s = TestServer::start(|_| {}).await;

    // A request head far beyond the limit.
    let mut c = Conn::open(s.cache).await;
    let mut head = b"GET /ping HTTP/1.1\r\nHost: x\r\n".to_vec();
    for i in 0..3000 {
        head.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "a".repeat(100)).as_bytes());
    }
    head.extend_from_slice(b"\r\n");
    let _ = c.try_send(&head).await;
    // Closing outright, without a response, is acceptable too.
    if let Some(r) = c.try_head().await {
        assert!(r.status == 431 || r.status == 400, "status {}", r.status);
    }

    // One enormous header line, and an enormous request target.
    for junk in [
        format!(
            "GET /ping HTTP/1.1\r\nX-Big: {}\r\n\r\n",
            "b".repeat(300_000)
        ),
        format!(
            "GET /cache/{} HTTP/1.1\r\nHost: x\r\n\r\n",
            "c".repeat(300_000)
        ),
    ] {
        let mut c = Conn::open(s.cache).await;
        let _ = c.try_send(junk.as_bytes()).await;
        if let Some(r) = c.try_head().await {
            assert!((400..500).contains(&r.status), "status {}", r.status);
        }
    }

    // Garbage instead of HTTP.
    let garbage: [&[u8]; 9] = [
        b"\x00\x01\x02\x03\xff\xfe\r\n\r\n",
        b"GET\r\n\r\n",
        b"GET /ping\r\n\r\n",
        b"GET /ping HTTP/9.9\r\nHost: x\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03", // a TLS ClientHello
        b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        b"GET /ping HTTP/1.1\r\nBroken header line\r\n\r\n",
        b"PUT /cache/00000000000000000000000000000001 HTTP/1.1\r\nContent-Length: -5\r\n\r\n",
        b"PUT /cache/00000000000000000000000000000001 HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n",
    ];
    for bytes in garbage {
        let mut c = Conn::open(s.cache).await;
        let _ = c.try_send(bytes).await;
        if let Some(r) = c.try_head().await {
            assert!(
                (400..=505).contains(&r.status),
                "{:?} -> {}",
                String::from_utf8_lossy(bytes),
                r.status
            );
        }
    }

    // Request smuggling shapes: both a length and chunked. Must not desync.
    let mut c = Conn::open(s.cache).await;
    c.send(b"PUT /cache/00000000000000000000000000000002 HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").await;
    if let Some(r) = c.try_response().await {
        assert!(r.status == 200 || r.status == 400, "status {}", r.status);
    }

    // Scanner probes.
    for path in [
        "/wp-login.php",
        "/.git/config",
        "/admin/",
        "/cgi-bin/luci",
        "/cache/../../etc/passwd",
    ] {
        let r = once(s.cache, "GET", path, &[], b"").await;
        assert!(r.status == 404 || r.status == 400, "{path} -> {}", r.status);
    }
    assert_alive(&s).await;
    assert_eq!(tmp_files(&s.dir), 0);
}

#[tokio::test]
async fn connection_cap_holds_and_recovers() {
    let s = TestServer::start(|c| {
        c.max_connections = 8;
        c.header_timeout = Duration::from_millis(500);
    })
    .await;
    // Fill every slot with connections that never send a request.
    let mut idle = Vec::new();
    for _ in 0..8 {
        idle.push(Conn::open(s.cache).await);
    }
    wait_for("all slots taken", || {
        s.app()
            .metrics
            .connections_active
            .load(std::sync::atomic::Ordering::Relaxed)
            == 8
    })
    .await;
    // More connections are accepted by the kernel but wait in the backlog...
    let mut extra = Vec::new();
    for _ in 0..20 {
        extra.push(Conn::open(s.cache).await);
    }
    assert_eq!(
        s.app()
            .metrics
            .connections_active
            .load(std::sync::atomic::Ordering::Relaxed),
        8
    );
    // ...and a real request is served as soon as the idle ones time out.
    let started = Instant::now();
    assert_alive(&s).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    for c in &mut idle {
        assert!(c.closed_within(Duration::from_secs(3)).await);
    }
    // The metrics listener has its own, separate limit and stayed reachable.
    assert!(s.metric("connections_active").await <= 8.0);
}

#[tokio::test]
async fn many_connections_at_once() {
    let s = TestServer::start(|_| {}).await;
    put(s.cache, &key(1), &pattern(20_000, 1)).await;
    let mut tasks = Vec::new();
    for i in 0..200u64 {
        let addr = s.cache;
        tasks.push(tokio::spawn(async move {
            let mut c = Conn::open(addr).await;
            for j in 0..10u64 {
                let hit = c
                    .request("GET", &format!("/cache/{}", key(1)), &[], b"")
                    .await;
                assert_eq!((hit.status, hit.body.len()), (200, 20_000));
                let k = key(1000 + i * 100 + j);
                assert_eq!(
                    c.request("GET", &format!("/cache/{k}"), &[], b"")
                        .await
                        .status,
                    404
                );
                assert_eq!(
                    c.request("PUT", &format!("/cache/{k}"), &[], &[i as u8; 500])
                        .await
                        .status,
                    200
                );
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(s.app().index.len(), 2001);
    assert_eq!(
        s.metric("requests_total{method=\"GET\",status=\"200\"}")
            .await,
        2000.0
    );
    assert_eq!(tmp_files(&s.dir), 0);
}
