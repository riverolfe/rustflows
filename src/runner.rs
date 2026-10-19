//! Spawn `claude`, read the structured payload off the stream.
//!
//! The final message of `--output-format stream-json` carries `structured_output`,
//! usage and cost. Plain `--output-format json` does NOT: the CLI injects a
//! `StructuredOutput` tool and only the streaming result message reports what it
//! was given. So the stream is parsed line by line and every line but the
//! terminal `result` is discarded.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// One agent call's inputs, already resolved by the caller.
#[derive(Debug, Clone)]
pub struct CallSpec {
    pub prompt: String,
    pub schema: Value,
    pub tools: Vec<String>,
    pub system: Option<String>,
    pub cwd: PathBuf,
    pub max_budget_usd: Option<f64>,
    /// `Some(sources)` passes `--setting-sources` verbatim, so `Some(vec![])`
    /// loads none while `--settings` still applies. `None` means *inherit*: the
    /// CLI's own `user,project,local`, which is what `--inherit-settings` asks
    /// for. Extensions decides which; the default is `Some(vec![])`.
    pub setting_sources: Option<Vec<String>>,
    pub settings_json: Option<String>,
    pub extra_allowed: Vec<String>,
    pub plugins: Vec<PathBuf>,
    pub mcp_servers: Value,
}

impl Default for CallSpec {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            schema: json!({"type": "object"}),
            tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            system: None,
            cwd: PathBuf::from("."),
            max_budget_usd: None,
            setting_sources: Some(Vec::new()),
            settings_json: None,
            extra_allowed: Vec::new(),
            plugins: Vec::new(),
            mcp_servers: json!({}),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CallResult {
    pub structured_output: Option<Value>,
    pub usage: Value,
    pub model_usage: Value,
    pub total_cost_usd: f64,
    pub duration_api_ms: f64,
    pub subtype: String,
    pub is_error: bool,
}

/// `subtype` the CLI reports when a call stops at its `--max-budget-usd` ceiling.
pub const OVER_BUDGET: &str = "error_max_budget_usd";

impl CallResult {
    pub fn over_budget(&self) -> bool {
        self.subtype == OVER_BUDGET
    }
}

fn binary() -> String {
    std::env::var("RUSTFLOWS_CLAUDE").unwrap_or_else(|_| "claude".to_string())
}

fn model() -> Option<String> {
    std::env::var("RUSTFLOWS_MODEL").ok().filter(|m| !m.is_empty())
}

/// Assemble argv. Split out so it can be checked without spending a call.
pub fn argv(spec: &CallSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(m) = model() {
        a.extend(["--model".into(), m]);
    }
    a.extend(["--json-schema".into(), spec.schema.to_string()]);

    let mut allowed: Vec<String> = spec.tools.clone();
    allowed.extend(spec.extra_allowed.iter().cloned());
    allowed.sort();
    allowed.dedup();
    if !allowed.is_empty() {
        a.push("--allowedTools".into());
        a.extend(allowed);
    }
    // Drops the write/exec surface before any allowlist is consulted.
    a.extend([
        "--disallowedTools".into(),
        "Bash".into(),
        "Write".into(),
        "Edit".into(),
    ]);

    if let Some(system) = &spec.system
        && !system.is_empty() {
            a.extend(["--system-prompt".into(), system.clone()]);
        }
    if let Some(budget) = spec.max_budget_usd {
        a.extend(["--max-budget-usd".into(), format!("{budget}")]);
    }
    // Settings, skills, plugins and MCP servers: none unless extensions grant them.
    match &spec.setting_sources {
        Some(sources) => a.extend(["--setting-sources".into(), sources.join(",")]),
        None => a.extend(["--setting-sources".into(), "user,project,local".into()]),
    }
    if let Some(settings) = &spec.settings_json {
        a.extend(["--settings".into(), settings.clone()]);
    }
    for p in &spec.plugins {
        a.extend(["--plugin-dir".into(), p.display().to_string()]);
    }
    // No `--skills` flag exists: a named skill is granted by the plugin that
    // ships it, or by loading user settings. Extensions rejects the case the
    // flag would have covered rather than dropping the grant silently.
    if let Some(servers) = spec.mcp_servers.as_object()
        && !servers.is_empty() {
            a.extend(["--mcp-config".into(), spec.mcp_servers.to_string()]);
            a.extend(["--strict-mcp-config".into()]);
        }
    a
}

/// Run one agent call. The prompt goes on stdin so a long prompt cannot hit
/// the argument-length limit.
pub async fn run(spec: &CallSpec) -> Result<CallResult, String> {
    let mut cmd = Command::new(binary());
    cmd.args(argv(spec))
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start `{}`: {e}", binary()))?;

    if let Some(mut stdin) = child.stdin.take() {
        // A closed pipe just means the CLI never wanted it; the exit status
        // below is the real verdict.
        let _ = stdin.write_all(spec.prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    // Drain stderr concurrently. A child that fills the ~64KB pipe buffer blocks
    // in write() and never closes stdout, so reading stderr only after wait()
    // hangs the call — and the gate permit is held across it, so eight such
    // calls exhaust the pool for the rest of the run.
    let stderr_pipe = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_string(&mut buf).await;
        }
        buf
    });

