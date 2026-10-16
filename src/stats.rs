//! Live accounting for a run, plus the run log the terminal panel replays.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::runner::CallResult;

/// One call's usage, accumulated across any retry.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub seconds: f64,
    pub cost_usd: f64,
}

pub fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input: a.input + b.input,
        output: a.output + b.output,
        cache_read: a.cache_read + b.cache_read,
        cache_write: a.cache_write + b.cache_write,
        seconds: a.seconds + b.seconds,
        cost_usd: a.cost_usd + b.cost_usd,
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub agents: u64,
    pub cache_hits: u64,
    pub dead: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
    pub seconds: f64,
    pub labels: HashMap<String, u64>,
}

impl Stats {
    pub fn reset(&mut self) {
        *self = Stats::default();
    }

    /// `label`, or `label#n` for the n-th use of it this run.
    pub fn unique(&mut self, label: &str) -> String {
        if label.is_empty() {
            return String::new();
        }
        let n = self.labels.entry(label.to_string()).or_insert(0);
        *n += 1;
        if *n == 1 {
            label.to_string()
        } else {
            format!("{label}#{n}")
        }
    }

    /// Add one call's usage to the run totals and return that call's usage.
    ///
    /// Does not log: `ask` writes one terminal record per agent, carrying the
    /// usage of its first call plus any retry.
    pub fn record(&mut self, result: &CallResult) -> Usage {
        self.agents += 1;
        self.seconds += result.duration_api_ms / 1000.0;
        self.cost_usd += result.total_cost_usd;

        let u = &result.usage;
        let read = num(u, "cache_read_input_tokens");
        let write = num(u, "cache_creation_input_tokens");
        let got = num(u, "input_tokens") + num(u, "output_tokens");

        let (mut tok_in, mut tok_out, mut r, mut w) = (0, 0, 0, 0);
        if got != 0 {
            tok_in = num(u, "input_tokens");
            tok_out = num(u, "output_tokens");
            r = read;
            w = write;
        } else {
            // usage is flat; modelUsage is the per-model breakdown. Fall back
            // per result — testing the cumulative fields would latch the
            // fallback off after the first non-empty one.
            if let Some(models) = result.model_usage.as_object() {
                for m in models.values() {
                    tok_in += num(m, "inputTokens");
                    tok_out += num(m, "outputTokens");
                    r += num(m, "cacheReadInputTokens");
                    w += num(m, "cacheCreationInputTokens");
                }
            }
        }

        self.input_tokens += tok_in;
        self.output_tokens += tok_out;
        self.cache_read += r;
        self.cache_write += w;
        Usage {
            input: tok_in,
            output: tok_out,
            cache_read: r,
            cache_write: w,
            seconds: result.duration_api_ms / 1000.0,
            cost_usd: result.total_cost_usd,
        }
    }

    pub fn note(&self, label: &str, outcome: &str) {
        log_run(json!({"label": label, "outcome": outcome, "agents": self.agents}));
    }

    /// The one terminal record for an agent that made at least one call.
    ///
    /// Exactly one per "started": a retry or a budget stop used to log an extra
    /// "ok" first, which claimed the panel row as done and left the real outcome
    /// as an orphan row — a dead agent displayed as finished. This call's usage,
    /// not the run's running totals: the panel sums the records.
    pub fn finish(&self, label: &str, outcome: &str, usage: Usage, retry: bool) {
        log_run(json!({
            "label": label,
            "retry": retry,
            "outcome": outcome,
            "agents": self.agents,
            "input": usage.input,
            "output": usage.output,
            "cache_read": usage.cache_read,
            "seconds": round1(usage.seconds),
            "cost_usd": round4(usage.cost_usd),
        }));
    }

