//! Terminal view of the run log. Framework-level: renders any workflow.
//!
//! Knows nothing about review or research — it reads the run log, which the
//! framework writes, and nothing else. A third workflow is observable for free.

use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;

use crate::clean::clean_text;
use crate::stats::{read_last_run, read_runs, run_log_path};

/// The stage column below is the contract every workflow labels against.
const STAGE_COL: usize = 32;

fn outcome_style(outcome: &str) -> (&'static str, &'static str) {
    match outcome {
        "started" => ("running", ""),
        "ok" => ("done", ""),
        "cache_hit" => ("cached", "served from cache, cost nothing"),
        "failed" => ("FAIL", "agent call raised"),
        "failed_retry" => ("FAIL", "retry after schema mismatch raised"),
        "invalid" => ("FAIL", "never produced schema-valid output"),
        "invalid_schema" => ("FAIL", "schema would not compile"),
        "error" => ("FAIL", "the CLI reported an error"),
        "no_output" => ("FAIL", "returned no structured output"),
        "over_budget" => ("BUDGET", "hit its per-call USD ceiling"),
        _ => ("?", ""),
    }
}

type Row = [String; 5];

/// Collapse start/finish pairs into one row per agent, in start order.
///
/// A "started" record with no terminal record after it is still running.
///
/// Labels are not unique — two agents can share one — so start/finish are
/// paired by occurrence: each finish claims the oldest unclaimed start with a
/// matching label. Keying on the label alone let two agents sharing one label
/// both write to the first row while the second hung at "running" forever.
fn stage_rows(run: &[Value]) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut pending: HashMap<String, Vec<usize>> = HashMap::new();

    for record in run {
        if record.get("outcome").and_then(Value::as_str) == Some("run_start") {
            continue;
        }
        let label = record
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        let outcome = record.get("outcome").and_then(Value::as_str).unwrap_or("");
        let queue = pending.entry(label.clone()).or_default();

        if outcome == "started" {
            queue.push(rows.len());
            rows.push([label, "running".into(), String::new(), String::new(), String::new()]);
            continue;
        }

        let usage_cells = |row: &mut Row, record: &Value| {
            // Any terminal record with usage, not just "ok": a FAIL or BUDGET
            // row that made calls still spent tokens.
            if record.get("input").is_some() {
                row[2] = k(record.get("input"));
                row[3] = k(record.get("output"));
                row[4] = format!("{:.1}", record.get("seconds").and_then(Value::as_f64).unwrap_or(0.0));
            }
        };

        if !queue.is_empty() {
            let index = queue.remove(0); // oldest unclaimed start for this label
            let row = &mut rows[index];
            row[1] = outcome_style(outcome).0.to_string();
            usage_cells(row, record);
        } else {
            // A terminal record with no start (older logs, or a wiped log).
            let mut row = [label, outcome_style(outcome).0.to_string(), "-".into(), "-".into(), "-".into()];
            usage_cells(&mut row, record);
            rows.push(row);
        }
    }
    rows
}

fn k(v: Option<&Value>) -> String {
    let n = v.and_then(Value::as_u64).unwrap_or(0) as f64 / 1000.0;
    format!("{n:.1}k")
}

