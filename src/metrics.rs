//! Counters, gauges and the Prometheus text exposition, written by hand.
//! All labels come from small fixed sets, so cardinality is bounded.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub const PREFIX: &str = "apps_infra_mobile_build_cache_";

const METHODS: [&str; 4] = ["GET", "HEAD", "PUT", "other"];
const STATUSES: [u16; 10] = [200, 400, 401, 403, 404, 405, 413, 429, 500, 503];
/// Upper bounds, in seconds, of the request duration buckets.
const BUCKETS: [f64; 13] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 30.0,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvictReason {
    Age = 0,
    Size = 1,
    FreeSpace = 2,
}
const EVICT_REASONS: [&str; 3] = ["age", "size", "free_space"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    TooLarge = 0,
    NoSpace = 1,
}
const REJECT_REASONS: [&str; 2] = ["too_large", "no_space"];

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        REJECT_REASONS[self as usize]
    }
}

#[derive(Default)]
struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        if let Some(i) = BUCKETS.iter().position(|b| secs <= *b) {
            self.buckets[i].fetch_add(1, Relaxed);
        }
        self.count.fetch_add(1, Relaxed);
        self.sum_micros.fetch_add(d.as_micros() as u64, Relaxed);
    }
}

#[derive(Default)]
pub struct Metrics {
    requests: [[AtomicU64; STATUSES.len() + 1]; METHODS.len()],
    durations: [Histogram; METHODS.len()],
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub bytes_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    evictions: [AtomicU64; 3],
    evicted_bytes: [AtomicU64; 3],
    pub eviction_errors: AtomicU64,
    pub eviction_duration_micros: AtomicU64,
    /// Unix time of the least recently used entry; 0 when the cache is empty.
    pub oldest_last_used: AtomicU64,
    rejected: [AtomicU64; 2],
    pub auth_failures: AtomicU64,
    pub auth_throttled: AtomicU64,
    pub auth_verifications: AtomicU64,
    pub auth_busy: AtomicU64,
    pub connections_active: AtomicI64,
    pub disk_free: AtomicU64,
    pub disk_total: AtomicU64,
    pub scan_duration_micros: AtomicU64,
    pub invalid_files: AtomicU64,
}

fn method_index(method: &str) -> usize {
    METHODS.iter().position(|m| *m == method).unwrap_or(3)
}

impl Metrics {
    pub fn request(&self, method: &str, status: u16, duration: Duration) {
        let m = method_index(method);
        let s = STATUSES
            .iter()
            .position(|s| *s == status)
            .unwrap_or(STATUSES.len());
        self.requests[m][s].fetch_add(1, Relaxed);
        self.durations[m].observe(duration);
    }

    pub fn evicted(&self, reason: EvictReason, bytes: u64) {
        self.evictions[reason as usize].fetch_add(1, Relaxed);
        self.evicted_bytes[reason as usize].fetch_add(bytes, Relaxed);
    }

    pub fn rejected_upload(&self, reason: RejectReason) {
        self.rejected[reason as usize].fetch_add(1, Relaxed);
    }

    pub fn rejected_uploads(&self, reason: RejectReason) -> u64 {
        self.rejected[reason as usize].load(Relaxed)
    }

    pub fn evictions(&self, reason: EvictReason) -> u64 {
        self.evictions[reason as usize].load(Relaxed)
    }