    pub fn summary(&self) -> String {
        // Everything the model processed, cache included. Fresh input plus
        // output alone read 0.01M for a run that processed 0.73M — cache reads
        // and writes are most of a run, and most of what it costs.
        let total =
            self.input_tokens + self.output_tokens + self.cache_read + self.cache_write;
        format!(
            "agents={} cache_hits={} dead={}\ntokens: {:.2}M processed \
(fresh in {:.2}M / out {:.1}k / cache read {:.2}M / write {:.2}M)\n\
cost: ${:.2} at API prices\napi time: {:.0}s across {} agents",
            self.agents,
            self.cache_hits,
            self.dead,
            total as f64 / 1e6,
            self.input_tokens as f64 / 1e6,
            self.output_tokens as f64 / 1e3,
            self.cache_read as f64 / 1e6,
            self.cache_write as f64 / 1e6,
            self.cost_usd,
            self.seconds,
            self.agents,
        )
    }
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Drop group/other bits from `path` if it has any.
///
/// `mkdir_private` and `OpenOptions::mode` only take effect at creation. The
/// run log is appended to in place, so one written by an earlier build kept
/// 0644 and stayed world-readable.
#[cfg(unix)]
fn tighten(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn tighten(_path: &Path, _mode: u32) {}

static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();

fn stats() -> &'static Mutex<Stats> {
    STATS.get_or_init(|| Mutex::new(Stats::default()))
}

/// Lock the process-global stats. Held only for the few field updates, never
/// across an `.await`.
pub fn with_stats<T>(f: impl FnOnce(&mut Stats) -> T) -> T {
    let mut guard = stats().lock().expect("stats mutex poisoned");
    f(&mut guard)
}

/// `~/.cache/rustflows-runs/run.jsonl` unless `RUSTFLOWS_RUN_LOG` says otherwise.
///
/// Deliberately NOT inside the cache dir — `rm -rf ~/.cache/rustflows` is how
/// you force a cold run, and it must not delete the record of what that run cost.
pub fn run_log_path() -> PathBuf {
    crate::env_path("RUSTFLOWS_RUN_LOG", "~/.cache/rustflows-runs/run.jsonl")
}

/// One JSON line per stage, so a finished run can be interrogated per agent.
pub fn log_run(record: Value) {
    let path = run_log_path();
    let mut line = match serde_json::to_string(&record) {
        Ok(s) => s,
        Err(_) => return,
    };
    line.push('\n');
    // Logging must never take down a run.
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            crate::cache::mkdir_private(parent)?;
            tighten(parent, 0o700);
        }
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            // The run log quotes the same source excerpts as the cache, and
            // outlives it: `rm -rf ~/.cache/rustflows` does not touch this.
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut fh = opts.open(&path)?;
        fh.write_all(line.as_bytes())?;
        // `.mode(0o600)` applies only when the file is created; appending to an
        // existing run log kept whatever mode it already had, so a log written
        // by an older build stayed world-readable.
        tighten(&path, 0o600);
        Ok(())
    };
    let _ = write();
}

/// The newest run only, parsed without touching the older ones.
///
/// `watch` redraws every 0.5s (as fast as 0.05s) and only ever renders the
/// last run; parsing the whole file on each tick meant every historical record
/// was re-deserialized for nothing. The file is still read whole — it is
/// append-only and unindexed — so this trades parsing for reading. Rotate the
/// log if that ever matters.
///
/// ponytail: whole-file read. An offset-tracked tail is the upgrade if a run
/// log gets large enough for the read to show.
pub fn read_last_run() -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(run_log_path()) else {
        return Vec::new();
    };
    let start = text
        .rfind("\"outcome\":\"run_start\"")
        .map(|at| text[..at].rfind('\n').map_or(0, |nl| nl + 1))
        .unwrap_or(0);

    let mut run = Vec::new();
    for line in text[start..].lines() {
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            run.push(record);
        }
    }
    run
}

/// Split the run log into runs. A `run_start` record begins each one.
pub fn read_runs() -> Vec<Vec<Value>> {
    let Ok(text) = std::fs::read_to_string(run_log_path()) else {
        return Vec::new();
    };
    let mut runs: Vec<Vec<Value>> = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let starts = record.get("outcome").and_then(Value::as_str) == Some("run_start");
        if starts || runs.is_empty() {
            runs.push(Vec::new());
        }
        runs.last_mut().expect("just pushed").push(record);
    }
    runs
}