pub fn render(run: &[Value]) -> String {
    let start = run.first();
    let mode = start
        .and_then(|s| s.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("full");

    // Research fans out over search angles; review over code lenses; decide over
    // a bracket field. Same field, different word — calling angles "lenses"
    // reads as a bug.
    let count = start.and_then(|s| s.get("lenses"));
    let (one, many) = match mode {
        "research" => ("angle", "angles"),
        "decide" => ("option", "options"),
        "upgrade" => ("dependency", "dependencies"),
        _ => ("lens", "lenses"),
    };
    // Upgrade logs no count: its fan-out width is not known until the inventory
    // lands. "None lenss" was what that rendered as.
    let fanout = match count {
        None | Some(Value::Null) => String::new(),
        Some(v) => {
            let n = v.as_u64().unwrap_or(0);
            format!(", {n} {}", if n == 1 { one } else { many })
        }
    };

    // The log is replayed raw; a label or target carrying ESC would drive the
    // terminal. One line each — a newline would break the table.
    let mut target = clean_text(
        &start
            .and_then(|s| s.get("target"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "?".into()),
        false,
    );
    // Questions are long and arbitrary; paths are short. Truncate the question.
    if mode == "research" && target.chars().count() > 58 {
        target = target.chars().take(57).collect::<String>() + "…";
    }

    let rows = stage_rows(run);
    let mut lines = vec![
        format!("{target}  [{mode}{fanout}]"),
        format!(
            "{:<STAGE_COL$} {:<8} {:>8} {:>7} {:>6}",
            "stage", "status", "in", "out", "sec"
        ),
    ];

    for row in &rows {
        let label = clean_text(&row[0], false);
        // Truncate from the left: the tail of a path is the useful half.
        let stage = if label.chars().count() <= STAGE_COL {
            label
        } else {
            let tail: String = label
                .chars()
                .skip(label.chars().count() - (STAGE_COL - 1))
                .collect();
            format!("…{tail}")
        };
        lines.push(format!(
            "{:<STAGE_COL$} {:<8} {:>8} {:>7} {:>6}",
            stage, row[1], row[2], row[3], row[4]
        ));
    }

    let failed = rows.iter().filter(|r| r[1] == "FAIL").count();
    let cached = run
        .iter()
        .filter(|r| r.get("outcome").and_then(Value::as_str) == Some("cache_hit"))
        .count();
    let running = rows.iter().filter(|r| r[1] == "running").count();
    let spent: f64 = run
        .iter()
        .map(|r| r.get("input").and_then(Value::as_i64).unwrap_or(0) as f64)
        .sum();

    lines.push("-".repeat(66));
    lines.push(format!(
        "{} stages · {failed} failed · {cached} cached · {:.1}k input tokens{}",
        rows.len(),
        spent / 1000.0,
        if running > 0 {
            format!(" · {running} running")
        } else {
            String::new()
        }
    ));
    lines.join("\n")
}

/// Browse agents from past runs — the poor man's /workflows panel.
pub fn show_agents(last: usize, as_json: bool) -> i32 {
    let runs = read_runs();
    if runs.is_empty() {
        println!("no runs logged yet ({})", run_log_path().display());
        return 0;
    }
    let start = runs.len().saturating_sub(last);
    for run in &runs[start..] {
        if as_json {
            println!("{}", serde_json::to_string_pretty(run).unwrap_or_default());
        } else {
            println!("\n{}", render(run));
        }
    }
    0
}

/// Live panel: redraw the latest run as its agents report in.
///
/// Always the newest run — watching an older one has no meaning.
pub async fn watch(interval: f64) -> i32 {
    let period = std::time::Duration::from_secs_f64(interval.max(0.05));
    let mut shown = 0usize;
    loop {
        let run = read_last_run();
        // Redraw only when something changed: the log is append-only, so a
        // stable run length means no new stage record since the last tick.
        if !run.is_empty() && run.len() != shown {
            shown = run.len();
            let mut out = std::io::stdout();
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let _ = out.write_all(b"\x1b[2J\x1b[H");
            }
            let _ = writeln!(out, "\n{}\n", render(&run));
            let _ = out.flush();
        }
        tokio::time::sleep(period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(records: Vec<Value>) -> Vec<Value> {
        records
    }

    #[test]
    fn pairs_a_start_with_its_finish_in_order() {
        let rows = stage_rows(&run(vec![
            json!({"label": "a", "outcome": "started"}),
            json!({"label": "b", "outcome": "started"}),
            json!({"label": "a", "outcome": "ok", "input": 1200, "output": 300, "seconds": 1.25}),
        ]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], "done");
        assert_eq!(rows[0][2], "1.2k");
        assert_eq!(rows[0][3], "0.3k");
        assert_eq!(rows[0][4], "1.2");
        assert_eq!(rows[1][1], "running");
    }

    #[test]
    fn a_shared_label_pairs_by_occurrence() {
        // The bug this guards: two agents under one label must not both finish
        // on the first row and leave the second hanging.
        let rows = stage_rows(&run(vec![
            json!({"label": "x", "outcome": "started"}),
            json!({"label": "x", "outcome": "started"}),
            json!({"label": "x", "outcome": "ok", "input": 10}),
            json!({"label": "x", "outcome": "cache_hit"}),
        ]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], "done");
        assert_eq!(rows[1][1], "cached");
    }

    #[test]
    fn an_unmatched_finish_still_gets_a_row() {
        let rows = stage_rows(&run(vec![json!({"label": "z", "outcome": "failed"})]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], "FAIL");
        assert_eq!(rows[0][2], "-");
    }

    #[test]
    fn a_failed_row_that_made_calls_still_shows_its_tokens() {
        let rows = stage_rows(&run(vec![
            json!({"label": "z", "outcome": "started"}),
            json!({"label": "z", "outcome": "over_budget", "input": 5000, "output": 100, "seconds": 9.0}),
        ]));
        assert_eq!(rows[0][1], "BUDGET");
        assert_eq!(rows[0][2], "5.0k");
        assert_eq!(rows[0][4], "9.0");
    }

    #[test]
    fn render_uses_the_mode_s_word_for_the_fan_out() {
        let out = render(&run(vec![
            json!({"outcome": "run_start", "target": "src", "lenses": 1, "mode": "review"}),
            json!({"label": "find/correctness", "outcome": "started"}),
        ]));
        assert!(out.starts_with("src  [review, 1 lens]"), "{out}");
    }

    #[test]
    fn render_omits_the_count_when_it_was_never_logged() {
        let out = render(&run(vec![json!({"outcome": "run_start", "target": ".", "mode": "upgrade"})]));
        assert!(out.contains("[upgrade]"), "{out}");
    }

    #[test]
    fn a_long_label_keeps_its_tail() {
        let long = format!("/very/long/path/{}/deep.py", "x".repeat(60));
        let out = render(&run(vec![
            json!({"outcome": "run_start", "target": ".", "lenses": 1, "mode": "review"}),
            json!({"label": long, "outcome": "started"}),
        ]));
        // Truncated from the left, so the useful tail survives.
        assert!(out.contains("/deep.py"), "{out}");
        assert!(out.contains('…'), "{out}");
    }

    #[test]
    fn render_strips_terminal_control_sequences() {
        let out = render(&run(vec![
            json!({"outcome": "run_start", "target": "\u{1b}[2Jevil\nsrc", "lenses": 2, "mode": "review"}),
        ]));
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("[2Jevil src"), "{out}");
    }
}