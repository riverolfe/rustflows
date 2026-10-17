//! Example workflow: review a repo through independent lenses, then refute.
//!
//! Shape is find -> barrier -> dedup -> verify -> confirm. This is the pipeline
//! you'd write in a JS workflow script, in Rust.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ext::Extensions;
use crate::stats::{Spend, with_stats};
use crate::{AskOpts, MAX_CALLS, OrchError, ask, gather};

pub const FINDINGS: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "line": {"type": "integer"},
                    "summary": {"type": "string"},
                    "failure": {"type": "string"},
                },
                "required": ["file", "line", "summary", "failure"],
            },
        }
    },
    "required": ["findings"],
});

/// One agent judges a BATCH of findings. Reading the source dominates the cost,
/// so judging 5 in one call is far cheaper than 5 calls that each re-read it.
pub const BATCH: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "index": {"type": "integer"},
                    "refuted": {"type": "boolean"},
                    "reason": {"type": "string"},
                },
                "required": ["index", "refuted", "reason"],
            },
        }
    },
    "required": ["verdicts"],
});

/// Findings per verifying agent. 1 is the old one-agent-per-finding behaviour,
/// useful for debugging a single suspect finding.
pub fn batch_size_default() -> usize {
    std::env::var("RUSTFLOWS_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(5)
}

/// Quick mode: one lens, one verifier, one batch. A fraction of the cost and
/// wall clock, with no independent cross-check.
pub const QUICK_LENSES: &[&str] = &["correctness"];

pub const LENSES: &[&str] = &["correctness", "security", "performance", "error-handling"];

/// Run only when named with `--lens`.
pub const OPT_IN_LENSES: &[&str] = &["test-quality"];

fn lens_focus(lens: &str) -> Option<&'static str> {
    match lens {
        "test-quality" => Some(
            "Review the tests and what they leave uncovered. Report:\n\
- tests that cannot fail: no assertion, assertions on a fake or mock instead of the \
code, tautologies, exceptions swallowed around the call under test;\n\
- tests that pass for the wrong reason: wrong fixture, a patched target that is never \
called, dependence on test order or shared state, sleeps standing in for synchronisation;\n\
- flakiness sources: wall clock, unseeded randomness, network, real paths under ~;\n\
- missing coverage: behaviour no test would catch if it were removed or broken. Point \
at the untested code's file and line, not at a test file.\n\
No style, naming or test-count findings.",
        ),
        _ => None,
    }
}

fn lens_refute(lens: &str) -> Option<&'static str> {
    match lens {
        "test-quality" => Some(
            "Claims marked [test-quality] are about tests. You cannot run them; judge by \
reading. A claim that a test cannot fail is refuted if you can name a code change that \
would make it fail. A claim that behaviour is uncovered is refuted if you find a test that \
exercises it and asserts on its result.",
        ),
        _ => None,
    }
}

/// Dedup key. Two lenses may legitimately report the same file:line for two
/// different reasons, so the lens is always part of the key.
///
/// It used to be added only for an opt-in lens, which silently dropped
/// `error-handling` findings wherever `correctness` — which runs first, and
/// overlaps it most — had already claimed the line. Nothing counted the drop,
/// so the lens read as one that had found nothing.
fn key(f: &Value) -> String {
    format!(
        "{}:{}:{}",
        f.get("file").and_then(Value::as_str).unwrap_or(""),
        f.get("line").and_then(Value::as_i64).unwrap_or(0),
        f.get("lens").and_then(Value::as_str).unwrap_or("")
    )
}

/// Directories the agent must not read.
///
/// `sign::SKIP_DIRS` prunes these from the *cache fingerprint*, but that says
/// nothing to the agent: its own Glob and Grep walked `target/` freely — 19,721
/// files against 38 source files on this crate, and the single largest cost in
/// a review. Derived from the same list so the two cannot drift.
fn skip_note() -> String {
    format!(
        "Do not read any path containing a {} directory: they are build output or \
vendored dependencies, not the code under review.",
        crate::sign::SKIP_DIRS.join(" or ")
    )
}

