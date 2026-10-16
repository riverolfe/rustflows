//! `ask()` driven against a fake `claude`, so these cost nothing and need no
//! network.
//!
//! What is under test is what `ask` does with a result the CLI has already
//! flagged — specifically that a failed stage's payload is never recorded as
//! success or written to the cache.
//!
//! One test, not several: each case has to install a different fake CLI and
//! cache directory through the environment, and `set_var` is unsafe in edition
//! 2024 precisely because of concurrent access. Three `#[tokio::test]`s in one
//! binary run in parallel and would hand each other the other's CLI.

use std::path::{Path, PathBuf};

use rustflows::{AskOpts, ask};

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
    })
}

/// A fake CLI that prints one `result` line.
fn fake_cli(tag: &str, result_line: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rf-ask-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!("#!/bin/sh\ncat > /dev/null\ncat <<'JSON'\n{result_line}\nJSON\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}


/// A CLI that narrates in prose on its first call and complies on the second.
fn fake_cli_prose_first(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rf-ask-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let counter = dir.join("count").display().to_string();
    let path = dir.join("claude");
    let prose = r#"{"type":"result","is_error":false,"subtype":"success","stop_reason":"end_turn","duration_api_ms":1,"result":"I looked at it and here is what I found."}"#;
    let good = r#"{"type":"result","structured_output":{"answer":"recovered"},"usage":{"input_tokens":1},"total_cost_usd":0.01,"duration_api_ms":1,"subtype":"success","is_error":false}"#;
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > /dev/null\n\
             n=$(cat '{counter}' 2>/dev/null || echo 0)\necho $((n+1)) > '{counter}'\n\
             if [ \"$n\" = \"0\" ]; then\n  echo '{prose}'\nelse\n  echo '{good}'\nfi\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}


/// A CLI that narrates first (triggering a retry) and then reports an error
/// while still carrying a schema-valid payload.
fn fake_cli_erroring_retry(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rf-ask-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let counter = dir.join("count").display().to_string();
    let path = dir.join("claude");
    let prose = r#"{"type":"result","is_error":false,"subtype":"success","stop_reason":"end_turn","duration_api_ms":1,"result":"Here is what I found."}"#;
    // is_error with a *valid* payload: the shape that got cached as a success.
    let errored = r#"{"type":"result","structured_output":{"answer":"partial"},"usage":{"input_tokens":1},"total_cost_usd":0.01,"duration_api_ms":1,"subtype":"error_during_execution","is_error":true}"#;
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > /dev/null\n\
             n=$(cat '{counter}' 2>/dev/null || echo 0)\necho $((n+1)) > '{counter}'\n\
             if [ \"$n\" = \"0\" ]; then\n  echo '{prose}'\nelse\n  echo '{errored}'\nfi\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn cache_entries(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("entries"))
        .map(|d| d.count())
        .unwrap_or(0)
}

fn opts() -> AskOpts {
    AskOpts {
        cwd: std::env::temp_dir(),
        max_budget_usd: Some(1.0),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_error_result_is_a_dead_stage_while_a_good_one_is_returned_and_cached() {
    let cli = fake_cli(
        "err",
        r#"{"type":"result","structured_output":{"answer":"partial"},"usage":{"input_tokens":1},"total_cost_usd":0.01,"duration_api_ms":1,"subtype":"error_during_execution","is_error":true}"#,
    );
    let cache = std::env::temp_dir().join(format!("rf-ask-cache-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    // SAFETY: the only writer of the environment in this binary, and every
    // call below is sequential within this one test.
    unsafe {
        std::env::set_var("RUSTFLOWS_CLAUDE", &cli);
        std::env::set_var("RUSTFLOWS_CACHE", &cache);
    }

    // The CLI reports `error_during_execution` with a well-formed
    // structured_output. Nothing checked `is_error`, so this partial answer was
    // recorded `ok` and written to disk — and served to every later run.
    let out = ask("q", &schema(), opts())
        .await
        .expect("a dead stage is not an error");
    assert_eq!(out, None, "an errored result must not be returned as a payload");
    assert_eq!(
        cache_entries(&cache),
        0,
        "a failed stage's payload must not reach the cache"
    );

    // A schema that will not compile refuses the stage before any call.
    let broken = serde_json::json!({"type": "not-a-real-type"});
    assert_eq!(ask("q", &broken, opts()).await.unwrap(), None);
    assert_eq!(cache_entries(&cache), 0, "a broken schema must not cache");

    // The healthy path still works, and still caches.
    let good = fake_cli(
        "ok",
        r#"{"type":"result","structured_output":{"answer":"pong"},"usage":{"input_tokens":1},"total_cost_usd":0.01,"duration_api_ms":1,"subtype":"success","is_error":false}"#,
    );
    // SAFETY: as above.
    unsafe { std::env::set_var("RUSTFLOWS_CLAUDE", &good) };

    let out = ask("q", &schema(), opts()).await.expect("a healthy stage");
    assert_eq!(out, Some(serde_json::json!({"answer": "pong"})));
    assert_eq!(cache_entries(&cache), 1, "a good result must be cached");

    // An agent that narrates instead of calling StructuredOutput arrives as
    // `subtype: success, is_error: false` with no payload — which is what kept
    // killing the performance lens. That used to be a dead `no_output` with no
    // retry at all.
    let flaky = fake_cli_prose_first("flaky");
    // SAFETY: as above.
    unsafe { std::env::set_var("RUSTFLOWS_CLAUDE", &flaky) };
    let out = ask("q2", &schema(), opts())
        .await
        .expect("a narrated first attempt is retried, not fatal");
    assert_eq!(
        out,
        Some(serde_json::json!({"answer": "recovered"})),
        "a prose-only answer should be retried, not reported as a dead stage"
    );

    // And the retry itself must be judged: an errored retry carrying a
    // schema-valid partial payload used to be recorded `ok` and cached. This
    // check existed only on the first-call path.
    let bad_retry = fake_cli_erroring_retry("badretry");
    // SAFETY: as above.
    unsafe { std::env::set_var("RUSTFLOWS_CLAUDE", &bad_retry) };
    let out = ask("q3", &schema(), opts()).await.expect("not fatal to the run");
    assert_eq!(
        out,
        None,
        "a failed retry must not be returned or cached as a success"
    );

    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_dir_all(cli.parent().unwrap());
}