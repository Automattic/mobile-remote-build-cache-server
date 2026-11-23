//! Disk layout and file operations.
//!
//! ```text
//! {root}/ab/abcdef….artifact   one file per entry, sharded by the first two key characters
//! {root}/.tmp/                 in-progress uploads, same filesystem as the entries
//! ```
//!
//! This is the storage boundary: open for read, create for write, delete,
//! list. Everything here is blocking `std::fs`; callers run it on the blocking
//! thread pool. A validated key is the only client input that ever becomes
//! part of a path.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SUFFIX: &str = ".artifact";
const TMP_DIR: &str = ".tmp";
/// Upload bytes buffered in memory between writes to the temp file. Uploads
/// smaller than this reach the disk in a single blocking call.
const WRITE_BUFFER: usize = 256 * 1024;

/// `^[0-9a-f]{32,128}$` — the path traversal defence.
pub fn is_valid_key(key: &str) -> bool {
    (32..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn now_secs() -> u64 {
    unix_secs(SystemTime::now())
}

/// True for errors that mean the volume (or quota) is full.
pub fn is_no_space(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}

pub struct Opened {
    pub file: File,
    pub size: u64,
    pub mtime: u64,
    /// The first bytes of the entry, read in the same blocking call.
    pub first: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub entries: u64,
    pub bytes: u64,
    /// Files and directories that are not valid entries. Never served, never deleted.
    pub invalid: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct DiskSpace {
    pub free: u64,
    pub total: u64,
}

pub struct Store {
    root: PathBuf,
    tmp: PathBuf,
    fsync: bool,
}

impl Store {
    pub fn new(root: PathBuf, fsync: bool) -> Store {
        let tmp = root.join(TMP_DIR);
        Store { root, tmp, fsync }
    }

    /// Creates the directories and discards uploads left over from a previous run.
    pub fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.tmp)?;
        for entry in fs::read_dir(&self.tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        debug_assert!(is_valid_key(key));
        let mut name = String::with_capacity(key.len() + SUFFIX.len());
        name.push_str(key);
        name.push_str(SUFFIX);
        self.root.join(&key[..2]).join(name)
    }

    /// Opens an entry and reads up to `first_len` bytes of it. With
    /// `set_mtime`, also persists the time of last use to the file.
    pub fn open_read(
        &self,
        key: &str,
        first_len: usize,
        set_mtime: Option<SystemTime>,
    ) -> io::Result<Opened> {
        let mut file = File::open(self.entry_path(key))?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::ErrorKind::NotFound.into());
        }
        let size = meta.len();
        let mtime = match set_mtime {
            // Best effort: failing to record last use must not fail the read.
            Some(t) if file.set_modified(t).is_ok() => unix_secs(t),
            _ => meta.modified().map(unix_secs).unwrap_or(0),
        };
        let mut first = vec![0; first_len.min(size.min(usize::MAX as u64) as usize)];
        file.read_exact(&mut first)?;
        Ok(Opened {
            file,
            size,
            mtime,
            first,
        })
    }

    pub fn delete(&self, key: &str) -> io::Result<()> {
        fs::remove_file(self.entry_path(key))
    }

    /// Walks the shard directories, calling `on_entry(key, size, mtime)` for
    /// every valid entry. Entries may appear and vanish while this runs.
    pub fn scan(&self, mut on_entry: impl FnMut(&str, u64, u64)) -> io::Result<ScanStats> {
        let mut stats = ScanStats::default();
        for dir in fs::read_dir(&self.root)? {
            let dir = dir?;
            let dir_name = dir.file_name();
            let Some(dir_name) = dir_name.to_str() else {
                stats.invalid += 1;
                continue;
            };
            if dir_name == TMP_DIR {
                continue;
            }
            let is_shard = dir_name.len() == 2
                && dir_name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && dir.file_type().is_ok_and(|t| t.is_dir());
            if !is_shard {
                stats.invalid += 1;
                continue;
            }
            for file in fs::read_dir(dir.path())? {
                let file = file?;
                let name = file.file_name();
                let key = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(SUFFIX))
                    .filter(|k| is_valid_key(k) && k.starts_with(dir_name));
                let Some(key) = key else {
                    stats.invalid += 1;
                    continue;
                };
                match file.metadata() {
                    Ok(meta) if meta.is_file() => {
                        stats.entries += 1;
                        stats.bytes += meta.len();
                        on_entry(key, meta.len(), meta.modified().map(unix_secs).unwrap_or(0));
                    }
                    Ok(_) => stats.invalid += 1,
                    // Evicted or replaced between listing and stat.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(stats)
    }

    /// Free and total bytes of the filesystem holding the data directory, as
    /// available to an unprivileged process.
    pub fn disk_space(&self) -> io::Result<DiskSpace> {
        let s = rustix::fs::statvfs(&self.root)?;
        Ok(DiskSpace {
            free: s.f_bavail.saturating_mul(s.f_frsize),
            total: s.f_blocks.saturating_mul(s.f_frsize),
        })
    }

    /// Checks that the data directory accepts writes (used by `/health`).
    pub fn probe_writable(&self) -> io::Result<()> {
        let path = self.tmp.join(format!("probe-{}", random_name()?));
        let result = OpenOptions::new().write(true).create_new(true).open(&path);
        let _ = fs::remove_file(&path);
        result.map(|_| ())
    }

    pub fn begin_upload(self: &Arc<Self>) -> io::Result<Upload> {
        Ok(Upload {
            path: self.tmp.join(random_name()?),
            store: self.clone(),
            file: None,
            buf: Vec::new(),
            written: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
            may_exist: false,
            finished: false,
        })
    }

    /// Syncs, closes and atomically publishes a finished temp file.
    fn publish(&self, file: File, tmp: &PathBuf, key: &str) -> io::Result<()> {
        if self.fsync {
            // Entries are compressed archives: a truncated one served after a
            // crash fails a build instead of just missing.
            file.sync_all()?;
        }
        drop(file);
        let dest = self.entry_path(key);
        match fs::rename(tmp, &dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(shard) = dest.parent() {
                    fs::create_dir_all(shard)?;
                }
                fs::rename(tmp, &dest)
            }
            other => other,
        }
    }
}

fn random_name() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let mut name = String::with_capacity(32);
    for b in bytes {
        name.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        name.push(char::from_digit((b & 15) as u32, 16).unwrap_or('0'));
    }
    Ok(name)
}

