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
        Some(Verdict::Outside(path)) => deny(&format!(
            "{tool} on {path} is outside the working directory {}; only files under it may be read.",
            root.display()
        )),
        Some(Verdict::Unreadable(field)) => deny(&format!(
            "{tool} was given a {field} that is not a path this hook could check, so the read \
             cannot be confined. Rejecting it: an unchecked read is not a safe read."
        )),
    }
}

/// Why a call was refused. Two reasons, never merged: reporting a malformed
/// field as "is outside the working directory" sends whoever is debugging it
/// looking in the wrong place.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// Resolves outside the root and every extra read root.
    Outside(String),
    /// A declared path field was present but could not be interpreted.
    Unreadable(String),
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

/// The first path in `tool_input` outside `root` and every `also` dir, or a
/// declared field whose value this hook cannot interpret.
fn escapes(root: &Path, tool: &str, tool_input: &Value, also: &[PathBuf]) -> Option<Verdict> {
    let allowed: Vec<&Path> = std::iter::once(root).chain(also.iter().map(PathBuf::as_path)).collect();
    // (symlinks resolved first, ".." applied first)
    let mut bases = (root.to_path_buf(), root.to_path_buf());

    // A tool that takes no path is never confined.
    let (_, fields) = PATH_TOOLS.iter().find(|(name, _)| *name == tool)?;

    for field in *fields {
        let Some(raw) = tool_input.get(*field) else {
            // Absent is legitimate and different from unchecked: Grep and Glob
            // both default to the working directory when `path` is omitted, and
            // the cwd is the root, so there is nothing outside it. The
            // `tool_input`-missing case above is denied for the same reason this
            // one is not: a tool that takes no path at all is a different
            // shape from one that takes a path it did not supply.
            continue;
        };
        let Some(value) = raw.as_str() else {
            // Present, and not something we can read as a path. Approving a
            // read whose target we could not interpret is how a boundary fails
            // open: this used to `continue`, so `{"file_path": 123}` allowed.
            return Some(Verdict::Unreadable((*field).to_string()));
        };
        // An empty string is the cwd spelled badly, not an unchecked path. It
        // is checked as one so the allow is earned rather than skipped into.
        let value = if value.is_empty() { "." } else { value };
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
            return Some(Verdict::Outside(value.to_string()));
        }
        if *field == "path" {
            bases = (targets.0.clone(), normpath(&bases.1.join(&rel)));
        }
    }
    None
}

/// Expand a leading `~` — the CLI does, so a path from a prompt can use one.
///
/// A bare `~` is `$HOME`, not the literal directory named `~`. Handling only
/// `~/` resolved `Grep {"path":"~"}` to `<root>/~`, which is inside the root and
/// therefore allowed, while the CLI read the user's whole home directory. The
/// sibling copy in `ext.rs` already got this right; `the_hook_agrees_with_ext_on
/// _expanding_a_tilde` keeps the two from drifting again.
///
/// Deliberately not `rustflows::ext::expanduser`: that would link clap,
/// jsonschema, sha2 and tokio into a hook that runs on every tool call, taking
/// it from 563K to roughly the size of the whole binary.
fn expanduser(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let rest = match s.strip_prefix("~/") {
        Some(rest) => rest,
        None if s == "~" => "",
        None => return path.to_path_buf(),
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
            Some(Verdict::Outside("/etc/passwd".to_string()))
        );
    }

    #[test]
    fn dotdot_out_of_root_is_denied() {
        assert_eq!(
            escapes(Path::new("/repo"), "Read", &json!({"file_path": "../secrets"}), &[]),
            Some(Verdict::Outside("../secrets".to_string()))
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

    #[test]
    fn a_path_field_that_is_not_a_string_is_refused_not_skipped() {
        // This used to `continue`, so a value the hook could not interpret was
        // approved: the one case where a boundary fails open rather than shut.
        for field in ["file_path", "path", "pattern"] {
            let tool = match field {
                "file_path" => "Read",
                "path" => "Grep",
                _ => "Glob",
            };
            let event = json!({field: 123});
            assert_eq!(
                escapes(Path::new("/repo"), tool, &event, &[]),
                Some(Verdict::Unreadable(field.to_string())),
                "{tool} with a non-string {field}"
            );
            // And not merely non-string: null, an array, an object, a bool.
            for bad in [json!(null), json!([1]), json!({"a": 1}), json!(true)] {
                assert_eq!(
                    escapes(Path::new("/repo"), tool, &json!({field: bad}), &[]),
                    Some(Verdict::Unreadable(field.to_string()))
                );
            }
        }
    }

    #[test]
    fn an_absent_path_field_is_the_tools_own_default_not_an_unchecked_read() {
        // Grep and Glob both default to the working directory, which is the
        // root. Denying these would break ordinary searches, so the distinction
        // from "present but unreadable" is load-bearing.
        assert_eq!(
            escapes(Path::new("/repo"), "Glob", &json!({"pattern": "src/**/*.py"}), &[]),
            None
        );
        assert_eq!(escapes(Path::new("/repo"), "Grep", &json!({}), &[]), None);
        assert_eq!(escapes(Path::new("/repo"), "Read", &json!({}), &[]), None);
    }

    #[test]
    fn an_empty_path_is_checked_as_the_cwd_rather_than_skipped() {
        // An empty string is the cwd spelled badly. It is checked, so the allow
        // is earned — and it stays inside the root.
        assert_eq!(escapes(Path::new("/repo"), "Grep", &json!({"path": ""}), &[]), None);
    }

    #[test]
    fn a_bare_tilde_is_the_home_directory_and_not_a_directory_named_tilde() {
        // Only "~/x" was expanded, so `~` resolved to <root>/~ — inside the
        // root, therefore allowed — while the CLI read all of $HOME.
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
        assert_eq!(expanduser(Path::new("~")), home);
        assert_eq!(expanduser(Path::new("~/x")), home.join("x"));

        let verdict = escapes(Path::new("/repo"), "Grep", &json!({"path": "~"}), &[]);
        assert!(
            matches!(verdict, Some(Verdict::Outside(_))),
            "a bare ~ must not pass as inside the root, got {verdict:?}"
        );
        // With home granted, the same call is fine.
        assert_eq!(
            escapes(Path::new("/repo"), "Grep", &json!({"path": "~"}), &[home]),
            None
        );
    }

    #[test]
    fn the_hook_agrees_with_ext_on_expanding_a_tilde() {
        // Two copies of expanduser exist because the hook must not link the
        // lib (it runs on every tool call). This is what keeps them from
        // drifting — the divergence is exactly how `~` slipped through.
        for input in ["~", "~/x", "a/~/b", "x", "/abs/~/y", "", "./~", "~user/x"] {
            assert_eq!(
                expanduser(Path::new(input)),
                rustflows::ext::expanduser(Path::new(input)),
                "{input:?}"
            );
        }
    }
}