    /// Renders the Prometheus text format. Index figures and static limits are
    /// passed in because they live elsewhere.
    pub fn render(
        &self,
        entries: u64,
        size_bytes: u64,
        max_size: Option<u64>,
        healthy: bool,
    ) -> String {
        let mut o = String::with_capacity(8 * 1024);
        let micros = |a: &AtomicU64| a.load(Relaxed) as f64 / 1e6;

        head(
            &mut o,
            "requests_total",
            "counter",
            "Requests on the cache listener by method and status.",
        );
        for (m, method) in METHODS.iter().enumerate() {
            for (s, count) in self.requests[m].iter().enumerate() {
                let n = count.load(Relaxed);
                let code = STATUSES.get(s).copied();
                // Always expose the common series; rare ones appear once seen.
                if n == 0 && !(m < 3 && matches!(code, Some(200 | 404))) {
                    continue;
                }
                let status = code.map_or("other".to_string(), |c| c.to_string());
                let _ = writeln!(
                    o,
                    "{PREFIX}requests_total{{method=\"{method}\",status=\"{status}\"}} {n}"
                );
            }
        }

        head(
            &mut o,
            "request_duration_seconds",
            "histogram",
            "Time from receiving a request head to its response head being ready (uploads include the body transfer).",
        );
        for (m, method) in METHODS.iter().enumerate() {
            let h = &self.durations[m];
            let mut cumulative = 0;
            for (i, le) in BUCKETS.iter().enumerate() {
                cumulative += h.buckets[i].load(Relaxed);
                let _ = writeln!(
                    o,
                    "{PREFIX}request_duration_seconds_bucket{{method=\"{method}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let count = h.count.load(Relaxed);
            let _ = writeln!(
                o,
                "{PREFIX}request_duration_seconds_bucket{{method=\"{method}\",le=\"+Inf\"}} {count}"
            );
            let _ = writeln!(
                o,
                "{PREFIX}request_duration_seconds_sum{{method=\"{method}\"}} {}",
                micros(&h.sum_micros)
            );
            let _ = writeln!(
                o,
                "{PREFIX}request_duration_seconds_count{{method=\"{method}\"}} {count}"
            );
        }

        let simple = |o: &mut String, name: &str, kind: &str, help: &str, value: String| {
            head(o, name, kind, help);
            let _ = writeln!(o, "{PREFIX}{name} {value}");
        };
        let n = |a: &AtomicU64| a.load(Relaxed).to_string();

        simple(
            &mut o,
            "hits_total",
            "counter",
            "GET and HEAD requests answered from the cache.",
            n(&self.hits),
        );
        simple(
            &mut o,
            "misses_total",
            "counter",
            "GET and HEAD requests for entries not in the cache.",
            n(&self.misses),
        );
        simple(
            &mut o,
            "bytes_received_total",
            "counter",
            "Upload body bytes received.",
            n(&self.bytes_received),
        );
        simple(
            &mut o,
            "bytes_sent_total",
            "counter",
            "Entry body bytes sent.",
            n(&self.bytes_sent),
        );
        simple(
            &mut o,
            "entries",
            "gauge",
            "Entries in the cache.",
            entries.to_string(),
        );
        simple(
            &mut o,
            "size_bytes",
            "gauge",
            "Total size of all entries.",
            size_bytes.to_string(),
        );
        simple(
            &mut o,
            "max_size_bytes",
            "gauge",
            "Configured CACHE_MAX_SIZE; 0 when unset.",
            max_size.unwrap_or(0).to_string(),
        );
        simple(
            &mut o,
            "disk_free_bytes",
            "gauge",
            "Free space on the data volume at the last eviction pass.",
            n(&self.disk_free),
        );
        simple(
            &mut o,
            "disk_total_bytes",
            "gauge",
            "Size of the data volume.",
            n(&self.disk_total),
        );

        head(
            &mut o,
            "evictions_total",
            "counter",
            "Entries evicted, by reason.",
        );
        for (i, reason) in EVICT_REASONS.iter().enumerate() {
            let _ = writeln!(
                o,
                "{PREFIX}evictions_total{{reason=\"{reason}\"}} {}",
                self.evictions[i].load(Relaxed)
            );
        }
        head(
            &mut o,
            "evicted_bytes_total",
            "counter",
            "Bytes evicted, by reason.",
        );
        for (i, reason) in EVICT_REASONS.iter().enumerate() {
            let _ = writeln!(
                o,
                "{PREFIX}evicted_bytes_total{{reason=\"{reason}\"}} {}",
                self.evicted_bytes[i].load(Relaxed)
            );
        }
        simple(
            &mut o,
            "eviction_errors_total",
            "counter",
            "Entry files that could not be deleted during eviction.",
            n(&self.eviction_errors),
        );
        simple(
            &mut o,
            "eviction_duration_seconds",
            "gauge",
            "Duration of the last eviction pass.",
            micros(&self.eviction_duration_micros).to_string(),
        );
        simple(
            &mut o,
            "oldest_entry_last_used_seconds",
            "gauge",
            "Unix time at which the least recently used entry was last used; 0 when the cache is empty.",
            n(&self.oldest_last_used),
        );

        head(
            &mut o,
            "rejected_uploads_total",
            "counter",
            "Uploads answered 200 but not stored (soft rejects), by reason.",
        );
        for (i, reason) in REJECT_REASONS.iter().enumerate() {
            let _ = writeln!(
                o,
                "{PREFIX}rejected_uploads_total{{reason=\"{reason}\"}} {}",
                self.rejected[i].load(Relaxed)
            );
        }
        simple(
            &mut o,
            "auth_failures_total",
            "counter",
            "Requests with missing, malformed or wrong credentials.",
            n(&self.auth_failures),
        );
        simple(
            &mut o,
            "auth_verifications_total",
            "counter",
            "Password verifications run with argon2 (credentials not in the verification cache).",
            n(&self.auth_verifications),
        );
        simple(
            &mut o,
            "auth_throttled_total",
            "counter",
            "Requests answered 429 by the failed-attempt throttle.",
            n(&self.auth_throttled),
        );
        simple(
            &mut o,
            "auth_busy_total",
            "counter",
            "Requests answered 503 after waiting too long for a password verification slot.",
            n(&self.auth_busy),
        );
        simple(
            &mut o,
            "connections_active",
            "gauge",
            "Open connections on the cache listener.",
            self.connections_active.load(Relaxed).max(0).to_string(),
        );
        simple(
            &mut o,
            "startup_scan_duration_seconds",
            "gauge",
            "Duration of the startup index scan; 0 while it is still running.",
            micros(&self.scan_duration_micros).to_string(),
        );
        simple(
            &mut o,
            "invalid_files",
            "gauge",
            "Files in the data directory that are not valid entries; never served, never deleted.",
            n(&self.invalid_files),
        );
        simple(
            &mut o,
            "healthy",
            "gauge",
            "1 when /health answers 200.",
            u8::from(healthy).to_string(),
        );

        head(&mut o, "build_info", "gauge", "Build information.");
        let _ = writeln!(
            o,
            "{PREFIX}build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        o
    }
}

fn head(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {PREFIX}{name} {help}");
    let _ = writeln!(o, "# TYPE {PREFIX}{name} {kind}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_exposition() {
        let m = Metrics::default();
        m.request("GET", 200, Duration::from_micros(700));
        m.request("GET", 200, Duration::from_millis(3));
        m.request("GET", 404, Duration::from_micros(100));
        m.request("PUT", 200, Duration::from_secs(60));
        m.request("DELETE", 405, Duration::from_micros(10));
        m.request("GET", 418, Duration::from_micros(10));
        m.hits.fetch_add(2, Relaxed);
        m.evicted(EvictReason::Size, 1000);
        m.evicted(EvictReason::Size, 500);
        m.rejected_upload(RejectReason::TooLarge);
        m.eviction_duration_micros.store(2_500_000, Relaxed);
        let text = m.render(7, 12345, Some(1 << 30), true);

        let has = |line: &str| {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in:\n{text}"
            )
        };
        has("apps_infra_mobile_build_cache_requests_total{method=\"GET\",status=\"200\"} 2");
        has("apps_infra_mobile_build_cache_requests_total{method=\"GET\",status=\"404\"} 1");
        has("apps_infra_mobile_build_cache_requests_total{method=\"GET\",status=\"other\"} 1");
        has("apps_infra_mobile_build_cache_requests_total{method=\"other\",status=\"405\"} 1");
        has("apps_infra_mobile_build_cache_requests_total{method=\"HEAD\",status=\"200\"} 0");
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"GET\",le=\"0.0005\"} 2",
        );
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"GET\",le=\"0.001\"} 3",
        );
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"GET\",le=\"0.005\"} 4",
        );
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"GET\",le=\"+Inf\"} 4",
        );
        has("apps_infra_mobile_build_cache_request_duration_seconds_count{method=\"GET\"} 4");
        // Beyond the last bucket: only in +Inf.
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"PUT\",le=\"30\"} 0",
        );
        has(
            "apps_infra_mobile_build_cache_request_duration_seconds_bucket{method=\"PUT\",le=\"+Inf\"} 1",
        );
        has("apps_infra_mobile_build_cache_request_duration_seconds_sum{method=\"PUT\"} 60");
        has("apps_infra_mobile_build_cache_hits_total 2");
        has("apps_infra_mobile_build_cache_entries 7");
        has("apps_infra_mobile_build_cache_size_bytes 12345");
        has("apps_infra_mobile_build_cache_max_size_bytes 1073741824");
        has("apps_infra_mobile_build_cache_evictions_total{reason=\"size\"} 2");
        has("apps_infra_mobile_build_cache_evicted_bytes_total{reason=\"size\"} 1500");
        has("apps_infra_mobile_build_cache_evictions_total{reason=\"age\"} 0");
        has("apps_infra_mobile_build_cache_rejected_uploads_total{reason=\"too_large\"} 1");
        has("apps_infra_mobile_build_cache_rejected_uploads_total{reason=\"no_space\"} 0");
        has("apps_infra_mobile_build_cache_eviction_duration_seconds 2.5");
        has("apps_infra_mobile_build_cache_healthy 1");
        has(&format!(
            "apps_infra_mobile_build_cache_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        ));

        // Structure: every sample belongs to a metric announced by HELP and TYPE.
        let mut announced = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest.split(' ').next().unwrap();
                assert!(
                    announced.insert(name.to_string()),
                    "duplicate TYPE for {name}"
                );
            } else if !line.starts_with('#') {
                let name = line.split(['{', ' ']).next().unwrap();
                let base = name
                    .trim_end_matches("_bucket")
                    .trim_end_matches("_sum")
                    .trim_end_matches("_count");
                assert!(
                    announced.contains(name) || announced.contains(base),
                    "sample {name} has no TYPE line"
                );
                assert!(name.starts_with(PREFIX));
                let value = line.rsplit(' ').next().unwrap();
                assert!(value.parse::<f64>().is_ok(), "bad value in {line:?}");
            }
        }
    }
}