fn finder_prompt(target: &str, lens: &str) -> String {
    let mut prompt = format!(
        "Review {target} for {lens} problems. Report only defects you can point at a\
specific file and line.\n\n{}",
        skip_note()
    );
    if let Some(focus) = lens_focus(lens) {
        prompt.push_str("\n\n");
        prompt.push_str(focus);
    }
    prompt
}

fn claim(i: usize, f: &Value) -> String {
    let lens = f.get("lens").and_then(Value::as_str).unwrap_or("");
    let tag = if lens_refute(lens).is_some() {
        format!("[{lens}] ")
    } else {
        String::new()
    };
    format!(
        "[{i}] {tag}{}:{} — {}\n    Failure it would cause: {}",
        f.get("file").and_then(Value::as_str).unwrap_or(""),
        f.get("line").and_then(Value::as_i64).unwrap_or(0),
        f.get("summary").and_then(Value::as_str).unwrap_or(""),
        f.get("failure").and_then(Value::as_str).unwrap_or("")
    )
}

fn refute_prompt(target: &str, batch: &[Value]) -> String {
    // Order-preserving, deduplicated: the rule text depends on which lenses are
    // present in this batch, and must be added once each.
    let mut seen = HashSet::new();
    let mut rules = Vec::new();
    for f in batch {
        let lens = f.get("lens").and_then(Value::as_str).unwrap_or("").to_string();
        if !seen.insert(lens.clone()) {
            continue;
        }
        if let Some(rule) = lens_refute(&lens) {
            rules.push(rule);
        }
    }
    let claims: Vec<String> = batch.iter().enumerate().map(|(i, f)| claim(i, f)).collect();
    format!(
        "Try to REFUTE each claim about {target}. Read the actual code once, then judge \
every claim.\n\n{}\n\nDefault to refuted=true if the code does not back a claim up. Return \
one verdict per index, {} in total, by calling the StructuredOutput tool.{}{}",
        claims.join("\n\n"),
        batch.len(),
        skip_note(),
        rules.iter().map(|r| format!("\n\n{r}")).collect::<String>()
    )
}

#[derive(Debug)]
pub struct ReviewFailed(pub String);

#[derive(Debug, Default)]
pub struct ReviewResult {
    pub confirmed: Vec<Value>,
    pub unconfirmed: usize,
    pub dead: usize,
}

pub struct ReviewArgs {
    pub target: PathBuf,
    pub lenses: Vec<String>,
    pub batch_size: Option<usize>,
    pub ext: Extensions,
}

