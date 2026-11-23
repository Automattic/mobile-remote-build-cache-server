//! Eviction policy and background task.
//!
//! Each pass applies three rules in order: entries unused for longer than the
//! maximum age, least recently used entries while the cache is over its size
//! limit, and least recently used entries while the volume is short of free
//! space. The policy itself ([`plan`]) is a pure function of a snapshot, so it
//! is tested with a fake clock.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use crate::App;
use crate::index::Snapshot;
use crate::log;
use crate::metrics::EvictReason;
use crate::store::now_secs;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_size: Option<u64>,
    /// Size eviction stops once the total is at or below this fraction of `max_size`.
    pub low_watermark: f64,
    /// Seconds an entry may go unused.
    pub max_age: Option<u64>,
    /// Bytes to keep free on the volume.
    pub min_free: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Victim {
    pub key: Box<str>,
    pub size: u64,
    pub last_used: u64,
    pub reason: EvictReason,
}

/// Chooses what to delete. `total` is the sum of all entry sizes in the
/// snapshot, `disk_free` the volume's current free bytes if known.
pub fn plan(
    mut snapshot: Vec<Snapshot>,
    total: u64,
    now: u64,
    limits: &Limits,
    disk_free: Option<u64>,
) -> Vec<Victim> {
    // Oldest first; the key breaks ties so the order is deterministic.
    snapshot.sort_unstable();
    let mut victims = Vec::new();
    let mut total = total;
    let mut freed = 0u64;
    let mut entries = snapshot.into_iter().peekable();
    let mut take = |entries: &mut std::iter::Peekable<std::vec::IntoIter<Snapshot>>,
                    reason: EvictReason,
                    total: &mut u64,
                    freed: &mut u64| {
        if let Some(e) = entries.next() {
            *total = total.saturating_sub(e.size);
            *freed += e.size;
            victims.push(Victim {
                key: e.key,
                size: e.size,
                last_used: e.last_used,
                reason,
            });
        }
    };

    if let Some(max_age) = limits.max_age {
        let cutoff = now.saturating_sub(max_age);
        while entries.peek().is_some_and(|e| e.last_used < cutoff) {
            take(&mut entries, EvictReason::Age, &mut total, &mut freed);
        }
    }

    if let Some(max_size) = limits.max_size
        && total > max_size
    {
        let target = (max_size as f64 * limits.low_watermark) as u64;
        while total > target && entries.peek().is_some() {
            take(&mut entries, EvictReason::Size, &mut total, &mut freed);
        }
    }

    if let (Some(min_free), Some(disk_free)) = (limits.min_free, disk_free)
        && disk_free.saturating_add(freed) < min_free
    {
        // Same hysteresis as the size rule, so a full volume does not trigger
        // an eviction pass on every upload.
        let margin = (total as f64 * (1.0 - limits.low_watermark)) as u64;
        let target = min_free.saturating_add(margin);
        while disk_free.saturating_add(freed) < target && entries.peek().is_some() {
            take(&mut entries, EvictReason::FreeSpace, &mut total, &mut freed);
        }
    }
    victims
}