    let mut result: Option<CallResult> = None;
    let mut read_error: Option<String> = None;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("type").and_then(Value::as_str) != Some("result") {
                    continue;
                }
                result = Some(parse_result(&message));
            }
            Ok(None) => break,
            // `while let Ok(Some(..))` threw this away, so a local read failure
            // was reported as "the CLI sent no result message" — blaming the
            // CLI for our own broken pipe.
            Err(e) => {
                read_error = Some(e.to_string());
                break;
            }
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("waiting on `{}`: {e}", binary()))?;
    let stderr = stderr_task.await.unwrap_or_default();

    match result {
        Some(r) => Ok(r),
        None => {
            Err(format!(
                "stream ended without a result message ({status}){}{}",
                read_error.map(|e| format!("; reading stdout failed: {e}")).unwrap_or_default(),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            ))
        }
    }
}

fn parse_result(m: &Value) -> CallResult {
    CallResult {
        structured_output: m.get("structured_output").filter(|v| !v.is_null()).cloned(),
        usage: m.get("usage").cloned().unwrap_or_else(|| json!({})),
        model_usage: m
            .get("modelUsage")
            .cloned()
            .unwrap_or_else(|| json!({})),
        total_cost_usd: m.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        duration_api_ms: m
            .get("duration_api_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        subtype: m
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        is_error: m.get("is_error").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// The PreToolUse hook settings document for `confine`, or `None` when there is
/// nothing to confine (the hook binary missing, or no read roots beyond cwd).
pub fn confine_settings(
    confine_bin: &Path,
    root: &Path,
    also: &[PathBuf],
) -> Option<String> {
    if !crate::stats::is_executable_file(confine_bin) {
        return None;
    }
    // The working directory first, always. This used to be a BTreeMap's keys(),
    // which iterate in sorted order — so `~/.claude/skills` could become argv[1]
    // and the hook, which takes its first root as the boundary, adopted a
    // directory as the confinement root instead of the cwd.
    let root_arg = root.display().to_string();
    let mut others: Vec<String> = also
        .iter()
        .map(|r| r.display().to_string())
        .filter(|r| *r != root_arg)
        .collect();
    others.sort();
    others.dedup();
    let args: Vec<String> = std::iter::once(root_arg)
        .chain(others)
        .map(|r| shell_quote(&r))
        .collect();
    // The binary path is quoted too: a path with a space would otherwise
    // word-split and the hook would never run — which reads as "nothing to
    // enforce" rather than "the boundary failed".
    let command = format!("{} {}", shell_quote(&confine_bin.display().to_string()), args.join(" "));
    Some(
        json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Read|Grep|Glob",
                    "hooks": [{"type": "command", "command": command}]
                }]
            }
        })
        .to_string(),
    )
}