/// Countdown for the whole run. Not a token meter — a blast radius.
pub struct Spend {
    pub max_calls: usize,
    pub remaining: Mutex<usize>,
}

impl Spend {
    pub fn new(max_calls: usize) -> Self {
        Self {
            max_calls,
            remaining: Mutex::new(max_calls),
        }
    }

    pub fn take(&self) -> Result<(), crate::OrchError> {
        let mut remaining = self.remaining.lock().expect("spend mutex poisoned");
        if *remaining == 0 {
            return Err(crate::OrchError::Budget(format!(
                "exhausted {}-call budget",
                self.max_calls
            )));
        }
        *remaining -= 1;
        Ok(())
    }
}

pub fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_labels_get_a_counter_from_the_second_use() {
        let mut s = Stats::default();
        assert_eq!(s.unique("find/correctness"), "find/correctness");
        assert_eq!(s.unique("find/correctness"), "find/correctness#2");
        assert_eq!(s.unique("find/correctness"), "find/correctness#3");
        assert_eq!(s.unique(""), "");
    }

    #[test]
    fn record_prefers_flat_usage_then_falls_back_per_model() {
        let flat = CallResult {
            usage: json!({"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 7}),
            model_usage: json!({"m": {"inputTokens": 999}}),
            ..Default::default()
        };
        let s = Stats::default();
        let u = { let mut s = s; s.record(&flat) };
        assert_eq!((u.input, u.output, u.cache_read), (10, 5, 7));

        let nested = CallResult {
            usage: json!({}),
            model_usage: json!({"m": {"inputTokens": 4, "outputTokens": 2}}),
            ..Default::default()
        };
        let mut s = Stats::default();
        let u = s.record(&nested);
        assert_eq!((u.input, u.output), (4, 2));
    }

    #[test]
    fn read_last_run_returns_only_the_newest_run() {
        // `watch` used to parse every historical run on every tick.
        let dir = std::env::temp_dir().join(format!("rf-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("run.jsonl");
        let line = |s: Value| serde_json::to_string(&s).unwrap();
        std::fs::write(
            &log,
            [
                line(json!({"outcome": "run_start", "mode": "full"})),
                line(json!({"label": "old", "outcome": "started"})),
                line(json!({"outcome": "run_start", "mode": "quick"})),
                line(json!({"label": "new", "outcome": "started"})),
            ]
            .join("\n"),
        )
        .unwrap();

        // SAFETY: the only writer of the environment in this test.
        unsafe { std::env::set_var("RUSTFLOWS_RUN_LOG", &log) };
        let last = read_last_run();
        assert_eq!(last.len(), 2);
        assert_eq!(last[0]["mode"], "quick");
        assert!(last.iter().all(|r| r.get("label").and_then(Value::as_str) != Some("old")));
        unsafe { std::env::remove_var("RUSTFLOWS_RUN_LOG") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spend_stops_at_zero() {
        let spend = Spend::new(2);
        assert!(spend.take().is_ok());
        assert!(spend.take().is_ok());
        assert!(spend.take().is_err());
    }

    #[test]
    fn add_usage_sums_every_field() {
        let a = Usage { input: 1, output: 2, cache_read: 3, cache_write: 4, seconds: 0.5, cost_usd: 0.01 };
        let b = Usage { input: 10, output: 20, cache_read: 30, cache_write: 40, seconds: 1.5, cost_usd: 0.02 };
        let got = add_usage(a, b);
        assert_eq!((got.input, got.output, got.cache_read, got.cache_write), (11, 22, 33, 44));
        assert!((got.seconds - 2.0).abs() < 1e-9);
        assert!((got.cost_usd - 0.03).abs() < 1e-9);
    }
}