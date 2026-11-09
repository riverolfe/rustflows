//! Cheap fingerprint of a source tree: name/size/mtime of every file.
//!
//! A review's answer depends on the files, but the prompt never quotes them —
//! so without this the cache serves results for code that has since changed.
//! mtime+size, not content: a stat walk is orders of magnitude cheaper than
//! hashing every file, and good enough to invalidate on edit.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Fingerprint `root`.
///
/// `exts` narrows the fingerprint to files with those extensions. `None` — the
/// default — fingerprints **every** regular file.
///
/// The narrowing used to be the default, and the allowlist it used was a
/// guess at what code looks like. A repo whose language the list did not name
/// — shell, SQL, Terraform, Lua, Vue, anything — fingerprinted as *empty*, and
/// an empty fingerprint never moves, so the cache served a review of code that
/// had since changed. `Makefile` and `Dockerfile`, which have no extension at
/// all, never moved it either.
///
/// The churn an allowlist was there to avoid is handled by [`SKIP_DIRS`], which
/// is exact: `node_modules`, `target` and `.git` are skipped by name, and
/// everything else is counted whether or not it looks like code.
pub fn stat_signature(root: &Path, exts: Option<&[&str]>) -> String {
    let exts: Option<Vec<String>> = exts.map(|e| e.iter().map(|x| format!(".{x}")).collect());

    // The canonical root, so the fingerprint names the tree and not the way
    // the caller spelled it.
    let base = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    walk(&base, exts.as_deref(), &mut parts, &base, &mut seen);
    parts.sort(); // walk order is filesystem order; the key must not depend on it
    digest(&parts.join("|"))
}

fn walk(
    dir: &Path,
    exts: Option<&[String]>,
    parts: &mut Vec<String>,
    base: &Path,
    seen: &mut HashSet<PathBuf>,
) {
    // A directory already walked is not walked again: a symlink pointing at
    // an ancestor would otherwise recurse forever.
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !seen.insert(canonical) {
        return;
    }
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
        let file_type = entry.file_type();
        let is_dir = file_type.as_ref().map(|t| t.is_dir()).unwrap_or(false);

        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, exts, parts, base, seen);
            continue;
        }

        // A symlinked directory is code the reviewer can read, so it belongs
        // in the fingerprint — `read_dir` does not follow links, so without
        // this the tree behind the link contributed nothing and its changes
        // never moved the key. One that points *outside* the tree does not
        // belong: the confine hook denies reads through it, so it is not code
        // under review either.
        if file_type.as_ref().map(|t| t.is_symlink()).unwrap_or(false) {
            let Ok(target) = path.canonicalize() else {
                continue;
            };
            if target.is_dir() {
                if target.starts_with(base) {
                    walk(&target, exts, parts, base, seen);
                }
                continue;
            }
        }

        // The extension filter is opt-in. A file with no extension is code
        // too — `Makefile`, `Dockerfile`, `LICENSE` — so it is only skipped
        // when the caller asked for a narrowing.
        if let Some(list) = exts {
            let Some(suffix) = path.extension().map(|e| format!(".{}", e.to_string_lossy()))
            else {
                continue;
            };
            if !list.contains(&suffix) {
                continue;
            }
        }
        // Devices and fifos are not code; stat the target and let the metadata
        // mode decide. A symlinked file is stat'd through to its target, so
        // its content moves the key.
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

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_inside_the_tree_is_fingerprinted() {
        // `read_dir` does not follow links, so the tree behind a link used to
        // contribute nothing — and its changes never moved the key.
        let dir = std::env::temp_dir().join("rustflows-sign-link");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/a.rs"), "fn main() {}").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();

        let before = stat_signature(&dir, None);
        std::fs::write(dir.join("real/a.rs"), "fn main() { println!(); }").unwrap();
        assert_ne!(
            before,
            stat_signature(&dir, None),
            "a change behind a symlinked directory must move the signature"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_not_followed() {
        // The confine hook denies reads through it, so the tree behind it is
        // not code under review and must not decide the cache key.
        let outside = std::env::temp_dir().join("rustflows-sign-outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.rs"), "fn main() {}").unwrap();

        let dir = std::env::temp_dir().join("rustflows-sign-outside-link");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

        let before = stat_signature(&dir, None);
        std::fs::write(outside.join("a.rs"), "fn main() { println!(); }").unwrap();
        assert_eq!(
            before,
            stat_signature(&dir, None),
            "a link out of the tree must not decide the key"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_does_not_recurse_forever() {
        let dir = std::env::temp_dir().join("rustflows-sign-loop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/x.rs"), "x").unwrap();
        std::os::unix::fs::symlink(dir.join("a"), dir.join("a/loop")).unwrap();

        // It must terminate, and it must terminate with a signature.
        assert!(!stat_signature(&dir, None).is_empty());
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

    #[test]
    fn a_file_the_allowlist_did_not_name_still_moves_the_signature() {
        // The allowlist used to skip every file in a repo whose language it did
        // not name, so the signature never moved and the cache served a review
        // of code that had since changed.
        let dir = std::env::temp_dir().join("rustflows-sign-unlisted");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deploy.sh"), "echo hi\n").unwrap();

        let before = stat_signature(&dir, None);
        assert_ne!(before, digest(""), "a repo of shell scripts is not an empty repo");
        std::fs::write(dir.join("deploy.sh"), "echo there\n").unwrap();
        assert_ne!(before, stat_signature(&dir, None));

        // A file with no extension at all is code too.
        std::fs::write(dir.join("Makefile"), "all:\n\techo hi\n").unwrap();
        assert_ne!(stat_signature(&dir, None), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_extension_list_narrows_the_fingerprint() {
        let dir = std::env::temp_dir().join("rustflows-sign-narrow");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.sh"), "echo hi\n").unwrap();

        let only_rs = stat_signature(&dir, Some(&["rs"]));
        std::fs::write(dir.join("b.sh"), "echo there\n").unwrap();
        assert_eq!(
            only_rs,
            stat_signature(&dir, Some(&["rs"])),
            "a narrowed fingerprint must ignore the files it excludes"
        );
        assert_ne!(stat_signature(&dir, None), only_rs);
        let _ = std::fs::remove_dir_all(&dir);
    }
}