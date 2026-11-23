//! Shared helpers: an in-process server on a temp directory and a raw-socket
//! HTTP client, so the tests see exactly what is on the wire.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use apps_infra_mobile_build_cache_server::App;
use apps_infra_mobile_build_cache_server::config::{Access, Config};
use apps_infra_mobile_build_cache_server::server::Server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const MIB: u64 = 1 << 20;
const IO_TIMEOUT: Duration = Duration::from_secs(20);

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!(
        "build-cache-test-{label}-{}-{n}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 32-character lowercase hex key.
pub fn key(n: u64) -> String {
    format!("{n:032x}")
}

pub fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(&key[..2]).join(format!("{key}.artifact"))
}

pub fn tmp_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(".tmp")).map_or(0, |d| d.count())
}

pub fn test_config(dir: &Path) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        admin_listen: Some("127.0.0.1:0".parse().unwrap()),
        data_dir: dir.to_path_buf(),
        max_size: Some(1 << 30),
        min_free_space: None,
        fsync: false,
        anonymous_access: Access::ReadWrite,
        eviction_interval: Duration::from_millis(100),
        shutdown_timeout: Duration::from_secs(5),
        ..Config::default()
    }
}

pub struct TestServer {
    server: Option<Server>,
    pub dir: PathBuf,
    pub cache: SocketAddr,
    pub admin: SocketAddr,
    remove_dir: bool,
}

impl TestServer {
    pub async fn start(configure: impl FnOnce(&mut Config)) -> TestServer {
        TestServer::start_in(temp_dir("srv"), configure).await
    }

    pub async fn start_in(dir: PathBuf, configure: impl FnOnce(&mut Config)) -> TestServer {
        apps_infra_mobile_build_cache_server::log::set_enabled(false);
        let mut cfg = test_config(&dir);
        configure(&mut cfg);
        let server = Server::start(cfg).await.expect("server starts");
        let t = TestServer {
            cache: server.cache_addr,
            admin: server.admin_addr.expect("admin listener"),
            server: Some(server),
            dir,
            remove_dir: true,
        };
        t.wait_healthy().await;
        t
    }

    pub fn app(&self) -> &Arc<App> {
        &self.server.as_ref().unwrap().app
    }

    pub async fn wait_healthy(&self) {
        wait_for("server to become healthy", || self.app().is_healthy()).await;
    }

    /// Shuts down gracefully and hands back the data directory.
    pub async fn stop(mut self) -> PathBuf {
        self.remove_dir = false;
        assert!(
            self.server.take().unwrap().shutdown().await,
            "connections drained"
        );
        self.dir.clone()
    }

    pub async fn shutdown(&mut self) -> bool {
        self.server.take().unwrap().shutdown().await
    }

    /// The value of one metric sample, matched by its full name and labels.
    pub async fn metric(&self, sample: &str) -> f64 {
        let resp = once(self.admin, "GET", "/metrics", &[], b"").await;
        assert_eq!(resp.status, 200);
        let text = String::from_utf8(resp.body).unwrap();
        let full = format!("apps_infra_mobile_build_cache_{sample} ");
        text.lines()
            .find_map(|l| l.strip_prefix(&full))
            .unwrap_or_else(|| panic!("metric {sample} not found in:\n{text}"))
            .parse()
            .unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.remove_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Polls a condition for up to ten seconds.
pub async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").map(|v| v.parse().unwrap())
    }

    pub fn closes(&self) -> bool {
        self.header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
    }
}

pub struct Conn {
    pub stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    pub async fn open(addr: SocketAddr) -> Conn {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(addr))
            .await
            .expect("connect timed out")
            .expect("connect");
        Conn {
            stream,
            buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.try_send(bytes).await.expect("send");
    }

    pub async fn try_send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        tokio::time::timeout(IO_TIMEOUT, self.stream.write_all(bytes))
            .await
            .expect("send timed out")
    }

    async fn fill(&mut self) -> Option<usize> {
        let mut chunk = [0u8; 16 * 1024];
        match tokio::time::timeout(IO_TIMEOUT, self.stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => None,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Some(n)
            }
            Err(_) => panic!("read timed out"),
        }
    }

    /// Reads a status line and headers. `None` if the peer closed first.
    pub async fn try_head(&mut self) -> Option<Resp> {
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            self.fill().await?;
        };
        let head = String::from_utf8(self.buf[..end].to_vec()).expect("ASCII response head");
        self.buf.drain(..end + 4);
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1 "),
            "bad status line {status_line:?}"
        );
        let status = status_line[9..12].parse().unwrap();
        let headers = lines
            .map(|l| {
                let (n, v) = l.split_once(':').unwrap();
                (n.trim().to_string(), v.trim().to_string())
            })
            .collect();
        Some(Resp {
            status,
            headers,
            body: Vec::new(),
        })
    }

    pub async fn head(&mut self) -> Resp {
        self.try_head()
            .await
            .expect("connection closed before a response")
    }

    /// Reads exactly `n` body bytes.
    pub async fn read_body(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            self.fill().await.expect("connection closed mid-body");
        }
        self.buf.drain(..n).collect()
    }

    /// Reads a complete response to a request that is not HEAD.
    pub async fn try_response(&mut self) -> Option<Resp> {
        let mut resp = self.try_head().await?;
        assert!(
            resp.header("transfer-encoding").is_none(),
            "the server never needs chunked responses"
        );
        if resp.status >= 200 && resp.status != 204 && resp.status != 304 {
            let len = resp.content_length().expect("content-length") as usize;
            resp.body = self.read_body(len).await;
        }
        Some(resp)
    }

    pub async fn response(&mut self) -> Resp {
        self.try_response()
            .await
            .expect("connection closed before a response")
    }

    /// True if the server closes the connection (or resets it) within `within`
    /// without sending anything further.
    pub async fn closed_within(&mut self, within: Duration) -> bool {
        if !self.buf.is_empty() {
            return false;
        }
        let mut byte = [0u8; 1];
        match tokio::time::timeout(within, self.stream.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) => true,
            Ok(Ok(_)) => {
                self.buf.push(byte[0]);
                false
            }
            Err(_) => false,
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Resp {
        self.send(&req(method, path, headers, body)).await;
        if method == "HEAD" {
            self.head().await
        } else {
            self.response().await
        }
    }
}

/// Serialises a request. `Content-Length` is added for PUT and for any body.
pub fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = req_head(
        method,
        path,
        headers,
        (method == "PUT" || !body.is_empty()).then_some(body.len() as u64),
    );
    out.extend_from_slice(body);
    out
}

pub fn req_head(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_length: Option<u64>,
) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: cache.test\r\n");
    for (n, v) in headers {
        out.push_str(&format!("{n}: {v}\r\n"));
    }
    if let Some(len) = content_length {
        out.push_str(&format!("Content-Length: {len}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// One request on a fresh connection.
pub async fn once(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    Conn::open(addr)
        .await
        .request(method, path, headers, body)
        .await
}

pub async fn put(addr: SocketAddr, key: &str, body: &[u8]) -> Resp {
    once(addr, "PUT", &format!("/cache/{key}"), &[], body).await
}

pub async fn get(addr: SocketAddr, key: &str) -> Resp {
    once(addr, "GET", &format!("/cache/{key}"), &[], b"").await
}

pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// An `Authorization` header value.
pub fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
}

/// Deterministic, non-repeating test data.
pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut state = 0x9E37_79B9u32 ^ u32::from(seed);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}
