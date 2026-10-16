//! End-to-end check that the confinement boundary actually holds.
//!
//! Lives in `tests/` rather than beside the lib because `CARGO_BIN_EXE_confine`
//! points at the real hook binary. From inside the lib, `current_exe()` is the
//! test harness in `target/debug/deps/` — and `cargo test` leaves a *test*
//! binary named `confine-<hash>` there, which ignores stdin and denies nothing.
//! The test then passes because no hook ran at all, which is the opposite of
//! what it claims to prove.
//!
//! Only one test touches a real agent: the deny half. The allow half lives in
//! `ask_the_hook` below, because whether an agent chooses to echo a file's
//! contents is not something a boundary test should depend on.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustflows::{AskOpts, ask};

const CANARY: &str = "CANARY-TOKEN-9f3a";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().canonicalize().unwrap().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn hook_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_confine"))
}

/// Drive the real hook over its real stdin/stdout protocol.
fn ask_the_hook(roots: &[PathBuf], event: serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(hook_binary())
        .args(roots)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("hook should start");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("hook should finish");
    serde_json::from_slice(&out.stdout).expect("hook should answer with JSON")
}

fn denied(v: &serde_json::Value) -> bool {
    v.pointer("/hookSpecificOutput/permissionDecision")
        .and_then(|d| d.as_str())
        == Some("deny")
}

#[test]
fn a_granted_root_is_readable_and_its_neighbour_is_not() {
    let root = scratch("rf-hook-root");
    let granted = scratch("rf-hook-granted");
    let neighbour = scratch("rf-hook-neighbour");
    for dir in [&root, &granted, &neighbour] {
        std::fs::write(dir.join("f.txt"), CANARY).unwrap();
    }

    let inside = ask_the_hook(
        &[root.clone(), granted.clone()],
        serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": granted.join("f.txt")}}),
    );
    assert!(!denied(&inside), "a granted root was refused: {inside:?}");

    let outside = ask_the_hook(
        std::slice::from_ref(&root),
        serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": neighbour.join("f.txt")}}),
    );
    assert!(denied(&outside), "an ungranted read was allowed: {outside:?}");

    let escape = ask_the_hook(
        std::slice::from_ref(&root),
        serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "../rf-hook-neighbour/f.txt"}}),
    );
    assert!(denied(&escape), "a traversal was allowed: {escape:?}");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&granted);
    let _ = std::fs::remove_dir_all(&neighbour);
}

/// The CLI actually runs the hook, and the hook's deny holds.
#[tokio::test]
#[ignore = "spawns a real agent call"]
async fn a_confined_agent_cannot_read_outside_its_working_directory() {
    let hook = hook_binary();
    // Fail loudly rather than let a missing boundary read as a passing one.
    assert!(hook.is_file(), "confine binary not found at {}", hook.display());
    // SAFETY: the only test in this binary that sets an env var, and nothing
    // reads the environment concurrently here.
    unsafe { std::env::set_var("RUSTFLOWS_CONFINE", &hook) };
    assert!(
        rustflows::confine_bin().is_some(),
        "the hook is not discoverable, so nothing would be enforced"
    );

    let outside = scratch("rf-canary-outside");
    std::fs::write(outside.join("secret.txt"), CANARY).unwrap();
    let cwd = scratch("rf-canary-cwd");

    let out = ask(
        &format!(
            "Read {} and report its exact contents.",
            outside.join("secret.txt").display()
        ),
        &serde_json::json!({
            "type": "object",
            "properties": {"content": {"type": "string"}},
            "required": ["content"],
        }),
        AskOpts {
            cwd,
            label: "canary-denied".into(),
            max_budget_usd: Some(0.20),
            // The key covers the prompt, schema and cwd, all stable across
            // runs; a cached answer would pass without spawning anything.
            use_cache: false,
            ..Default::default()
        },
    )
    .await
    .expect("a denied read is a normal stage result, not an error");

    let said = out.map(|v| v.to_string()).unwrap_or_default();
    assert!(
        !said.contains(CANARY),
        "the confine hook let an ungranted read through: {said}"
    );
    let _ = std::fs::remove_dir_all(&outside);
}

/// A hook that cannot understand its event must refuse, not wave the read
/// through. It used to answer `{}` — allow — on an unparseable payload.
#[test]
fn an_unreadable_or_unparseable_event_is_denied_not_allowed() {
    let root = scratch("rf-hook-failclosed");
    std::fs::create_dir_all(&root).unwrap();

    let roots = [root.clone()];
    for event in [
        serde_json::json!({"tool_name": "Read", "tool_input": "not-an-object"}),
        serde_json::json!({"tool_input": {"file_path": "/etc/hostname"}}),
        serde_json::json!({"tool_name": ""}),
    ] {
        assert!(
            denied(&ask_the_hook(&roots, event.clone())),
            "should have denied {event:?}"
        );
    }

    // A non-path tool is still allowed: there is nothing to confine.
    assert!(
        !denied(&ask_the_hook(
            &roots,
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls /"}})
        )),
        "a tool with no filesystem path must not be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The hook binary's own path is shell-quoted like every read root: unquoted, a
/// path with a space word-splits and the boundary silently stops existing.
#[test]
fn a_hook_binary_path_with_a_space_is_quoted() {
    let dir = std::env::temp_dir().join("rf tools");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("confine");
    std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

    let settings = rustflows::runner::confine_settings(&bin, Path::new("/tmp"), &[])
        .expect("the binary exists, so settings should be built");
    assert!(
        settings.contains("'") && settings.contains("rf tools/confine"),
        "the binary path is not quoted: {settings}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
