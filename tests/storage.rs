//! Index, eviction, restart and crash safety.

mod common;

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apps_infra_mobile_build_cache_server::config::FreeSpace;
use apps_infra_mobile_build_cache_server::metrics::EvictReason;
use apps_infra_mobile_build_cache_server::store::set_entry_mtime;
use common::*;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn mtime(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Writes an entry file directly, as the old node would have left it.
fn seed(dir: &std::path::Path, k: &str, data: &[u8], age_secs: u64) {
    let path = entry_path(dir, k);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    set_entry_mtime(dir, k, now() - age_secs).unwrap();
}

#[tokio::test]
async fn restart_rebuilds_the_index_with_sizes_and_last_use_times() {
    let s = TestServer::start(|_| {}).await;
    put(s.cache, &key(1), &pattern(1000, 1)).await;
    put(s.cache, &key(2), &pattern(2000, 2)).await;
    put(s.cache, &key(3), &pattern(3000, 3)).await;
    let dir = s.stop().await;

    // Give the entries distinct, known last-use times.
    let base = now();
    set_entry_mtime(&dir, &key(1), base - 86_400).unwrap();
    set_entry_mtime(&dir, &key(2), base - 7_200).unwrap();
    // A leftover upload from the previous run.
    std::fs::write(dir.join(".tmp").join("leftover"), b"partial").unwrap();

    let s = TestServer::start_in(dir, |_| {}).await;
    assert_eq!(tmp_files(&s.dir), 0, ".tmp is emptied at startup");
    let ix = &s.app().index;
    assert_eq!((ix.len(), ix.total_bytes()), (3, 6000));
    let e1 = ix.get(&key(1)).unwrap();
    let e2 = ix.get(&key(2)).unwrap();
    let e3 = ix.get(&key(3)).unwrap();
    assert_eq!((e1.size, e1.last_used), (1000, base - 86_400));
    assert_eq!((e2.size, e2.last_used), (2000, base - 7_200));
    assert_eq!(e3.size, 3000);
    assert!(e3.last_used >= base - 5 && e3.last_used <= base + 5);
    assert_eq!(get(s.cache, &key(2)).await.body, pattern(2000, 2));
    assert_eq!(
        s.metric("oldest_entry_last_used_seconds").await,
        (base - 86_400) as f64
    );
}

#[tokio::test]
async fn hits_persist_last_use_to_the_file_at_most_once_per_interval() {
    let dir = temp_dir("touch");
    seed(&dir, &key(1), b"old entry", 10 * 86_400);
    let s = TestServer::start_in(dir, |_| {}).await;
    let path = entry_path(&s.dir, &key(1));
    let before = now();

    assert_eq!(get(s.cache, &key(1)).await.status, 200);
    let first = mtime(&path);
    assert!(first >= before, "the hit moved the file's mtime to now");
    assert!(s.app().index.get(&key(1)).unwrap().last_used >= before);

    // Further hits within the interval do not write metadata again.
    set_entry_mtime(&s.dir, &key(1), 12345).unwrap();
    for _ in 0..5 {
        assert_eq!(get(s.cache, &key(1)).await.status, 200);
    }
    assert_eq!(mtime(&path), 12345);

    // The persisted time survives a restart and orders eviction.
    set_entry_mtime(&s.dir, &key(1), first).unwrap();
    let dir = s.stop().await;
    let s = TestServer::start_in(dir, |_| {}).await;
    assert_eq!(s.app().index.get(&key(1)).unwrap().last_used, first);
}

#[tokio::test]
async fn existing_volume_is_served_and_stray_files_are_left_alone() {
    // The old node's artifacts-v2 directory, plus things that do not belong.
    let dir = temp_dir("legacy");
    seed(
        &dir,
        "ab54a98ceb1f0ad2c0ffee0123456789",
        b"from the old node",
        3600,
    );
    seed(&dir, &key(7), b"another", 60);
    std::fs::write(dir.join("ab").join("README.txt"), "stray").unwrap();
    std::fs::write(
        dir.join("ab")
            .join("ABCDEF0123456789ABCDEF0123456789.artifact"),
        "uppercase",
    )
    .unwrap();
    // A valid name in the wrong shard directory.
    std::fs::write(
        dir.join("ab").join(format!("{}.artifact", key(9))),
        "misplaced",
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "stray").unwrap();
    std::fs::create_dir_all(dir.join("lost+found")).unwrap();

    let s = TestServer::start_in(dir, |c| c.max_size = Some(30)).await;
    assert_eq!(s.metric("invalid_files").await, 5.0);
    assert_eq!(
        get(s.cache, "ab54a98ceb1f0ad2c0ffee0123456789").await.body,
        b"from the old node"
    );
    assert_eq!(
        get(s.cache, &key(9)).await.status,
        404,
        "misplaced files are not served"
    );

    // Eviction (this upload takes the cache over its 30-byte limit) never
    // touches them either.
    put(s.cache, &key(8), b"0123456789012345").await;
    wait_for("eviction", || s.app().index.total_bytes() <= 30).await;
    for stray in [
        "ab/README.txt",
        "ab/ABCDEF0123456789ABCDEF0123456789.artifact",
        "notes.txt",
        "lost+found",
    ] {
        assert!(s.dir.join(stray).exists(), "{stray} must not be deleted");
    }
    assert!(
        s.dir
            .join("ab")
            .join(format!("{}.artifact", key(9)))
            .exists()
    );
}

#[tokio::test]
async fn size_limit_evicts_least_recently_used_entries() {
    let dir = temp_dir("evict-size");
    // Ten 1000-byte entries, entry i last used i hours ago.
    for i in 1..=10u64 {
        seed(&dir, &key(i), &pattern(1000, i as u8), i * 3600);
    }
    let s = TestServer::start_in(dir, |c| {
        c.max_size = Some(10_500);
        c.eviction_low_watermark = 0.8;
        c.touch_interval = Duration::from_secs(1);
    })
    .await;
    assert_eq!(s.app().index.len(), 10);
    // Reading the oldest entry makes it the most recently used.
    assert_eq!(get(s.cache, &key(10)).await.status, 200);

    // One more upload goes over the limit and wakes eviction straight away.
    assert_eq!(put(s.cache, &key(11), &pattern(1000, 11)).await.status, 200);
    wait_for("eviction below the watermark", || {
        s.app().index.total_bytes() <= 8400
    })
    .await;

    // 11000 -> 8000 bytes: the three least recently used go (9, 8, 7).
    for evicted in [9, 8, 7] {
        assert_eq!(
            get(s.cache, &key(evicted)).await.status,
            404,
            "entry {evicted}"
        );
        assert!(!entry_path(&s.dir, &key(evicted)).exists());
    }
    for kept in [1, 2, 3, 4, 5, 6, 10, 11] {
        assert_eq!(get(s.cache, &key(kept)).await.status, 200, "entry {kept}");
    }
    assert_eq!(s.app().metrics.evictions(EvictReason::Size), 3);
    assert_eq!(s.metric("evictions_total{reason=\"size\"}").await, 3.0);
    assert_eq!(
        s.metric("evicted_bytes_total{reason=\"size\"}").await,
        3000.0
    );
    assert_eq!(s.metric("size_bytes").await, 8000.0);
}

#[tokio::test]
async fn max_age_evicts_unused_entries() {
    let dir = temp_dir("evict-age");
    seed(&dir, &key(1), b"stale", 3 * 86_400);
    seed(&dir, &key(2), b"fresh", 3600);
    let s = TestServer::start_in(dir, |c| c.max_age = Some(Duration::from_secs(86_400))).await;
    wait_for("age eviction", || {
        s.app().metrics.evictions(EvictReason::Age) == 1
    })
    .await;
    assert_eq!(get(s.cache, &key(1)).await.status, 404);
    assert_eq!(get(s.cache, &key(2)).await.status, 200);
    assert!(!entry_path(&s.dir, &key(1)).exists());
}

#[tokio::test]
async fn low_free_space_evicts_until_restored() {
    let dir = temp_dir("evict-free");
    for i in 1..=5u64 {
        seed(&dir, &key(i), &pattern(4000, i as u8), i * 3600);
    }
    // Ask for more free space than the volume has: every pass is short of space.
    let s = TestServer::start_in(dir, |c| {
        c.max_size = None;
        c.min_free_space = Some(FreeSpace::Percent(99.999));
    })
    .await;
    wait_for("free-space eviction", || s.app().index.is_empty()).await;
    assert_eq!(s.app().metrics.evictions(EvictReason::FreeSpace), 5);
    assert!(s.metric("disk_free_bytes").await > 0.0);

    // And an upload that cannot fit beside the reserve is soft-rejected.
    assert_eq!(put(s.cache, &key(6), &pattern(4000, 6)).await.status, 200);
    let total = s.metric("disk_total_bytes").await;
    if total < 4000.0 * 100_000.0 {
        assert_eq!(get(s.cache, &key(6)).await.status, 404);
    }
}

#[tokio::test]
async fn entry_evicted_mid_download_keeps_streaming() {
    let s = TestServer::start(|c| c.max_size = Some(40 * MIB)).await;
    let big = pattern(32 * MIB as usize, 1);
    assert_eq!(put(s.cache, &key(1), &big).await.status, 200);

    // Start downloading and stop reading after the first bytes: the rest is
    // still on the server side.
    let mut c = Conn::open(s.cache).await;
    c.send(&req("GET", &format!("/cache/{}", key(1)), &[], b""))
        .await;
    let head = c.head().await;
    assert_eq!((head.status, head.content_length()), (200, Some(32 * MIB)));
    let mut received = c.read_body(100_000).await;

    // A second upload pushes the cache over its limit; the first entry is the
    // least recently used and is evicted while its download is in flight.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        put(s.cache, &key(2), &pattern(16 * MIB as usize, 2))
            .await
            .status,
        200
    );
    wait_for("eviction of the entry being read", || {
        s.app().index.get(&key(1)).is_none()
    })
    .await;
    assert!(!entry_path(&s.dir, &key(1)).exists());
    assert_eq!(get(s.cache, &key(1)).await.status, 404, "new requests miss");

    // The open file handle outlives the unlink: the download completes intact.
    received.extend(c.read_body(big.len() - 100_000).await);
    assert!(received == big, "download corrupted by eviction");
}

