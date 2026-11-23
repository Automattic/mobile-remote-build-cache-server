//! Gradle remote build cache server. See `README.md` for operation.

#![forbid(unsafe_code)]

pub mod auth;
pub mod body;
pub mod config;
pub mod evict;
pub mod index;
pub mod log;
pub mod metrics;
pub mod routes;
pub mod server;
pub mod store;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

use tokio::sync::Notify;

/// Shared state of one server instance.
pub struct App {
    pub cfg: config::Config,
    pub store: Arc<store::Store>,
    pub index: index::Index,
    pub auth: auth::Auth,
    pub metrics: Arc<metrics::Metrics>,
    /// Wakes the eviction task ahead of its interval.
    pub evict_wake: Notify,
    /// Whether the data directory accepted a write at the last eviction pass.
    pub writable: AtomicBool,
}

impl App {
    pub fn new(cfg: config::Config) -> Result<App, String> {
        Ok(App {
            store: Arc::new(store::Store::new(cfg.data_dir.clone(), cfg.fsync)),
            index: index::Index::new(),
            auth: auth::Auth::new(&cfg)?,
            metrics: Arc::new(metrics::Metrics::default()),
            evict_wake: Notify::new(),
            writable: AtomicBool::new(false),
            cfg,
        })
    }

    /// What `/health` reports: index loaded and data directory writable.
    pub fn is_healthy(&self) -> bool {
        self.index.is_ready() && self.writable.load(Relaxed)
    }

    /// Whether an upload of `len` bytes could be kept at all, even with the
    /// cache otherwise empty.
    pub fn can_fit(&self, len: u64) -> bool {
        if self.cfg.max_size.is_some_and(|max| len > max) {
            return false;
        }
        let disk_total = self.metrics.disk_total.load(Relaxed);
        if disk_total == 0 {
            // Not measured yet.
            return true;
        }
        let reserve = self.cfg.min_free_space.map_or(0, |f| f.resolve(disk_total));
        len.saturating_add(reserve) <= disk_total
    }

    /// Startup scan: rebuilds the index from the files on disk. Blocking.
    pub fn scan(&self) {
        let started = Instant::now();
        let mut seen = 0u64;
        let result = self.store.scan(|key, size, mtime| {
            self.index.insert_if_absent(key, size, mtime);
            seen += 1;
            if seen.is_multiple_of(100_000) {
                log::info("startup scan in progress", &[("entries", seen.into())]);
            }
        });
        let elapsed = started.elapsed();
        match result {
            Ok(stats) => {
                self.metrics.invalid_files.store(stats.invalid, Relaxed);
                self.metrics
                    .scan_duration_micros
                    .store(elapsed.as_micros().max(1) as u64, Relaxed);
                self.index.set_ready(true);
                log::info(
                    "startup scan complete",
                    &[
                        ("entries", stats.entries.into()),
                        ("size_bytes", stats.bytes.into()),
                        ("invalid_files", stats.invalid.into()),
                        ("duration_ms", (elapsed.as_millis() as u64).into()),
                    ],
                );
                if stats.invalid > 0 {
                    log::warn(
                        "data directory contains files that are not cache entries; they are never served and never deleted",
                        &[("count", stats.invalid.into())],
                    );
                }
            }
            Err(e) => {
                // The index stays "not ready": reads keep going to disk,
                // eviction stays off, and /health reports 503.
                log::error("startup scan failed", &[("error", (&e.to_string()).into())]);
            }
        }
    }
}