/// Review `target` and return the findings that survived refutation.
///
/// `unconfirmed` is returned, not just printed: a run whose verifier died is a
/// partial review, and the CLI must not exit 0 over findings nobody checked.
///
/// `batch_size` `None` means one verifier per batch of `batch_size_default()`
/// findings; pass a value >= the finding count to force a single verifier.
pub async fn review(args: ReviewArgs) -> Result<ReviewResult, ReviewError> {
    let ReviewArgs { target, lenses, batch_size, ext } = args;
    // A configuration mistake, not a dead agent: `gather` over no futures
    // succeeds, so this used to fall through to "all 0 lenses failed" and exit
    // 1, pointing the reader at agents that never ran.
    if lenses.is_empty() {
        return Err(ReviewError::Failed(
            "no lenses selected — name at least one with --lens".into(),
        ));
    }
    // The CLI checks this too, but `ReviewArgs` is public API and a plain file
    // here is used as the agents' cwd and as the confinement root.
    if let Err(e) = check_target(&target) {
        return Err(ReviewError::Failed(e));
    }
    let target_arg = target.display().to_string();
    // `chunks(0)` panics. The CLI cannot pass 0, but `ReviewArgs` is public API.
    let batch_size = batch_size.unwrap_or_else(batch_size_default).max(1);
    let spend = Arc::new(Spend::new(MAX_CALLS));

    let found = gather(
        lenses
            .iter()
            .map(|lens| {
                let prompt = finder_prompt(&target_arg, lens);
                let label = format!("find/{lens}");
                let system = format!("You review code for {lens} defects. No style opinions.");
                let cwd = target.clone();
                let dep = Some(target.clone());
                let ext = ext.clone();
                let spend = Arc::clone(&spend);
                async move {
                    ask(&prompt, &FINDINGS(), AskOpts {
                        spend: Some(spend),
                        ext,
                        cwd,
                        depends_on: dep,
                        label,
                        system: Some(system),
                        ..Default::default()
                    })
                    .await
                }
            })
            .collect(),
    )
    .await
    .map_err(ReviewError::Orch)?;

    // A dead lens and a lens that found nothing are both None here — count the
    // dead ones, or "reviewed nothing" reads as "found nothing".
    let mut alive: Vec<Vec<Value>> = Vec::new();
    for (lens, group) in lenses.iter().zip(&found) {
        let Some(group) = group else { continue };
        let mut tagged = Vec::new();
        for f in group
            .get("findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let mut f = f;
            f["lens"] = Value::String(lens.clone());
            tagged.push(f);
        }
        alive.push(tagged);
    }
    if alive.is_empty() {
        return Err(ReviewError::Failed(format!(
            "all {} lenses failed",
            lenses.len()
        )));
    }

    // Dedup needs every finder's result at once — the one place a barrier is right.
    let mut seen = HashSet::new();
    let mut unique: Vec<Value> = Vec::new();
    for group in &alive {
        for f in group {
            if seen.insert(key(f)) {
                unique.push(f.clone());
            }
        }
    }

    // Live lenses that found nothing is a clean review, not a dead one: with no
    // batches, "every batch died" below would fire on an empty list.
    if unique.is_empty() {
        println!(
            "no findings from {}/{} lenses",
            alive.len(),
            lenses.len()
        );
        return Ok(ReviewResult {
            confirmed: Vec::new(),
            unconfirmed: 0,
            dead: lenses.len() - alive.len(),
        });
    }

    println!(
        "{} unique findings from {}/{} lenses, verifying…",
        unique.len(),
        alive.len(),
        lenses.len()
    );

    let batches: Vec<Vec<Value>> = unique.chunks(batch_size).map(|c| c.to_vec()).collect();
    let results = gather(
        batches
            .iter()
            .map(|batch| {
                let prompt = refute_prompt(&target_arg, batch);
                let first_file = batch
                    .first()
                    .and_then(|f| f.get("file"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // The panel's stage column is 32 wide, and every sibling workflow bounds its
                // labels. This one interpolates a full agent-supplied path.
                let label = format!("verify/{first_file}:{}", batch.len())
                    .chars()
                    .take(32)
                    .collect::<String>();
                let cwd = target.clone();
                let dep = Some(target.clone());
                let ext = ext.clone();
                let spend = Arc::clone(&spend);
                async move {
                    ask(&prompt, &BATCH(), AskOpts {
                        spend: Some(spend),
                        ext,
                        cwd,
                        depends_on: dep,
                        label,
                        system: Some("You are an adversarial reviewer. Default to refuted=true.".into()),
                        tools: vec!["Read".into(), "Grep".into()],
                        ..Default::default()
                    })
                    .await
                }
            })
            .collect(),
    )
    .await
    .map_err(ReviewError::Orch)?;

    let mut confirmed = Vec::new();
    let mut unconfirmed = 0usize;
    for (batch, result) in batches.iter().zip(&results) {
        let Some(result) = result else {
            unconfirmed += batch.len();
            continue;
        };
        let verdicts: Vec<Value> = result
            .get("verdicts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // First verdict wins for a repeated index, as the dict did.
        let by_index: Vec<(i64, Value)> = {
            let mut seen = HashSet::new();
            verdicts
                .into_iter()
                .map(|v| (v.get("index").and_then(Value::as_i64).unwrap_or(-1), v))
                .filter(|(i, _)| seen.insert(*i))
                .collect()
        };
        for (i, f) in batch.iter().enumerate() {
            match by_index.iter().find(|(idx, _)| *idx == i as i64) {
                // An agent that skipped a finding has not refuted it.
                None => unconfirmed += 1,
                Some((_, v)) => {
                    if !v.get("refuted").and_then(Value::as_bool).unwrap_or(true) {
                        let mut f = f.clone();
                        f["reason"] = v
                            .get("reason")
                            .cloned()
                            .unwrap_or_else(|| Value::String(String::new()));
                        confirmed.push(f);
                    }
                }
            }
        }
    }

    if unconfirmed > 0 {
        println!("  {unconfirmed}/{} findings unconfirmed (failed or skipped)", unique.len());
    }

    // Every verifier dead is a dead run, not a clean one — same rule as the find
    // stage above. Guarded here because the check must be where the evidence
    // is: a batch that died cannot be told from one that refuted.
    if results.iter().all(|r| r.is_none()) {
        return Err(ReviewError::Failed(format!(
            "all {} verification batches failed",
            batches.len()
        )));
    }

    Ok(ReviewResult {
        confirmed,
        unconfirmed,
        dead: lenses.len() - alive.len(),
    })
}

#[derive(Debug)]
pub enum ReviewError {
    /// No lens came back. A dead run must never read as a clean one.
    Failed(String),
    Orch(OrchError),
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewError::Failed(m) | ReviewError::Orch(OrchError::Budget(m)) => write!(f, "{m}"),
            ReviewError::Orch(e) => write!(f, "{e}"),
        }
    }
}

/// Choose the lens list from the CLI flags, preserving first-seen order.
pub fn choose_lenses(chosen: &[String], quick: bool, limit: Option<usize>) -> Vec<String> {
    // Order-preserving dedup. `Vec::dedup` only collapses *adjacent* repeats, so
// `--lens security --lens correctness --lens security` used to fan out over a
// duplicate finder that burned budget and inflated the lens count.
let mut seen = HashSet::new();
let chosen: Vec<String> = chosen
        .iter()
        .filter(|l| seen.insert((*l).clone()))
        .cloned()
        .collect();
    let mut lenses: Vec<String> = if chosen.is_empty() {
        if quick {
            QUICK_LENSES.iter().map(|s| s.to_string()).collect()
        } else {
            LENSES.iter().map(|s| s.to_string()).collect()
        }
    } else {
        chosen
    };
    if let Some(n) = limit {
        lenses.truncate(n);
    }
    lenses
}

/// True when `lens` is a lens this workflow knows.
pub fn known_lens(lens: &str) -> bool {
    LENSES.contains(&lens) || OPT_IN_LENSES.contains(&lens)
}

pub fn summary_line() -> String {
    with_stats(|s| s.summary())
}

/// Print the confirmed findings the way the CLI reports them.
pub fn print_confirmed(confirmed: &[Value]) {
    for f in confirmed {
        let lens = f.get("lens").and_then(Value::as_str).unwrap_or("");
        let tag = if OPT_IN_LENSES.contains(&lens) {
            format!("[{lens}] ")
        } else {
            String::new()
        };
        println!(
            "\n{tag}{}:{}  {}\n  {}",
            f.get("file").and_then(Value::as_str).unwrap_or(""),
            f.get("line").and_then(Value::as_i64).unwrap_or(0),
            f.get("summary").and_then(Value::as_str).unwrap_or(""),
            f.get("reason").and_then(Value::as_str).unwrap_or("")
        );
    }
}

/// Convenience: `target` exists and is a directory we can hand to an agent.
///
/// A plain file passes `exists()`, but it is used as the agents' cwd and as the
/// confinement root, so accepting one burned a full budget of calls against a
/// tree the agents could not read.
pub fn check_target(target: &Path) -> Result<(), String> {
    if target.is_dir() {
        Ok(())
    } else if target.exists() {
        Err(format!("not a directory: {}", target.display()))
    } else {
        Err(format!("no such path: {}", target.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::schema_error;

    #[test]
    fn dedup_key_separates_two_default_lenses_on_the_same_line() {
        // The bug: error-handling overlapped correctness on almost every line,
        // and correctness ran first, so error-handling was silently swallowed.
        let correctness = json!({"file": "lib.rs", "line": 289, "lens": "correctness"});
        let error_handling = json!({"file": "lib.rs", "line": 289, "lens": "error-handling"});
        assert_ne!(
            key(&correctness),
            key(&error_handling),
            "no default lens may be swallowed by another on the same line"
        );

        let mut other = correctness.clone();
        other["line"] = json!(290);
        assert_ne!(key(&correctness), key(&other));
    }

    #[test]
    fn dedup_key_still_collapses_two_claims_by_one_lens() {
        // Within one lens, file:line is the identity — two claims about the
        // same line are one claim.
        let security = json!({"file": "a.py", "line": 3, "lens": "security"});
        let mut seen = HashSet::new();
        assert!(seen.insert(key(&security)));
        assert!(!seen.insert(key(&security)));
    }

    #[test]
    fn a_non_adjacent_repeat_of_a_lens_is_still_deduplicated() {
        // `Vec::dedup` only collapses neighbours, so this used to fan out over
        // a duplicate finder that burned budget and inflated the lens count.
        assert_eq!(
            choose_lenses(
                &["security".into(), "correctness".into(), "security".into()],
                false,
                None
            ),
            vec!["security", "correctness"]
        );
    }

    #[test]
    fn lens_selection_respects_flags_in_order() {
        assert_eq!(choose_lenses(&[], false, None), LENSES);
        assert_eq!(choose_lenses(&[], true, None), QUICK_LENSES);
        assert_eq!(
            choose_lenses(&["security".into(), "correctness".into()], false, None),
            vec!["security", "correctness"]
        );
        assert_eq!(
            choose_lenses(&["security".into(), "security".into()], false, None),
            vec!["security"]
        );
        assert_eq!(choose_lenses(&[], false, Some(2)), vec!["correctness", "security"]);
    }

    #[test]
    fn refute_prompt_adds_each_lens_rule_once() {
        let a = json!({"file": "t.py", "line": 1, "summary": "s", "failure": "f", "lens": "test-quality"});
        let b = json!({"file": "u.py", "line": 2, "summary": "s", "failure": "f", "lens": "test-quality"});
        let prompt = refute_prompt("src", &[a, b]);
        assert_eq!(prompt.matches("Claims marked [test-quality]").count(), 1);
        assert!(prompt.contains("2 in total"));
    }

    #[test]
    fn refute_prompt_carries_no_rule_for_a_default_lens() {
        let f = json!({"file": "a.py", "line": 1, "summary": "s", "failure": "f", "lens": "security"});
        assert!(!refute_prompt("src", &[f]).contains("Claims marked"));
    }

    #[test]
    fn finder_prompt_tells_the_agent_to_skip_build_output() {
        // target/ alone held 19,721 files against 38 source files; the agent's
        // own Glob and Grep were never told about it.
        let prompt = finder_prompt(".", "security");
        assert!(prompt.contains("target"), "{prompt}");
        assert!(prompt.contains("node_modules"), "{prompt}");
        // And the refute prompt reads the code too.
        let f = json!({"file": "a.py", "line": 1, "summary": "s", "failure": "f", "lens": "security"});
        assert!(refute_prompt(".", &[f]).contains("target"));
    }

    #[test]
    fn finder_prompt_appends_focus_only_when_the_lens_has_one() {
        assert!(finder_prompt("src", "security").starts_with("Review src for security"));
        assert!(!finder_prompt("src", "security").contains("Review the tests"));
        assert!(finder_prompt("src", "test-quality").contains("Review the tests"));
    }

    #[test]
    fn both_schemas_demand_their_required_key() {
        assert!(schema_error(&json!({}), &FINDINGS()).unwrap().is_some());
        assert!(schema_error(&json!({"findings": []}), &FINDINGS()).unwrap().is_none());
        assert!(schema_error(&json!({"verdicts": []}), &BATCH()).unwrap().is_none());
        // Both workflow schemas compile: a stage that cannot compile one would
        // now be refused outright rather than validate everything.
        assert!(crate::schema::validator(&FINDINGS()).is_ok());
        assert!(crate::schema::validator(&BATCH()).is_ok());
    }

    #[test]
    fn known_lenses_include_the_opt_in_one() {
        assert!(known_lens("correctness"));
        assert!(known_lens("test-quality"));
        assert!(!known_lens("style"));
    }

    #[test]
    fn check_target_rejects_a_file_even_though_it_exists() {
        // The doc promised a directory and the code tested only `exists()`, so
        // `rustflows review some.rs` cleared the CLI's one precondition.
        let dir = std::env::temp_dir().join("rustflows-target-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        std::fs::write(&file, "// not a tree\n").unwrap();

        assert!(check_target(&dir).is_ok());
        assert!(check_target(&file).unwrap_err().contains("not a directory"));
        assert!(
            check_target(&dir.join("missing"))
                .unwrap_err()
                .contains("no such path")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}