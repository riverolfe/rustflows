//! Cheap fingerprint of a source tree: name/size/mtime of matching files.
//!
//! A review's answer depends on the files, but the prompt never quotes them —
//! so without this the cache serves results for code that has since changed.
//! mtime+size, not content: a stat walk is orders of magnitude cheaper than
//! hashing every file, and good enough to invalidate on edit.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The extensions a review is likely to be pointed at.
///
/// Deliberately not "every file": node_modules and .git churn on every build and
/// would keep the cache permanently cold. `.yml`/`.yaml` because a pipeline is
/// code with the same review needs.
pub const DEFAULT_EXTS: &[&str] = &[
    "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "rs", "rb", "java", "kt", "swift", "c",
    "h", "cc", "cpp", "hpp", "cs", "php", "scala", "ex", "exs", "yml", "yaml",
];

/// Never fingerprinted: dependencies and build output, not the code under review.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
    "target",
    "vendor",
    ".next",
    ".nuxt",
    "coverage",
    // A sibling worktree's source is not the tree under review.
    ".worktrees",
];

/// Files modified this recently get their content hashed, not just stat'd: a
/// filesystem whose clock ticks coarser than this can stamp two writes alike.
/// 2s covers ext4's few-ms tick and FAT's 2s resolution.
const RACY_WINDOW_NS: u128 = 2_000_000_000;
const RACY_HASH_MAX_BYTES: u64 = 16 * 1024 * 1024;

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Fingerprint `root`; `exts` defaults to [`DEFAULT_EXTS`].
pub fn stat_signature(root: &Path, exts: Option<&[&str]>) -> String {
    let exts: Vec<String> = exts
        .unwrap_or(DEFAULT_EXTS)
        .iter()
        .map(|e| format!(".{}", e))
        .collect();

    let mut parts: Vec<String> = Vec::new();
    walk(root, &exts, &mut parts);
    parts.sort(); // walk order is filesystem order; the key must not depend on it
    digest(&parts.join("|"))
}

fn walk(dir: &Path, exts: &[String], parts: &mut Vec<String>) {
    // A directory we cannot list must appear in the fingerprint. Returning
    // silently made it indistinguishable from an empty one, so a file added
    // inside it never moved the cache key and `cache.get` served the review
    // from before it existed — exit 0, no error, a stale answer.
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            parts.push(format!("!unreadable:{}:{e}", dir.display()));
            return;
        }
    };
    let now = now_ns();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, exts, parts);
            continue;
        }

        let Some(suffix) = path.extension().map(|e| format!(".{}", e.to_string_lossy())) else {
            continue;
        };
        if !exts.contains(&suffix) {
            continue;
        }
        // Devices, fifos and symlinks are not code; stat the target and let the
        // metadata mode decide.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let mtime_ns = modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let mut part = format!("{}:{}:{}", path.display(), meta.len(), mtime_ns);
        if now.saturating_sub(mtime_ns) < RACY_WINDOW_NS && meta.len() <= RACY_HASH_MAX_BYTES
            && let Ok(bytes) = std::fs::read(&path) {
                part.push(':');
                part.push_str(&hex(&Sha256::digest(&bytes)));
            }
        parts.push(part);
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(blob: &str) -> String {
    hex(&Sha256::digest(blob.as_bytes()))[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_16_chars() {
        assert_eq!(digest("abc"), digest("abc"));
        assert_eq!(digest("abc").len(), 16);
        assert_ne!(digest("abc"), digest("abd"));
    }

    #[test]
    fn signature_changes_when_a_tracked_file_is_touched() {
        let dir = std::env::temp_dir().join("rustflows-sign-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "print(1)").unwrap();

        let before = stat_signature(&dir, None);
        // Within the racy window the content hash is included, so a same-size
        // rewrite must move the signature.
        std::fs::write(dir.join("a.py"), "print(2)").unwrap();
        let after = stat_signature(&dir, None);
        assert_ne!(before, after);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skipped_dirs_do_not_contribute() {
        let dir = std::env::temp_dir().join("rustflows-skip-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/dep.py"), "x = 1").unwrap();
        assert_eq!(stat_signature(&dir, None), digest(""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_directory_shows_up_in_the_fingerprint() {
        // Returning silently made a directory we cannot list identical to an
        // empty one, so files appearing inside it never invalidated the cache.
        let dir = std::env::temp_dir().join("rustflows-no-such-tree");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(
            stat_signature(&dir, None),
            digest(""),
            "an unreadable dir must not fingerprint as an empty one"
        );
    }
}