/// One eviction pass. Blocking: run it on the blocking pool.
pub fn run_pass(app: &App) {
    let started = Instant::now();
    let cfg = &app.cfg;

    // Refresh the cached health and disk figures that `/health`, `/metrics`
    // and the upload acceptance check read.
    let writable = match app.store.probe_writable() {
        Ok(()) => true,
        Err(e) => {
            log::error(
                "data directory is not writable",
                &[("error", (&e.to_string()).into())],
            );
            false
        }
    };
    app.writable.store(writable, Relaxed);
    let disk = match app.store.disk_space() {
        Ok(d) => {
            app.metrics.disk_free.store(d.free, Relaxed);
            app.metrics.disk_total.store(d.total, Relaxed);
            Some(d)
        }
        Err(e) => {
            log::error(
                "cannot read filesystem statistics",
                &[("error", (&e.to_string()).into())],
            );
            None
        }
    };

    // Until the startup scan finishes the index is incomplete: evicting from
    // it would pick the wrong victims.
    if !app.index.is_ready() {
        return;
    }

    let now = now_secs();
    let limits = Limits {
        max_size: cfg.max_size,
        low_watermark: cfg.eviction_low_watermark,
        max_age: cfg.max_age.map(|d| d.as_secs()),
        min_free: match (cfg.min_free_space, disk) {
            (Some(f), Some(d)) => Some(f.resolve(d.total)),
            _ => None,
        },
    };
    let oldest = app.index.oldest_last_used();
    app.metrics
        .oldest_last_used
        .store(oldest.unwrap_or(0), Relaxed);
    let total = app.index.total_bytes();
    let over_size = limits.max_size.is_some_and(|m| total > m);
    let over_age = match (limits.max_age, oldest) {
        (Some(age), Some(oldest)) => oldest < now.saturating_sub(age),
        _ => false,
    };
    let low_space = match (limits.min_free, disk) {
        (Some(min), Some(d)) => d.free < min,
        _ => false,
    };
    if !(over_size || over_age || low_space) {
        app.metrics
            .eviction_duration_micros
            .store(started.elapsed().as_micros() as u64, Relaxed);
        return;
    }

    let victims = plan(
        app.index.snapshot(),
        total,
        now,
        &limits,
        disk.map(|d| d.free),
    );
    let (mut count, mut bytes, mut failed) = (0u64, 0u64, 0u64);
    for v in victims {
        // Index first, then the file. An entry read or replaced since the
        // snapshot is spared. A reader that already has the file open keeps
        // streaming: the handle outlives the unlink.
        if app
            .index
            .remove_if_unused_since(&v.key, v.last_used)
            .is_none()
        {
            continue;
        }
        match app.store.delete(&v.key) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                failed += 1;
                app.metrics.eviction_errors.fetch_add(1, Relaxed);
                if failed <= 5 {
                    log::error(
                        "cannot delete evicted entry",
                        &[
                            ("key", v.key.as_ref().into()),
                            ("error", (&e.to_string()).into()),
                        ],
                    );
                }
                continue;
            }
        }
        app.metrics.evicted(v.reason, v.size);
        count += 1;
        bytes += v.size;
    }
    app.metrics
        .oldest_last_used
        .store(app.index.oldest_last_used().unwrap_or(0), Relaxed);
    let elapsed = started.elapsed();
    app.metrics
        .eviction_duration_micros
        .store(elapsed.as_micros() as u64, Relaxed);
    if count > 0 || failed > 0 {
        log::info(
            "eviction pass",
            &[
                ("evicted", count.into()),
                ("evicted_bytes", bytes.into()),
                ("failed", failed.into()),
                ("entries", app.index.len().into()),
                ("size_bytes", app.index.total_bytes().into()),
                ("duration_ms", (elapsed.as_millis() as u64).into()),
            ],
        );
    }
}

