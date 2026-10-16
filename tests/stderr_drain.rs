//! A child that floods stderr must not deadlock the call.
//!
//! This is the bug the first self-review found: `stderr` was piped and drained
//! only after `child.wait()`, so a CLI writing more than the ~64KB pipe buffer
//! blocked in `write()`, never closed stdout, and the read loop never ended —
//! while holding its gate permit for the rest of the run.
//!
//! Uses a fake CLI, so it costs nothing and needs no network.


use rustflows::runner::{self, CallSpec};

const RESULT: &str = r#"{"type":"result","structured_output":{"ok":true},"usage":{"input_tokens":1},"total_cost_usd":0,"duration_api_ms":1,"subtype":"success","is_error":false}"#;

fn spec() -> CallSpec {
    CallSpec {
        cwd: std::env::temp_dir(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_child_that_floods_stderr_still_returns_and_its_output_is_reported() {
    // One test, not two: `set_var` is unsafe in edition 2024, and two tests
    // setting the same variable concurrently is the hazard it is unsafe about.
    let dir = std::env::temp_dir().join(format!("rf-fake-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let write = |name: &str, body: String| {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };

    // 256KB of stderr — four times a typical pipe buffer.
    let noisy = write(
        "claude-noisy",
        format!(
            "#!/bin/sh\ncat > /dev/null\necho '{RESULT}'\n\
             dd if=/dev/zero bs=1024 count=256 2>/dev/null | tr '\\0' 'x' 1>&2\nexit 0\n"
        ),
    );
    // SAFETY: the only writer of the environment in this binary.
    unsafe { std::env::set_var("RUSTFLOWS_CLAUDE", &noisy) };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        runner::run(&spec()),
    )
    .await
    .expect("run() hung: stderr filled the pipe buffer and was never drained")
    .expect("run() should succeed");
    assert_eq!(result.structured_output, Some(serde_json::json!({"ok": true})));

    // A run with no result line still reports the child's stderr, so a failure
    // is diagnosable rather than a bare "no result message".
    let quiet = write(
        "claude-no-result",
        "#!/bin/sh\ncat > /dev/null\necho 'a distinctive warning' 1>&2\nexit 3\n".into(),
    );
    // SAFETY: as above, and no other thread reads the environment here.
    unsafe { std::env::set_var("RUSTFLOWS_CLAUDE", &quiet) };

    let err = runner::run(&spec()).await.expect_err("no result line means an error");
    assert!(err.contains("a distinctive warning"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
