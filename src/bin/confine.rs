//! PreToolUse hook: deny Read/Grep/Glob on any path outside the roots given as
//! arguments. The first argument is the working directory; the rest are extra
//! read-only roots granted by the caller.
//!
//! The boundary cwd alone is not. A bare "Read" in allowed_tools approves the
//! whole tool before any permission callback is consulted, so absolute paths
//! walked straight out. A PreToolUse hook runs before that approval.
//!
//! Reads a hook event on stdin; writes `{}` to allow, or a deny decision with a
//! reason. The CLI runs this as its own subprocess.

use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

/// The tools that take a filesystem path, and the input fields holding one.
const PATH_TOOLS: &[(&str, &[&str])] = &[
    ("Read", &["file_path"]),
    ("Grep", &["path"]),
    ("Glob", &["path", "pattern"]),
];

fn main() {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        // Fail closed. An unreadable event used to answer `{}` — allow — and a
        // confinement hook that cannot read its input has no business granting
        // anything.
        deny("could not read the hook event from stdin");
        return;
    }

    // Resolve the roots the same way the file paths are resolved, or a root
    // reached through a symlink (/tmp -> /private/tmp) matches nothing.
    let roots: Vec<PathBuf> = std::env::args()
        .skip(1)
        .map(|a| resolve_lenient(&expanduser(Path::new(&a))))
        .collect();
    let Some(root) = roots.first().cloned() else {
        deny("the confine hook was started with no working directory to confine to");
        return;
    };

    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        deny("the hook event was not valid JSON");
        return;
    };
    let Some(tool) = event.get("tool_name").and_then(Value::as_str) else {
        deny("the hook event carried no tool_name");
        return;
    };
    if tool.is_empty() {
        deny("the hook event named no tool, so there was nothing to confine");
        return;
    }
    // A tool that takes no filesystem path has nothing to confine.
    if !PATH_TOOLS.iter().any(|(name, _)| *name == tool) {
        println!("{}", json!({}));
        return;
    }
    // From here the tool *does* take a path, so a `tool_input` that is missing
    // or is not an object means no path was ever checked. Allow would be a
    // read the boundary never looked at.
    let Some(fields) = event.get("tool_input").and_then(Value::as_object) else {
        deny(&format!(
            "{tool} was called with no readable tool_input, so its path could not be checked"
        ));
        return;
    };

    match escapes(&root, tool, &Value::Object(fields.clone()), &roots[1..]) {
        None => println!("{}", json!({})),
        Some(path) => deny(&format!(
            "{tool} on {path} is outside the working directory {}; only files under it may be read.",
            root.display()
        )),
    }
}

/// A PreToolUse deny with a reason the agent can report back.
fn deny(reason: &str) {
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
    );
}

/// The first path in `tool_input` outside `root` and every `also` dir.
fn escapes(root: &Path, tool: &str, tool_input: &Value, also: &[PathBuf]) -> Option<String> {
    let allowed: Vec<&Path> = std::iter::once(root).chain(also.iter().map(PathBuf::as_path)).collect();
    // (symlinks resolved first, ".." applied first)
    let mut bases = (root.to_path_buf(), root.to_path_buf());

    // A tool that takes no path is never confined.
    let (_, fields) = PATH_TOOLS.iter().find(|(name, _)| *name == tool)?;

    for field in *fields {
        let Some(value) = tool_input.get(*field).and_then(Value::as_str) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let rel = expanduser(Path::new(value));
        // Both orders of resolving symlinks and "..": the CLI applies ".."
        // lexically, canonicalize follows links first. Glob patterns are
        // resolved whole, wildcards as plain names.
        let targets = (
            resolve_lenient(&bases.0.join(&rel)),
            resolve_lenient(&normpath(&bases.1.join(&rel))),
        );
        let escapes = [targets.0.as_path(), targets.1.as_path()]
            .iter()
            .any(|t| !allowed.iter().any(|r| t.starts_with(*r)));
        if escapes {
            return Some(value.to_string());
        }
        if *field == "path" {
            bases = (targets.0.clone(), normpath(&bases.1.join(&rel)));
        }
    }
    None
}

/// Expand a leading `~` — the CLI does, so a path from a prompt can use it.
fn expanduser(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let Some(rest) = s.strip_prefix("~/") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Lexical normalisation: collapse `.` and `..` without touching the
/// filesystem. A `..` past the root stays at the root, as `normpath` does.
fn normpath(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// `canonicalize` that tolerates a path that does not exist: a Glob pattern
/// like `**\/*.py` never does. Canonicalize the nearest existing ancestor and
/// re-attach the rest, which is what resolving symlinks needs.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(real) = parent.canonicalize() {
            let mut out = real;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        if parent == cursor {
            break;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_inside_root_passes() {
        assert_eq!(escapes(Path::new("/repo"), "Read", &json!({"file_path": "/repo/src/a.py"}), &[]), None);
    }

    #[test]
    fn an_absolute_path_out_of_root_is_denied() {
        assert_eq!(
            escapes(Path::new("/repo"), "Read", &json!({"file_path": "/etc/passwd"}), &[]),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn dotdot_out_of_root_is_denied() {
        assert_eq!(
            escapes(Path::new("/repo"), "Read", &json!({"file_path": "../secrets"}), &[]),
            Some("../secrets".to_string())
        );
    }

    #[test]
    fn a_sibling_prefix_is_still_outside() {
        // /repo-backup must not pass as inside /repo.
        assert!(escapes(Path::new("/repo"), "Read", &json!({"file_path": "/repo-backup/x"}), &[]).is_some());
    }

    #[test]
    fn an_extra_read_root_is_permitted() {
        assert_eq!(
            escapes(Path::new("/repo"), "Read", &json!({"file_path": "/extra/a.py"}), &[PathBuf::from("/extra")]),
            None
        );
    }

    #[test]
    fn a_non_path_tool_is_never_confined() {
        assert_eq!(escapes(Path::new("/repo"), "Bash", &json!({"command": "rm -rf /"}), &[]), None);
    }

    #[test]
    fn glob_patterns_are_resolved_whole() {
        assert_eq!(escapes(Path::new("/repo"), "Glob", &json!({"pattern": "src/**/*.py"}), &[]), None);
        assert!(escapes(Path::new("/repo"), "Glob", &json!({"pattern": "../**/*.py"}), &[]).is_some());
    }

    #[test]
    fn normpath_collapses_dots_without_the_filesystem() {
        assert_eq!(normpath(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        // A `..` past the root clamps, it does not escape upward.
        assert_eq!(normpath(Path::new("/../etc")), PathBuf::from("/etc"));
    }

    #[test]
    fn a_root_reached_through_a_symlink_still_matches() {
        // /tmp is a symlink to /private/tmp on macOS. Resolving the file but
        // not the root makes every read look like an escape — and denies the
        // agent the directory it was pointed at.
        let real = std::env::temp_dir().canonicalize().unwrap();
        let inside = real.join("rustflows-symlink-probe.txt");
        std::fs::write(&inside, "x").unwrap();
        assert_eq!(escapes(&real, "Read", &json!({"file_path": inside.to_str().unwrap()}), &[]), None);
        let _ = std::fs::remove_file(&inside);
    }

    #[test]
    fn resolve_lenient_handles_a_path_that_does_not_exist() {
        let got = resolve_lenient(Path::new("/definitely/not/here/**/*.py"));
        assert!(got.is_absolute());
        assert!(got.ends_with("**/*.py"));
    }
}