#[tokio::test]
async fn requests_are_served_while_the_startup_scan_is_running() {
    let dir = temp_dir("scan");
    seed(&dir, &key(1), b"already here", 3600);
    let s = TestServer::start_in(dir, |c| {
        c.max_size = Some(10);
        c.max_artifact_size = 1000;
        c.discard_ceiling = 4000;
    })
    .await;
    // Put the server back into the state it is in while the scan runs: the
    // index is incomplete and says so.
    wait_for("startup eviction", || s.app().index.is_empty()).await;
    seed(&s.dir, &key(1), b"already here", 3600);
    s.app().index.set_ready(false);

    // Never 503 for cache requests: reads go straight to disk...
    let hit = get(s.cache, &key(1)).await;
    assert_eq!(
        (hit.status, hit.body.as_slice()),
        (200, &b"already here"[..])
    );
    assert_eq!(
        s.app().index.get(&key(1)).unwrap().size,
        12,
        "the read indexed the entry"
    );
    assert_eq!(get(s.cache, &key(2)).await.status, 404);
    // ...and writes are accepted.
    assert_eq!(put(s.cache, &key(3), b"new entry").await.status, 200);
    assert_eq!(get(s.cache, &key(3)).await.body, b"new entry");
    // Readiness reports the truth meanwhile.
    assert_eq!(once(s.cache, "GET", "/health", &[], b"").await.status, 503);

    // Eviction is held back until the scan finishes, even over the limit.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.app().index.len(), 2);
    s.app().index.set_ready(true);
    wait_for("eviction after the scan", || {
        s.app().index.total_bytes() <= 10
    })
    .await;
    assert_eq!(once(s.cache, "GET", "/health", &[], b"").await.status, 200);
}

