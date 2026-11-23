//! Load generator for the build cache server.
//!
//! ```text
//! cargo run --release --example loadgen -- 127.0.0.1:5071 [--connections 32] [--seconds 30] [--preload 2000]
//! ```
//!
//! Workload: 90 % GET (half for keys that were uploaded, half for random keys,
//! matching the measured 50 % hit rate) and 10 % PUT, with entry sizes drawn
//! from the production distribution: median 2 KB, p90 309 KB,
//! p99 17.7 MB, maximum 152.7 MB. The server needs anonymous `readwrite`
//! access and `CACHE_MAX_ARTIFACT_SIZE` of at least 200MiB. Run it with a
//! small `CACHE_MAX_SIZE` so eviction is active throughout.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Quantile points of the production entry-size distribution.
const SIZE_QUANTILES: [(f64, f64); 5] = [
    (0.0, 449.0),
    (0.5, 2_000.0),
    (0.9, 309_000.0),
    (0.99, 17_700_000.0),
    (1.0, 152_700_000.0),
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn key(&mut self) -> String {
        format!("{:016x}{:016x}", self.next(), self.next())
    }
    /// Log-linear interpolation between the quantile points.
    fn size(&mut self) -> usize {
        let u = self.unit();
        for w in SIZE_QUANTILES.windows(2) {
            let ((q0, s0), (q1, s1)) = (w[0], w[1]);
            if u <= q1 {
                let t = (u - q0) / (q1 - q0);
                return (s0.ln() + t * (s1.ln() - s0.ln())).exp() as usize;
            }
        }
        SIZE_QUANTILES[4].1 as usize
    }
}

#[derive(Default)]
struct Stats {
    get_hit: Vec<Duration>,
    get_miss: Vec<Duration>,
    put: Vec<Duration>,
    bytes_up: u64,
    bytes_down: u64,
    errors: u64,
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    async fn connect(addr: &str) -> std::io::Result<Client> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Client {
            stream,
            buf: Vec::with_capacity(64 * 1024),
        })
    }

    /// Sends a request and reads the response; returns (status, body length).
    async fn call(&mut self, head: &str, body: &[u8]) -> std::io::Result<(u16, u64)> {
        self.stream.write_all(head.as_bytes()).await?;
        self.stream.write_all(body).await?;
        self.buf.clear();
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&self.buf[..end]).to_ascii_lowercase();
        let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
        let length: u64 = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut remaining = length.saturating_sub((self.buf.len() - end) as u64);
        let mut sink = vec![0u8; 256 * 1024];
        while remaining > 0 {
            let want = remaining.min(sink.len() as u64) as usize;
            let n = self.stream.read(&mut sink[..want]).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            remaining -= n as u64;
        }
        Ok((status, length))
    }

    async fn put(&mut self, key: &str, body: &[u8]) -> std::io::Result<u16> {
        let head = format!(
            "PUT /cache/{key} HTTP/1.1\r\nHost: load\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        Ok(self.call(&head, body).await?.0)
    }

    async fn get(&mut self, key: &str) -> std::io::Result<(u16, u64)> {
        self.call(
            &format!("GET /cache/{key} HTTP/1.1\r\nHost: load\r\n\r\n"),
            b"",
        )
        .await
    }
}