/// Quote for the shell the CLI uses to run a hook command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CallSpec {
        CallSpec { cwd: PathBuf::from("/tmp"), ..Default::default() }
    }

    #[test]
    fn argv_carries_the_flags_the_port_needs() {
        let a = argv(&spec());
        let joined = a.join(" ");
        assert!(joined.contains("--output-format stream-json"));
        assert!(joined.contains("--verbose"));
        assert!(joined.contains("--permission-mode") || joined.contains("--allowedTools"));
        assert!(joined.contains("--disallowedTools Bash Write Edit"));
        assert!(joined.contains("--setting-sources"));
    }

    #[test]
    fn allowed_tools_are_sorted_and_deduped() {
        let mut s = spec();
        s.tools = vec!["Read".into(), "Glob".into()];
        s.extra_allowed = vec!["Read".into(), "mcp__x".into()];
        let a = argv(&s);
        let i = a.iter().position(|x| x == "--allowedTools").unwrap();
        assert_eq!(&a[i + 1..i + 5], ["Glob", "Read", "mcp__x", "--disallowedTools"]);
    }

    #[test]
    fn empty_settings_sources_still_passes_the_flag() {
        let a = argv(&spec());
        let i = a.iter().position(|x| x == "--setting-sources").unwrap();
        assert_eq!(a[i + 1], "");
    }

    #[test]
    fn parse_result_maps_every_field_the_port_reads() {
        let r = parse_result(&json!({
            "type": "result",
            "structured_output": {"answer": "pong"},
            "usage": {"input_tokens": 3},
            "modelUsage": {"m": {"inputTokens": 1}},
            "total_cost_usd": 0.5,
            "duration_api_ms": 1200.0,
            "subtype": "success",
            "is_error": false,
        }));
        assert_eq!(r.structured_output, Some(json!({"answer": "pong"})));
        assert_eq!(r.total_cost_usd, 0.5);
        assert_eq!(r.duration_api_ms, 1200.0);
        assert!(!r.over_budget());
    }

    #[test]
    fn null_structured_output_reads_as_absent() {
        let r = parse_result(&json!({"type": "result", "structured_output": null}));
        assert_eq!(r.structured_output, None);
    }

    #[test]
    fn budget_stop_is_recognised() {
        let r = parse_result(&json!({"type": "result", "subtype": OVER_BUDGET}));
        assert!(r.over_budget());
    }

    #[test]
    fn confine_settings_quote_every_root() {
        let bin = executable_temp_hook();
        let s = confine_settings(&bin, Path::new("/tmp/a b"), &[PathBuf::from("/tmp/c")]).unwrap();
        assert!(s.contains("Read|Grep|Glob"));
        assert!(s.contains("'/tmp/a b'"));
        assert!(s.contains("'/tmp/c'"));
        let _ = std::fs::remove_file(&bin);
    }

    /// A file the OS will run, which is what `confine_settings` now requires.
    fn executable_temp_hook() -> PathBuf {
        let bin = std::env::temp_dir().join(format!("rustflows-confine-test-{}", std::process::id()));
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    #[test]
    fn the_working_directory_is_the_first_root_the_hook_receives() {
        // This used to be a BTreeMap's keys(), which iterate sorted. An extra
        // root sorting before the cwd — which is what a ~/.claude/skills grant
        // does — put the hook's boundary on the wrong directory.
        let bin = executable_temp_hook();
        let s = confine_settings(
            &bin,
            Path::new("/repo"),
            &[
                PathBuf::from("/granted/skills"),
                PathBuf::from("/extra"),
            ],
        )
        .unwrap();
        let argv = s.split("command").nth(1).expect("a command");
        let cwd_at = argv.find("'/repo'").expect("the cwd is passed");
        let skills_at = argv.find("skills").expect("the granted root is passed");
        assert!(cwd_at < skills_at, "the cwd must come first: {argv}");
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn confine_settings_are_omitted_when_the_hook_is_not_runnable() {
        assert!(confine_settings(Path::new("/nope/confine"), Path::new("/tmp"), &[]).is_none());

        // Exists, is a regular file, and cannot be run. Approving the settings
        // document here would leave reads unconfined while the cache key still
        // recorded confined=true.
        let bin = std::env::temp_dir().join(format!("rustflows-confine-noexec-{}", std::process::id()));
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(
            confine_settings(&bin, Path::new("/tmp"), &[]).is_none(),
            "a non-executable hook must be treated as missing"
        );
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}