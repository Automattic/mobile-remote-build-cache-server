//! In-memory index: key to size and time of last use.
//!
//! The map is split into shards, each behind its own mutex, so hot-path
//! updates from concurrent requests do not contend on one lock.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

const SHARDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub size: u64,
    /// Unix seconds of the last hit (or of the upload).
    pub last_used: u64,
    /// The last-use time most recently written to the file's mtime.
    pub persisted: u64,
}

/// Result of recording a hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    /// The key is not in the index.
    Missing,
    /// Recorded in memory; the on-disk time is recent enough.
    Fresh,
    /// Recorded in memory; the caller should also update the file's mtime.
    Persist,
}

/// One row of an eviction snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Snapshot {
    pub last_used: u64,
    pub key: Box<str>,
    pub size: u64,
}

pub struct Index {
    shards: Vec<Mutex<HashMap<Box<str>, Entry>>>,
    bytes: AtomicU64,
    entries: AtomicU64,
    ready: AtomicBool,
}

impl Default for Index {
    fn default() -> Self {
        Index::new()
    }
}

impl Index {
    pub fn new() -> Index {
        Index {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            ready: AtomicBool::new(false),
        }
    }

    fn shard(&self, key: &str) -> std::sync::MutexGuard<'_, HashMap<Box<str>, Entry>> {
        // Keys are hashes, so their leading bytes are uniformly distributed.
        let b = key.as_bytes();
        let h = (b.first().copied().unwrap_or(0) as usize) * 31
            + b.get(1).copied().unwrap_or(0) as usize;
        // A poisoned lock only means another thread panicked mid-update of
        // plain data; the map itself is still usable.
        self.shards[h % SHARDS]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// True once the startup scan has finished: from then on a key missing
    /// from the index is a miss without touching the disk.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Relaxed)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Relaxed);
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes.load(Relaxed)
    }

    pub fn len(&self) -> u64 {
        self.entries.load(Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, key: &str) -> Option<Entry> {
        self.shard(key).get(key).copied()
    }

    /// Records a completed upload, replacing any previous entry.
    pub fn insert(&self, key: &str, size: u64, now: u64) {
        let entry = Entry {
            size,
            last_used: now,
            persisted: now,
        };
        let old = self.shard(key).insert(key.into(), entry);
        self.bytes.fetch_add(size, Relaxed);
        match old {
            Some(old) => {
                self.bytes.fetch_sub(old.size, Relaxed);
            }
            None => {
                self.entries.fetch_add(1, Relaxed);
            }
        }
    }

    /// Records an entry found on disk (startup scan, or a read that got there
    /// before the scan). Never overwrites a record that is already present,
    /// because that one is at least as recent.
    pub fn insert_if_absent(&self, key: &str, size: u64, mtime: u64) {
        let mut shard = self.shard(key);
        if !shard.contains_key(key) {
            shard.insert(
                key.into(),
                Entry {
                    size,
                    last_used: mtime,
                    persisted: mtime,
                },
            );
            drop(shard);
            self.bytes.fetch_add(size, Relaxed);
            self.entries.fetch_add(1, Relaxed);
        }
    }

    /// Records a hit. Returns `Persist` at most once per `interval` per entry,
    /// which bounds metadata writes.
    pub fn touch(&self, key: &str, now: u64, interval: u64) -> Touch {
        let mut shard = self.shard(key);
        match shard.get_mut(key) {
            None => Touch::Missing,
            Some(e) => {
                e.last_used = e.last_used.max(now);
                if now >= e.persisted.saturating_add(interval) {
                    e.persisted = now;
                    Touch::Persist
                } else {
                    Touch::Fresh
                }
            }
        }
    }

    pub fn remove(&self, key: &str) -> Option<Entry> {
        let old = self.shard(key).remove(key);
        if let Some(old) = old {
            self.bytes.fetch_sub(old.size, Relaxed);
            self.entries.fetch_sub(1, Relaxed);
        }
        old
    }

    /// Removes the entry only if it has not been used or replaced since the
    /// snapshot that selected it for eviction.
    pub fn remove_if_unused_since(&self, key: &str, last_used: u64) -> Option<Entry> {
        let mut shard = self.shard(key);
        match shard.get(key) {
            Some(e) if e.last_used == last_used => {
                let old = shard.remove(key);
                drop(shard);
                if let Some(old) = old {
                    self.bytes.fetch_sub(old.size, Relaxed);
                    self.entries.fetch_sub(1, Relaxed);
                }
                old
            }
            _ => None,
        }
    }

    /// The smallest `last_used` in the index.
    pub fn oldest_last_used(&self) -> Option<u64> {
        let mut oldest = None;
        for shard in &self.shards {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(min) = shard.values().map(|e| e.last_used).min() {
                oldest = Some(oldest.map_or(min, |o: u64| o.min(min)));
            }
        }
        oldest
    }

    /// Copies every entry out, holding each shard lock only briefly.
    pub fn snapshot(&self) -> Vec<Snapshot> {
        let mut out = Vec::with_capacity(self.len() as usize);
        for shard in &self.shards {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            out.extend(shard.iter().map(|(key, e)| Snapshot {
                last_used: e.last_used,
                key: key.clone(),
                size: e.size,
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn totals_follow_inserts_replacements_and_removals() {
        let ix = Index::new();
        ix.insert(A, 100, 10);
        ix.insert(B, 50, 11);
        assert_eq!((ix.len(), ix.total_bytes()), (2, 150));
        ix.insert(A, 30, 12);
        assert_eq!((ix.len(), ix.total_bytes()), (2, 80));
        assert_eq!(ix.remove(A).unwrap().size, 30);
        assert_eq!(ix.remove(A), None);
        assert_eq!((ix.len(), ix.total_bytes()), (1, 50));
    }

    #[test]
    fn insert_if_absent_keeps_newer_record() {
        let ix = Index::new();
        ix.insert(A, 100, 500);
        ix.insert_if_absent(A, 999, 1);
        assert_eq!(
            ix.get(A).unwrap(),
            Entry {
                size: 100,
                last_used: 500,
                persisted: 500
            }
        );
        ix.insert_if_absent(B, 7, 3);
        assert_eq!((ix.len(), ix.total_bytes()), (2, 107));
    }

    #[test]
    fn touch_persists_at_most_once_per_interval() {
        let ix = Index::new();
        assert_eq!(ix.touch(A, 5, 3600), Touch::Missing);
        ix.insert_if_absent(A, 1, 1000);
        assert_eq!(ix.touch(A, 1100, 3600), Touch::Fresh);
        assert_eq!(ix.get(A).unwrap().last_used, 1100);
        assert_eq!(ix.touch(A, 4600, 3600), Touch::Persist);
        assert_eq!(ix.touch(A, 4601, 3600), Touch::Fresh);
        assert_eq!(ix.touch(A, 8199, 3600), Touch::Fresh);
        assert_eq!(ix.touch(A, 8200, 3600), Touch::Persist);
        // A clock that steps backwards never lowers last_used.
        assert_eq!(ix.touch(A, 10, 3600), Touch::Fresh);
        assert_eq!(ix.get(A).unwrap().last_used, 8200);
    }

    #[test]
    fn conditional_remove_spares_entries_used_since_snapshot() {
        let ix = Index::new();
        ix.insert(A, 10, 100);
        ix.insert(B, 20, 100);
        ix.touch(A, 200, 3600);
        assert_eq!(ix.remove_if_unused_since(A, 100), None);
        assert!(ix.remove_if_unused_since(B, 100).is_some());
        assert_eq!((ix.len(), ix.total_bytes()), (1, 10));
    }

    #[test]
    fn snapshot_and_oldest() {
        let ix = Index::new();
        assert_eq!(ix.oldest_last_used(), None);
        ix.insert(A, 10, 300);
        ix.insert(B, 20, 100);
        assert_eq!(ix.oldest_last_used(), Some(100));
        let mut snap = ix.snapshot();
        snap.sort();
        assert_eq!(snap[0].key.as_ref(), B);
        assert_eq!(snap[1].size, 10);
    }
}