async fn worker(
    addr: String,
    id: u64,
    deadline: Instant,
    keys: Arc<Mutex<Vec<String>>>,
    payload: Arc<Vec<u8>>,
) -> Stats {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (id + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
    let mut stats = Stats::default();
    let mut client = None;
    while Instant::now() < deadline {
        let c = match &mut client {
            Some(c) => c,
            None => match Client::connect(&addr).await {
                Ok(c) => client.insert(c),
                Err(_) => {
                    stats.errors += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let roll = rng.unit();
        let started = Instant::now();
        let ok = if roll < 0.10 {
            let key = rng.key();
            let size = rng.size().min(payload.len());
            let offset = rng.next() as usize % (payload.len() - size + 1);
            match c.put(&key, &payload[offset..offset + size]).await {
                Ok(200) => {
                    stats.put.push(started.elapsed());
                    stats.bytes_up += size as u64;
                    let mut keys = keys.lock().unwrap();
                    if keys.len() < 200_000 {
                        keys.push(key);
                    } else {
                        let i = rng.next() as usize % keys.len();
                        keys[i] = key;
                    }
                    true
                }
                _ => false,
            }
        } else {
            let key = if roll < 0.55 {
                let keys = keys.lock().unwrap();
                // Favour recent uploads, as builds do; older ones may be evicted.
                let n = keys.len();
                let back = ((rng.unit() * rng.unit()) * n as f64) as usize;
                keys.get(n.saturating_sub(1 + back))
                    .cloned()
                    .unwrap_or_else(|| rng.key())
            } else {
                rng.key()
            };
            match c.get(&key).await {
                Ok((200, len)) => {
                    stats.get_hit.push(started.elapsed());
                    stats.bytes_down += len;
                    true
                }
                Ok((404, _)) => {
                    stats.get_miss.push(started.elapsed());
                    true
                }
                _ => false,
            }
        };
        if !ok {
            stats.errors += 1;
            client = None;
        }
    }
    stats
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p) as usize].as_secs_f64() * 1000.0
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(addr) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: loadgen HOST:PORT [--connections N] [--seconds N] [--preload N]");
        std::process::exit(2);
    };
    let connections: u64 = arg(&args, "--connections", 32);
    let seconds: u64 = arg(&args, "--seconds", 30);
    let preload: usize = arg(&args, "--preload", 2000);

    // One buffer of incompressible bytes; uploads are slices of it.
    let mut rng = Rng(42);
    let mut payload = vec![0u8; SIZE_QUANTILES[4].1 as usize];
    for chunk in payload.chunks_mut(8) {
        let v = rng.next().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    let payload = Arc::new(payload);

    // Seed the cache so the first GETs can hit.
    let keys = Arc::new(Mutex::new(Vec::new()));
    let mut client = Client::connect(&addr).await.expect("connect to the server");
    for _ in 0..preload {
        let key = rng.key();
        let size = rng.size().min(1_000_000);
        assert_eq!(
            client
                .put(&key, &payload[..size])
                .await
                .expect("preload PUT"),
            200
        );
        keys.lock().unwrap().push(key);
    }
    drop(client);

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let tasks: Vec<_> = (0..connections)
        .map(|id| {
            tokio::spawn(worker(
                addr.clone(),
                id,
                deadline,
                keys.clone(),
                payload.clone(),
            ))
        })
        .collect();
    let mut total = Stats::default();
    for t in tasks {
        let s = t.await.expect("worker");
        total.get_hit.extend(s.get_hit);
        total.get_miss.extend(s.get_miss);
        total.put.extend(s.put);
        total.bytes_up += s.bytes_up;
        total.bytes_down += s.bytes_down;
        total.errors += s.errors;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let requests = total.get_hit.len() + total.get_miss.len() + total.put.len();
    println!(
        "connections {connections}, duration {elapsed:.1}s, requests {requests}, errors {}",
        total.errors
    );
    println!(
        "throughput  {:.0} req/s, upload {:.1} MB/s, download {:.1} MB/s",
        requests as f64 / elapsed,
        total.bytes_up as f64 / 1e6 / elapsed,
        total.bytes_down as f64 / 1e6 / elapsed
    );
    for (name, mut v) in [
        ("GET hit ", total.get_hit),
        ("GET miss", total.get_miss),
        ("PUT     ", total.put),
    ] {
        v.sort();
        println!(
            "{name}  n={:<8} p50 {:>8.3} ms   p99 {:>8.3} ms   max {:>8.1} ms",
            v.len(),
            percentile(&v, 0.5),
            percentile(&v, 0.99),
            percentile(&v, 1.0)
        );
    }
}