/// Runs a pass every `CACHE_EVICTION_INTERVAL`, and sooner when woken because
/// an upload pushed the cache over a limit.
pub async fn run(app: Arc<App>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        let pass_app = app.clone();
        if tokio::task::spawn_blocking(move || run_pass(&pass_app))
            .await
            .is_err()
        {
            log::error("eviction pass failed", &[]);
        }
        tokio::select! {
            _ = tokio::time::sleep(app.cfg.eviction_interval) => {}
            _ = app.evict_wake.notified() => {}
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> Box<str> {
        format!("{n:032x}").into()
    }

    /// Entries `(id, last_used, size)`.
    fn snap(entries: &[(u32, u64, u64)]) -> (Vec<Snapshot>, u64) {
        let total = entries.iter().map(|e| e.2).sum();
        let mut v: Vec<Snapshot> = entries
            .iter()
            .map(|&(id, last_used, size)| Snapshot {
                last_used,
                key: key(id),
                size,
            })
            .collect();
        // The policy must not depend on snapshot order.
        v.reverse();
        (v, total)
    }

    fn ids(victims: &[Victim]) -> Vec<(u32, EvictReason)> {
        victims
            .iter()
            .map(|v| (u32::from_str_radix(&v.key, 16).unwrap(), v.reason))
            .collect()
    }

    const NONE: Limits = Limits {
        max_size: None,
        low_watermark: 0.98,
        max_age: None,
        min_free: None,
    };
    use EvictReason::{Age, FreeSpace, Size};

    #[test]
    fn nothing_to_do_within_limits() {
        let (s, total) = snap(&[(1, 100, 10), (2, 200, 10)]);
        let limits = Limits {
            max_size: Some(20),
            max_age: Some(1000),
            min_free: Some(50),
            ..NONE
        };
        assert!(plan(s, total, 1000, &limits, Some(50)).is_empty());
    }

    #[test]
    fn age_removes_everything_older_than_the_cutoff() {
        // Fake clock: now = 10_000, max age 1000, so the cutoff is 9000.
        let (s, total) = snap(&[(1, 8_000, 5), (2, 8_999, 5), (3, 9_000, 5), (4, 9_999, 5)]);
        let limits = Limits {
            max_age: Some(1000),
            ..NONE
        };
        assert_eq!(
            ids(&plan(s, total, 10_000, &limits, None)),
            [(1, Age), (2, Age)]
        );
        // The same entries one hour later: all expired.
        let (s, total) = snap(&[(1, 8_000, 5), (2, 8_999, 5), (3, 9_000, 5), (4, 9_999, 5)]);
        assert_eq!(plan(s, total, 13_600, &limits, None).len(), 4);
    }

    #[test]
    fn size_evicts_least_recently_used_down_to_the_low_watermark() {
        // 10 entries of 100 bytes, limit 950, watermark 80% = 760.
        let entries: Vec<(u32, u64, u64)> = (1..=10).map(|i| (i, 1000 - i as u64, 100)).collect();
        let (s, total) = snap(&entries);
        let limits = Limits {
            max_size: Some(950),
            low_watermark: 0.8,
            ..NONE
        };
        // Highest id is the least recently used; three go to reach 700 <= 760.
        assert_eq!(
            ids(&plan(s, total, 2000, &limits, None)),
            [(10, Size), (9, Size), (8, Size)]
        );
    }

    #[test]
    fn size_at_the_limit_is_not_over_it() {
        let (s, total) = snap(&[(1, 1, 500), (2, 2, 500)]);
        let limits = Limits {
            max_size: Some(1000),
            ..NONE
        };
        assert!(plan(s, total, 10, &limits, None).is_empty());
    }

    #[test]
    fn free_space_evicts_until_restored() {
        let (s, total) = snap(&[(1, 10, 300), (2, 20, 300), (3, 30, 300), (4, 40, 300)]);
        let limits = Limits {
            min_free: Some(1000),
            low_watermark: 1.0,
            ..NONE
        };
        // 500 free, 1000 wanted: two entries of 300 bring it to 1100.
        assert_eq!(
            ids(&plan(s.clone(), total, 100, &limits, Some(500))),
            [(1, FreeSpace), (2, FreeSpace)]
        );
        // Unknown disk state: the rule is skipped rather than guessed.
        assert!(plan(s.clone(), total, 100, &limits, None).is_empty());
        // More missing than the cache holds: everything goes, and it terminates.
        assert_eq!(plan(s, total, 100, &limits, Some(0)).len(), 4);
    }

    #[test]
    fn rules_apply_in_order_and_share_freed_bytes() {
        let (s, total) = snap(&[
            (1, 100, 400), // expired
            (2, 5_000, 400),
            (3, 6_000, 400),
            (4, 7_000, 400),
            (5, 8_000, 400),
        ]);
        let limits = Limits {
            max_age: Some(9_000),
            max_size: Some(1_500),
            low_watermark: 1.0,
            min_free: Some(1_000),
        };
        // now = 10_000: entry 1 expires (total 1600, still over 1500), entry 2
        // goes for size (total 1200). Free space was 0, 800 freed so far, so
        // one more goes to reach 1000.
        assert_eq!(
            ids(&plan(s, total, 10_000, &limits, Some(0))),
            [(1, Age), (2, Size), (3, FreeSpace)]
        );
    }

    #[test]
    fn ties_are_broken_by_key() {
        let (s, total) = snap(&[(3, 50, 10), (1, 50, 10), (2, 50, 10)]);
        let limits = Limits {
            max_size: Some(25),
            low_watermark: 1.0,
            ..NONE
        };
        assert_eq!(ids(&plan(s, total, 100, &limits, None)), [(1, Size)]);
    }
}