// ---- Tests that run the real binary ----

const BIN: &str = env!("CARGO_BIN_EXE_apps-infra-mobile-build-cache-server");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_server(dir: &std::path::Path, port: u16) -> tokio::process::Child {
    tokio::process::Command::new(BIN)
        .env_clear()
        .env("CACHE_LISTEN", format!("127.0.0.1:{port}"))
        .env("CACHE_ADMIN_LISTEN", "")
        .env("CACHE_DATA_DIR", dir)
        .env("CACHE_MAX_SIZE", "1GiB")
        .env("CACHE_MIN_FREE_SPACE", "none")
        .env("CACHE_ANONYMOUS_ACCESS", "readwrite")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("server binary starts")
}

async fn wait_listening(addr: std::net::SocketAddr) {
    for _ in 0..500 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not start listening on {addr}");
}

async fn wait_health(addr: std::net::SocketAddr) {
    for _ in 0..500 {
        if once(addr, "GET", "/health", &[], b"").await.status == 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not become healthy");
}

#[tokio::test]
async fn killed_during_uploads_serves_no_partial_entry_after_restart() {
    let dir = temp_dir("crash");
    let port = free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut child = spawn_server(&dir, port);
    wait_listening(addr).await;
    wait_health(addr).await;

    // One complete upload (fsync is on by default)...
    let complete = pattern(MIB as usize, 9);
    assert_eq!(put(addr, &key(100), &complete).await.status, 200);
    // ...and eight that are half-way through when the process dies.
    let mut uploads = Vec::new();
    for i in 0..8u64 {
        let mut c = Conn::open(addr).await;
        c.send(&req_head(
            "PUT",
            &format!("/cache/{}", key(i)),
            &[],
            Some(4 * MIB),
        ))
        .await;
        c.send(&pattern(2 * MIB as usize, i as u8)).await;
        uploads.push(c);
    }
    wait_for("partial uploads to reach the disk", || tmp_files(&dir) == 8).await;
    child.kill().await.unwrap(); // SIGKILL: no cleanup runs.
    drop(uploads);
    assert_eq!(tmp_files(&dir), 8, "the crash left partial files behind");

    let mut child = spawn_server(&dir, port);
    wait_listening(addr).await;
    wait_health(addr).await;
    assert_eq!(
        tmp_files(&dir),
        0,
        "partial uploads are discarded at startup"
    );
    for i in 0..8u64 {
        assert_eq!(
            get(addr, &key(i)).await.status,
            404,
            "partial entry {i} must not be served"
        );
    }
    assert!(get(addr, &key(100)).await.body == complete);

    // SIGTERM: graceful exit with status 0.
    let pid = child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "exit status after SIGTERM: {status:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn hash_password_and_healthcheck_subcommands() {
    use apps_infra_mobile_build_cache_server::auth;
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(BIN)
        .args(["hash-password", "ci-user", "readwrite"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"s3cret pass\n")
        .await
        .unwrap();
    let out = child.wait_with_output().await.unwrap();
    assert!(out.status.success());
    let line = String::from_utf8(out.stdout).unwrap();
    assert!(
        line.starts_with("ci-user:readwrite:$argon2id$v=19$m=19456,t=2,p=1$"),
        "{line}"
    );
    // The printed line is a valid users file, and the trailing newline was
    // not hashed as part of the password.
    assert_eq!(auth::parse_users(&line).unwrap().len(), 1);
    let hash = line.trim_end().splitn(3, ':').nth(2).unwrap();
    assert!(auth::verify_password(b"s3cret pass", hash));
    assert!(!auth::verify_password(b"s3cret pass\n", hash));

    for bad in [
        &["hash-password", "a:b", "read"][..],
        &["hash-password", "user", "admin"],
        &["frobnicate"],
    ] {
        let out = tokio::process::Command::new(BIN)
            .args(bad)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "{bad:?}");
    }

    // healthcheck: fails with nothing listening, succeeds against a live server.
    let dir = temp_dir("healthcheck");
    let port = free_port();
    let check = || async {
        tokio::process::Command::new(BIN)
            .arg("healthcheck")
            .env_clear()
            .env("CACHE_LISTEN", format!("0.0.0.0:{port}"))
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success()
    };
    assert!(!check().await);
    let _child = spawn_server(&dir, port);
    wait_listening(format!("127.0.0.1:{port}").parse().unwrap()).await;
    assert!(check().await);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn refuses_to_start_on_bad_configuration() {
    let run = |vars: &[(&str, &str)]| {
        let mut cmd = tokio::process::Command::new(BIN);
        cmd.env_clear().stdout(Stdio::null()).stderr(Stdio::piped());
        for (k, v) in vars {
            cmd.env(k, v);
        }
        async move {
            let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
                .await
                .unwrap()
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        }
    };
    let (ok, err) = run(&[("CACHE_MIN_FREE_SPACE", "none")]).await;
    assert!(!ok && err.contains("no eviction limit"), "{err}");
    let (ok, err) = run(&[("CACHE_MAX_SIZE", "ten gigs")]).await;
    assert!(!ok && err.contains("CACHE_MAX_SIZE"), "{err}");
    let (ok, err) = run(&[("CACHE_MAX_SISE", "10GiB")]).await;
    assert!(
        !ok && err.contains("unknown configuration variable CACHE_MAX_SISE"),
        "{err}"
    );
    let (ok, err) = run(&[("CACHE_USERS_FILE", "/nonexistent/users")]).await;
    assert!(!ok && err.contains("users file"), "{err}");
}