fn join_error(e: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("storage task failed: {e}"))
}

/// An in-progress upload: a uniquely named temp file that is either published
/// with [`Upload::commit`] or removed when the value is dropped, including
/// when the request is abandoned because the client disconnected.
pub struct Upload {
    store: Arc<Store>,
    path: PathBuf,
    file: Option<File>,
    buf: Vec<u8>,
    written: u64,
    /// Tells a still-running blocking write to remove the file it created.
    cancelled: Arc<AtomicBool>,
    may_exist: bool,
    finished: bool,
}

impl Upload {
    pub fn written(&self) -> u64 {
        self.written
    }

    pub async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        self.written += data.len() as u64;
        if self.buf.len() < WRITE_BUFFER {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.buf);
        let file = self.file.take();
        let path = self.path.clone();
        let cancelled = self.cancelled.clone();
        self.may_exist = true;
        let (file, buf) = tokio::task::spawn_blocking(move || {
            let result = write_chunk(file, &path, &buf);
            if cancelled.load(Relaxed) {
                let _ = fs::remove_file(&path);
            }
            result.map(|file| (file, buf))
        })
        .await
        .map_err(join_error)??;
        self.file = Some(file);
        self.buf = buf;
        self.buf.clear();
        Ok(())
    }

    /// Writes what is still buffered, syncs, and renames the file into place.
    /// `on_published` runs right after the rename, on the blocking thread, so
    /// the index learns about the entry even if the request is dropped while
    /// this is in flight. Returns the entry size.
    pub async fn commit(
        mut self,
        key: &str,
        on_published: impl FnOnce(u64) + Send + 'static,
    ) -> io::Result<u64> {
        // From here the blocking task owns cleanup.
        self.finished = true;
        let buf = std::mem::take(&mut self.buf);
        let file = self.file.take();
        let path = self.path.clone();
        let store = self.store.clone();
        let size = self.written;
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let result =
                write_chunk(file, &path, &buf).and_then(|file| store.publish(file, &path, &key));
            match result {
                Ok(()) => {
                    on_published(size);
                    Ok(size)
                }
                Err(e) => {
                    let _ = fs::remove_file(&path);
                    Err(e)
                }
            }
        })
        .await
        .map_err(join_error)?
    }
}

fn write_chunk(file: Option<File>, path: &PathBuf, buf: &[u8]) -> io::Result<File> {
    let mut file = match file {
        Some(file) => file,
        None => OpenOptions::new().write(true).create_new(true).open(path)?,
    };
    file.write_all(buf)?;
    Ok(file)
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled.store(true, Relaxed);
            if self.may_exist {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Sets a file's modification time; used by tests and tools to age entries.
pub fn set_entry_mtime(root: &std::path::Path, key: &str, secs: u64) -> io::Result<()> {
    if !is_valid_key(key) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let path = root.join(&key[..2]).join(format!("{key}{SUFFIX}"));
    File::options()
        .write(true)
        .open(path)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        let ok = |k: &str| assert!(is_valid_key(k), "{k:?} should be valid");
        let bad = |k: &str| assert!(!is_valid_key(k), "{k:?} should be invalid");
        ok("0123456789abcdef0123456789abcdef");
        ok(&"a".repeat(40));
        ok(&"f".repeat(64));
        ok(&"0".repeat(128));
        bad("");
        bad("abc");
        bad(&"a".repeat(31));
        bad(&"a".repeat(129));
        bad(&"a".repeat(100_000));
        // Uppercase could collide on a case-insensitive filesystem.
        bad("0123456789ABCDEF0123456789ABCDEF");
        bad(&"g".repeat(32));
        bad(&"z".repeat(32));
        // Traversal attempts, raw and percent-encoded.
        bad("../../../../../../../../etc/passwd");
        bad("..%2f..%2f..%2f..%2f..%2fetc%2fpasswd");
        bad("0123456789abcdef0123456789abcde/");
        bad("0123456789abcdef/123456789abcdef0");
        bad("0123456789abcdef%2f23456789abcdef");
        bad("0123456789abcdef\\123456789abcdef");
        bad("0123456789abcdef.123456789abcdef");
        bad("................................");
        bad("0123456789abcdef0123456789abcdef.artifact");
        bad("0123456789abcdef0123456789abcde\0");
        bad("0123456789abcdef0123456789abcde\n");
        bad(" 123456789abcdef0123456789abcdef");
        bad("0123456789abcdef0123456789abcd-_");
        // Non-ASCII digits and letters.
        bad("٠١٢٣٤٥٦٧٨٩abcdef0123456789abcdef");
        bad(&"а".repeat(32)); // Cyrillic
    }

    #[test]
    fn no_space_detection() {
        assert!(is_no_space(&io::ErrorKind::StorageFull.into()));
        assert!(is_no_space(&io::ErrorKind::QuotaExceeded.into()));
        assert!(!is_no_space(&io::ErrorKind::PermissionDenied.into()));
    }
}
