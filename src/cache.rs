//! Disk cache: one JSON file per key under `<dir>/entries`.
//!
//! Writes go to a pid-tagged temp file and are renamed into place, so a reader
//! never sees a half-written entry and two concurrent runs writing the same key
//! both land a valid file. Cache failures print and continue — a cache that
//! cannot be written must never take down a run.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

pub struct Cache {
    pub dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn entry(&self, key: &str) -> PathBuf {
        self.dir.join("entries").join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str, label: &str) -> Option<Value> {
        let path = self.entry(key);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => Some(value),
                Err(e) => {
                    println!("  cache read failed [{label}]: {e}");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                println!("  cache read failed [{label}]: {e}");
                None
            }
        }
    }

    pub fn set(&self, key: &str, value: &Value, label: &str) {
        let entries = self.dir.join("entries");
        // Split, not a tuple match: one `else` covering both reported a
        // PermissionDenied on the cache directory as "not serialisable".
        let bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(e) => {
                println!("  cache write failed [{label}]: not serialisable: {e}");
                return;
            }
        };
        if let Err(e) = mkdir_private(&entries) {
            println!("  cache write failed [{label}]: {e}");
            return;
        }
        let tmp = entries.join(format!(
            ".{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = self.entry(key);
        if let Err(e) = write_private(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
            println!("  cache write failed [{label}]: {e}");
        }
    }
}

/// Create `dir` readable only by its owner.
///
/// Cache entries are the cleaned findings of a code review, whose `failure` and
/// verifier `reason` fields routinely quote the source lines under review.
/// `create_dir_all` used the 0777 & ~umask default, which on a shared machine
/// is world-readable.
pub fn mkdir_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Write `bytes` to `path`, readable only by its owner.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

/// `~/.cache/rustflows` unless `RUSTFLOWS_CACHE` says otherwise.
pub fn cache_dir() -> PathBuf {
    super::env_path("RUSTFLOWS_CACHE", "~/.cache/rustflows")
}

pub fn default_cache() -> Cache {
    Cache::new(cache_dir())
}

/// True when `child` resolves inside `root` (or `root` itself).
pub fn is_relative_to(child: &Path, root: &Path) -> bool {
    child == root || child.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_value() {
        let dir = std::env::temp_dir().join(format!("rustflows-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::new(dir.clone());
        assert!(cache.get("k", "l").is_none());
        cache.set("k", &serde_json::json!({"a": 1}), "l");
        assert_eq!(cache.get("k", "l"), Some(serde_json::json!({"a": 1})));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_corrupt_entries_miss() {
        let dir = std::env::temp_dir().join(format!("rustflows-cache-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::new(dir.clone());
        std::fs::create_dir_all(dir.join("entries")).unwrap();
        std::fs::write(dir.join("entries/broken.json"), "{not json").unwrap();
        assert!(cache.get("broken", "l").is_none());
        assert!(cache.get("absent", "l").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_to_includes_self() {
        assert!(is_relative_to(Path::new("/a/b"), Path::new("/a/b")));
        assert!(is_relative_to(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(!is_relative_to(Path::new("/a/bc"), Path::new("/a/b")));
    }
}