//! Example workflow: audit dependency upgrades against a repo.
//!
//! Shape is inventory -> sweep -> verify. The interesting thing here is that the
//! fan-out width is **discovered at runtime**: the inventory returns the
//! dependency list, and that decides how many agents run. It is the first
//! workflow where the call count is not known before the run starts, and the
//! reason `MAX_CALLS` is a real constraint rather than a formality.
//!
//! Ported from `pyflows/examples/upgrade.py`. See README "Deliberate
//! differences from pyflows" for where this diverges.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::ext::Extensions;
use crate::stats::Spend;
use crate::{AskFn, MAX_CALLS, OrchError, gather};

/// Manifests to look for, in preference order. `package.json` first: a repo with
/// both a JS and a Python manifest is a JS repo more often than not, and the
/// wrong one costs a whole audit.
pub const MANIFESTS: &[&str] = &[
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "Cargo.toml",
    "Gemfile",
];

/// A dependency agent reads this repo *and* the web. Without both it either
/// finds no changelog or reports breaks this repo never hits.
pub const TOOLS: &[&str] = &["Read", "Grep", "Glob", "WebSearch", "WebFetch"];

const LABEL_MAX: usize = 32;

/// How many candidate breaks one verifier judges. Reading the source dominates
/// the cost, so judging 5 in one call is far cheaper than 5 that each re-read it.
const VERIFY_BATCH: usize = 5;

pub const INVENTORY: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "manifest": {"type": "string"},
        "dependencies": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "current": {"type": "string"},
                    // Direct deps break callers; a transitive bump is worth a
                    // look but not a dedicated agent.
                    "direct": {"type": "boolean"},
                    "uses": {"type": "integer"},
                },
                "required": ["name", "current", "direct", "uses"],
            },
        },
    },
    "required": ["manifest", "dependencies"],
});

pub const BREAKING: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        // Required, and the load-bearing field of this schema. A web tool that
        // fails still leaves the agent able to return a clean, schema-valid "no
        // breaks" without ever reading a changelog; one that consulted nothing
        // is `unchecked`, not audited.
        "sources_checked": {"type": "array", "items": {"type": "string"}},
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "line": {"type": "integer"},
                    "dependency": {"type": "string"},
                    "summary": {"type": "string"},
                    // What breaks *here*, not upstream's wording of it.
                    "failure": {"type": "string"},
                    "url": {"type": "string"},
                },
                "required": ["file", "line", "dependency", "summary", "failure", "url"],
            },
        },
    },
    "required": ["findings", "sources_checked"],
});

/// Note the field is `breaks_here`, not `refuted`: the question is not "is this
/// upstream change real" but "does it hit this repo".
pub const VERDICTS: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "index": {"type": "integer"},
                    "breaks_here": {"type": "boolean"},
                    "reason": {"type": "string"},
                },
                "required": ["index", "breaks_here", "reason"],
            },
        }
    },
    "required": ["verdicts"],
});

/// A candidate break that nothing settled: nobody verified it, or the verifier
/// skipped it. Never the same as one that was refuted.
#[derive(Debug, Clone)]
pub struct Unconfirmed {
    pub finding: Value,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct UpgradeResult {
    pub target: PathBuf,
    pub dependencies: usize,
    /// Agents that consulted at least one changelog.
    pub audited: usize,
    /// Agents that consulted nothing. Not audited, and never counted as safe.
    pub unchecked: usize,
    pub dead: usize,
    /// Dependencies the call budget could not afford. Also not audited.
    pub skipped: usize,
    pub confirmed: Vec<Value>,
    pub refuted: Vec<Value>,
    pub unconfirmed: Vec<Unconfirmed>,
    pub elapsed: f64,
}

pub struct UpgradeArgs {
    pub target: PathBuf,
    /// `None` audits everything the inventory found.
    pub limit: Option<usize>,
    /// Candidate breaks to verify. The rest are unconfirmed, not safe.
    pub depth: usize,
    pub verify: bool,
    pub spend: Option<Arc<Spend>>,
    pub ext: Extensions,
    pub asker: AskFn,
}

#[derive(Debug)]
pub enum UpgradeError {
    Failed(String),
    Orch(OrchError),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::Failed(m) | UpgradeError::Orch(OrchError::Budget(m)) => write!(f, "{m}"),
            UpgradeError::Orch(e) => write!(f, "{e}"),
        }
    }
}

impl From<OrchError> for UpgradeError {
    fn from(e: OrchError) -> Self {
        UpgradeError::Orch(e)
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn int_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn label(prefix: &str, what: &str) -> String {
    format!("{prefix}{what}").chars().take(LABEL_MAX).collect()
}

/// The manifest this repo uses, if any. Checked before spending a call.
pub fn find_manifest(target: &Path) -> Option<&'static str> {
    MANIFESTS.iter().copied().find(|m| target.join(m).is_file())
}

/// Rank the dependencies worth a dedicated agent: direct first, then most-used.
///
/// The name tiebreak is not cosmetic. The cache keys on the prompt, so a
/// non-deterministic pick would audit a different set of dependencies on run 2
/// than run 1 and the cached answer would be for a different question.
pub fn pick(deps: &[Value], limit: Option<usize>) -> Vec<Value> {
    let mut ranked: Vec<Value> = deps
        .iter()
        .filter(|d| !str_field(d, "name").is_empty())
        .cloned()
        .collect();
    ranked.sort_by(|a, b| {
        let direct = bool_field(a, "direct").cmp(&bool_field(b, "direct")).reverse();
        direct
            .then(int_field(b, "uses").cmp(&int_field(a, "uses")))
            .then(str_field(a, "name").cmp(&str_field(b, "name")))
    });
    match limit {
        Some(n) => ranked.into_iter().take(n).collect(),
        None => ranked,
    }
}

fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The dedup key is the 3-tuple, not `file:line`: two dependencies breaking the
/// same line are two findings, and collapsing them would hide one of them.
fn key(f: &Value) -> String {
    format!(
        "{}:{}:{}",
        str_field(f, "file"),
        int_field(f, "line"),
        str_field(f, "dependency")
    )
}

fn inventory_prompt(target: &str) -> String {
    format!(
        "In {target}, find the dependency manifest and list its dependencies.\n\nLook for: {}. \
         Count how many places in the source each dependency is referenced — set `uses` to 0 if \
         you cannot tell. Mark `direct` true only for entries named in the manifest itself, not \
         ones you inferred.",
        MANIFESTS.join(", ")
    )
}

/// The final paragraph is load-bearing and must not be shortened: it is the only
/// thing stopping an agent with broken web tools from reporting a clean
/// "no breaks" having read nothing.
fn dep_prompt(target: &str, name: &str, current: &str) -> String {
    format!(
        "Check whether upgrading `{name}` from {current} to its latest release would break this \
         repository.\n\nFind where {name} is actually used in {target}. Then check the upstream \
         changelog or migration guide for breaking changes between {current} and now. Report a \
         finding ONLY where a specific breaking change hits a specific file in this repo.\n\nIf \
         this repo is unaffected, return an empty findings array — that is the common case and \
         is a valid answer.\n\nList every changelog, release note or migration guide you actually \
         read in `sources_checked`. If your web tools fail or return nothing, list nothing — an \
         empty `sources_checked` is how the caller knows you could not check, and it will not be \
         counted as evidence that this dependency is safe."
    )
}

fn refute_prompt(batch: &[Value]) -> String {
    let claims: Vec<String> = batch
        .iter()
        .enumerate()
        .map(|(i, f)| {
            format!(
                "[{i}] {}:{} — {}\n    Dependency: {}\n    Failure: {}\n    Claimed source: {}",
                str_field(f, "file"),
                int_field(f, "line"),
                str_field(f, "summary"),
                str_field(f, "dependency"),
                str_field(f, "failure"),
                str_field(f, "url")
            )
        })
        .collect();
    format!(
        "Try to REFUTE each claim that upgrading a dependency breaks this repository. Read the \
         actual code at each location.\n\n{}\n\nDefault to breaks_here=false. The cited \
         changelog existing is not evidence that this repo hits it. Return one verdict per \
         claim, keyed by its [n] index.",
        claims.join("\n\n")
    )
}

pub async fn upgrade(args: UpgradeArgs) -> Result<UpgradeResult, UpgradeError> {
    let UpgradeArgs {
        target,
        limit,
        depth,
        verify,
        spend,
        ext,
        asker,
    } = args;
    let started = Instant::now();
    let target_arg = target.display().to_string();
    let spend = spend.unwrap_or_else(|| Arc::new(Spend::new(MAX_CALLS)));

    // Cloned so the closure can borrow it while `target` is still moved into the
// result below.
    let cwd = target.clone();
    let ask_opts = |label: String, system: String, budget: Arc<Spend>| crate::AskOpts {
        tools: TOOLS.iter().map(|s| s.to_string()).collect(),
        ext: ext.clone(),
        cwd: cwd.clone(),
        // The answer depends on files the prompt never quotes, so the tree
        // belongs in the cache key.
        depends_on: Some(cwd.clone()),
        label,
        system: Some(system),
        spend: Some(budget),
        ..Default::default()
    };

    let inventory = asker(
        inventory_prompt(&target_arg),
        INVENTORY(),
        ask_opts(
            "inventory".into(),
            "You read manifests. Report only what the manifest states.".into(),
            Arc::clone(&spend),
        ),
    )
    .await?;
    // A dead inventory is not an empty manifest.
    let Some(inventory) = inventory else {
        return Err(UpgradeError::Failed(
            "could not read a dependency manifest".into(),
        ));
    };

    let deps = pick(
        &inventory
            .get("dependencies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        limit,
    );
    if deps.is_empty() {
        return Err(UpgradeError::Failed(
            "manifest listed no dependencies".into(),
        ));
    }

    // The budget is sized for two calls per dependency — the audit and the
    // verification of whatever it finds — so half of it buys half the field.
    // Truncating silently would audit a prefix and call it a clean run.
    let mut skipped = 0usize;
    let mut deps = deps;
    let room = (spend.remaining() / 2).max(1);
    if deps.len() > room {
        skipped = deps.len() - room;
        deps.truncate(room);
    }
    println!(
        "auditing {}/{} dependencies on {target_arg}{}",
        deps.len(),
        skipped + deps.len(),
        if skipped > 0 {
            format!(" — only {} fit the budget, {skipped} NOT audited", deps.len())
        } else {
            String::new()
        }
    );

    let results = gather(
        deps.iter()
            .map(|d| {
                let prompt = dep_prompt(
                    &target_arg,
                    &str_field(d, "name"),
                    &str_field(d, "current"),
                );
                let label = label("dep/", &str_field(d, "name"));
                let asker = Arc::clone(&asker);
                let opts = ask_opts(
                    label,
                    "You audit one dependency. No upstream changelog that this repo does not \
                     actually hit is a finding."
                        .into(),
                    Arc::clone(&spend),
                );
                async move { asker(prompt, BREAKING(), opts).await }
            })
            .collect(),
    )
    .await?;

    let mut audited = 0usize;
    let mut unchecked = 0usize;
    let mut dead = 0usize;
    let mut findings: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Per result, never inferred for siblings: one agent's web tools failing says
    // nothing about the next dependency's.
    for result in &results {
        let Some(result) = result else {
            dead += 1;
            continue;
        };
        if result
            .get("sources_checked")
            .and_then(Value::as_array)
            .is_some_and(|s| !s.is_empty())
        {
            audited += 1;
        } else {
            unchecked += 1;
        }
        for f in result
            .get("findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if seen.insert(key(&f)) {
                findings.push(f);
            }
        }
    }
    if dead == deps.len() {
        return Err(UpgradeError::Failed(format!(
            "all {} dependency agents failed",
            deps.len()
        )));
    }
    if unchecked > 0 {
        println!(
            "  {unchecked}/{} dependencies consulted no source — those are NOT audited",
            audited + unchecked
        );
    }
    if audited == 0 && verify {
        return Err(UpgradeError::Failed(format!(
            "no agent could reach a changelog ({unchecked} consulted no source) — this looks like \
             the web tools are unavailable, not that the deps are safe"
        )));
    }

    let mut out = UpgradeResult {
        target,
        dependencies: skipped + deps.len(),
        audited,
        unchecked,
        dead,
        skipped,
        elapsed: 0.0,
        ..Default::default()
    };

    // A finding whose URL is empty, or points somewhere an agent must not fetch,
    // cannot be verified: a verifier handed a claim with no source it can
    // actually read can only guess. It goes to `unconfirmed` before the verify
    // fan-out, never silently dropped.
    let (mut checkable, unverifiable): (Vec<Value>, Vec<Value>) =
        findings.into_iter().partition(|f| crate::safe_url(&str_field(f, "url")));
    out.unconfirmed.extend(unverifiable.into_iter().map(|finding| Unconfirmed {
        finding,
        why: "no changelog URL".into(),
    }));
    if out.unconfirmed.iter().any(|u| u.why == "no changelog URL") {
        println!("  a finding with no readable changelog URL cannot be verified — listed as unconfirmed");
    }

    if !verify || checkable.is_empty() {
        out.unconfirmed.extend(checkable.into_iter().map(|finding| Unconfirmed {
            finding,
            why: "verification skipped".into(),
        }));
        out.elapsed = started.elapsed().as_secs_f64();
        return Ok(out);
    }

    // Ranked by (dependency, file) so the same set is verified on a cached rerun,
    // then capped: findings past the cap are unconfirmed, never silently safe.
    checkable.sort_by(|a, b| {
        str_field(a, "dependency")
            .cmp(&str_field(b, "dependency"))
            .then(str_field(a, "file").cmp(&str_field(b, "file")))
    });
    let beyond = checkable.split_off(depth);
    out.unconfirmed.extend(beyond.into_iter().map(|finding| Unconfirmed {
        finding,
        why: "beyond --depth".into(),
    }));

    let batches: Vec<Vec<Value>> = checkable
        .chunks(VERIFY_BATCH)
        .map(|c| c.to_vec())
        .collect();
    let verdicts = gather(
        batches
            .iter()
            .map(|batch| {
                let prompt = refute_prompt(batch);
                let label = label(
                    "verify/",
                    &format!(
                        "{}:{}",
                        str_field(&batch[0], "dependency"),
                        batch.len()
                    ),
                );
                let asker = Arc::clone(&asker);
                let opts = ask_opts(
                    label,
                    "You are an adversarial reviewer. Default to breaks_here=false.".into(),
                    Arc::clone(&spend),
                );
                async move { asker(prompt, VERDICTS(), opts).await }
            })
            .collect(),
    )
    .await?;

    for (batch, verdict) in batches.iter().zip(&verdicts) {
        let Some(verdict) = verdict else {
            out.unconfirmed.extend(batch.iter().cloned().map(|finding| Unconfirmed {
                finding,
                why: "verifier did not answer".into(),
            }));
            continue;
        };
        // Bound by index, not by position: the verifier may answer out of order.
        // An index outside this batch is ignored, not fatal.
        let mut by_index: HashSet<i64> = HashSet::new();
        let mut verdicts = verdict
            .get("verdicts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|v| (int_field(&v, "index"), v))
            .filter(|(i, _)| by_index.insert(*i))
            .collect::<Vec<_>>();
        verdicts.retain(|(i, _)| *i >= 0 && (*i as usize) < batch.len());
        for (i, f) in batch.iter().enumerate() {
            match verdicts.iter().find(|(idx, _)| *idx == i as i64) {
                // An agent that skipped a claim has not refuted it.
                None => out.unconfirmed.push(Unconfirmed {
                    finding: f.clone(),
                    why: "verifier skipped this claim".into(),
                }),
                Some((_, v)) => {
                    let mut f = f.clone();
                    f["reason"] = Value::String(str_field(v, "reason"));
                    if v.get("breaks_here").and_then(Value::as_bool).unwrap_or(false) {
                        out.confirmed.push(f);
                    } else {
                        out.refuted.push(f);
                    }
                }
            }
        }
    }

    out.elapsed = started.elapsed().as_secs_f64();
    Ok(out)
}

/// Every candidate break this audit found, settled or not.
///
/// Derived rather than stored: a stored copy went unpopulated on the early
/// return paths, so `--no-verify` printed "0 candidate breaks" and then listed
/// three of them as unverified in the next breath.
fn candidate_count(r: &UpgradeResult) -> usize {
    r.confirmed.len() + r.refuted.len() + r.unconfirmed.len()
}

/// Exit 1 for anything partial. "Safe to bump" is printed only when nothing is
/// unchecked, unconfirmed, dead or skipped.
pub fn exit_code(r: &UpgradeResult) -> i32 {
    i32::from(r.unchecked > 0 || !r.unconfirmed.is_empty() || r.dead > 0 || r.skipped > 0)
}

pub fn render(r: &UpgradeResult) -> String {
    let mut out = format!(
        "\n{}: {} candidate breaks, {} confirmed\n",
        r.target.display(),
        candidate_count(r),
        r.confirmed.len()
    );
    if !r.confirmed.is_empty() {
        let mut deps: Vec<String> =
            r.confirmed.iter().map(|f| str_field(f, "dependency")).collect();
        deps.sort();
        deps.dedup();
        for dep in deps {
            out.push_str(&format!("\n{dep}:\n"));
            for f in r.confirmed.iter().filter(|f| str_field(f, "dependency") == dep) {
                out.push_str(&format!(
                    "  {}:{}  {}\n    {}\n",
                    str_field(f, "file"),
                    int_field(f, "line"),
                    str_field(f, "summary"),
                    str_field(f, "reason")
                ));
            }
        }
    }
    if !r.refuted.is_empty() {
        out.push_str(&format!(
            "\nRefuted ({}): upstream changes that do not hit this repo\n",
            r.refuted.len()
        ));
    }
    if r.unchecked > 0 {
        out.push_str(&format!(
            "\n{}/{} dependencies were NOT checked — an agent that consulted no changelog is \
             unchecked, not audited\n",
            r.unchecked,
            r.audited + r.unchecked
        ));
    }
    if !r.unconfirmed.is_empty() {
        out.push_str(&format!(
            "\n{} candidate breaks were NOT verified (dead or skipped verifier, beyond --depth, \
             --no-verify, or no changelog URL):\n",
            r.unconfirmed.len()
        ));
        for u in &r.unconfirmed {
            out.push_str(&format!(
                "  {}  {}:{}  {}  [{}]\n",
                str_field(&u.finding, "dependency"),
                str_field(&u.finding, "file"),
                int_field(&u.finding, "line"),
                str_field(&u.finding, "summary"),
                u.why
            ));
        }
    }
    if r.dead > 0 {
        out.push_str(&format!(
            "\n{}/{} dependency agents failed — those dependencies were NOT audited.\n",
            r.dead, r.dependencies
        ));
    }
    if r.skipped > 0 {
        out.push_str(&format!(
            "\n{} dependencies were skipped by the call budget — NOT audited.\n",
            r.skipped
        ));
    }
    if r.confirmed.is_empty() && r.unchecked == 0 && r.unconfirmed.is_empty() && r.dead == 0 && r.skipped == 0 {
        out.push_str("\nno confirmed breaks — the deps look safe to bump\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::schema_error;

    fn dep(name: &str, direct: bool, uses: i64) -> Value {
        json!({"name": name, "current": "1.0", "direct": direct, "uses": uses})
    }

    fn names(v: &[Value]) -> Vec<String> {
        v.iter().map(|d| str_field(d, "name")).collect()
    }

    #[test]
    fn the_manifest_lookup_prefers_the_first_known_file() {
        let dir = std::env::temp_dir().join("rustflows-manifest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_manifest(&dir), None, "an empty dir has no manifest");
        // Order is the contract: package.json outranks pyproject.toml.
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        assert_eq!(find_manifest(&dir), Some("pyproject.toml"));
        std::fs::write(dir.join("package.json"), "").unwrap();
        assert_eq!(find_manifest(&dir), Some("package.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_dependencies_are_audited_before_transitive_ones() {
        let deps = vec![dep("attrs", false, 0), dep("requests", true, 2), dep("django", true, 50)];
        assert_eq!(names(&pick(&deps, None)), vec!["django", "requests", "attrs"]);
        assert_eq!(names(&pick(&deps, Some(2))), vec!["django", "requests"]);
    }

    #[test]
    fn pick_is_deterministic_so_a_cached_rerun_audits_the_same_dependencies() {
        // The cache keys on the prompt. A pick that depended on input order would
        // audit a different set on run 2 and serve the wrong cached answer.
        let deps = vec![
            dep("zeta", true, 5),
            dep("alpha", true, 5),
            dep("mid", true, 5),
        ];
        let mut reversed = deps.clone();
        reversed.reverse();
        assert_eq!(names(&pick(&deps, None)), names(&pick(&reversed, None)));
        // And the tiebreak is by name, so the result is not input order.
        assert_eq!(names(&pick(&deps, None)), vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn two_dependencies_breaking_the_same_line_stay_separate() {
        let a = json!({"file": "app.py", "line": 41, "dependency": "django"});
        let b = json!({"file": "app.py", "line": 41, "dependency": "requests"});
        assert_ne!(key(&a), key(&b), "the key is a 3-tuple, not file:line");
        // The same dependency twice on one line is one finding.
        assert_eq!(key(&a), key(&a));
    }

    #[test]
    fn sources_checked_is_required_by_the_schema() {
        let ok = json!({"findings": [], "sources_checked": ["https://changelog"]});
        assert!(schema_error(&ok, &BREAKING()).unwrap().is_none());
        // Without it, an agent whose web tools failed is indistinguishable from
        // one that read a changelog and found nothing.
        let missing = json!({"findings": []});
        assert!(schema_error(&missing, &BREAKING()).unwrap().is_some());
        for s in [&INVENTORY(), &BREAKING(), &VERDICTS()] {
            assert!(crate::schema::validator(s).is_ok());
        }
    }

    #[test]
    fn every_dependency_agent_is_told_to_report_the_sources_it_read() {
        let p = dep_prompt("src", "django", "4.2");
        assert!(p.contains("sources_checked"), "{p}");
        assert!(p.contains("If your web tools fail"), "{p}");
        // And it is told an empty findings array is the common, correct answer.
        assert!(p.contains("return an empty findings array"), "{p}");
    }

    #[test]
    fn the_refute_prompt_does_not_treat_a_cited_changelog_as_evidence() {
        let f = json!({"file": "app.py", "line": 41, "dependency": "django",
                       "summary": "s", "failure": "f", "url": "http://c"});
        let p = refute_prompt(&[f]);
        assert!(p.contains("not evidence that this repo hits it"), "{p}");
        assert!(p.contains("[0] app.py:41"), "{p}");
        assert!(p.contains("Dependency: django"), "{p}");
    }

    #[test]
    fn an_explicit_limit_is_not_reported_as_a_budget_shortfall() {
        // A user-asked ceiling is not a shortfall; conflating the two makes every
        // deliberate --limit look like the budget cut the audit short.
        let limited = pick(&[dep("a", true, 1), dep("b", true, 1)], Some(1));
        assert_eq!(limited.len(), 1);
        // `None` is no limit, and `Some(0)` is refused by the CLI rather than
        // silently auditing nothing.
        assert_eq!(pick(&[dep("a", true, 1)], None).len(), 1);
    }

    #[test]
    fn the_budget_cut_audits_only_what_fits_and_reports_the_rest() {
        // Two calls per dependency: the audit, and verifying whatever it finds.
        let room = (MAX_CALLS / 2).max(1);
        assert_eq!(room, 20);
        assert!(room * 2 <= MAX_CALLS + 1);
    }

    #[test]
    fn render_claims_safety_only_when_nothing_is_partial() {
        let clean = UpgradeResult {
            target: PathBuf::from("src"),
            ..Default::default()
        };
        assert!(render(&clean).contains("safe to bump"), "{}", render(&clean));
        assert_eq!(exit_code(&clean), 0);

        for partial in [
            UpgradeResult { unchecked: 1, ..Default::default() },
            UpgradeResult { dead: 1, ..Default::default() },
            UpgradeResult { skipped: 1, ..Default::default() },
            UpgradeResult {
                unconfirmed: vec![Unconfirmed { finding: json!({}), why: "w".into() }],
                ..Default::default()
            },
        ] {
            let out = render(&partial);
            assert!(!out.contains("safe to bump"), "{out}");
            assert_eq!(exit_code(&partial), 1);
        }
    }

    #[test]
    fn an_unchecked_run_says_not_checked_not_safe() {
        let out = render(&UpgradeResult { unchecked: 2, audited: 1, ..Default::default() });
        assert!(out.contains("NOT checked"), "{out}");
        assert!(!out.contains("safe to bump"), "{out}");
    }

    #[test]
    fn a_no_verify_run_does_not_report_zero_candidate_breaks() {
        // `findings` used to be a stored field that the early return never set,
        // so --no-verify printed "0 candidate breaks, 0 confirmed" and then
        // listed three of them as unverified in the very next paragraph.
        let r = UpgradeResult {
            target: PathBuf::from("src"),
            unconfirmed: (0..3)
                .map(|i| Unconfirmed {
                    finding: json!({"dependency": "django", "file": "a.py", "line": i}),
                    why: "verification skipped".into(),
                })
                .collect(),
            ..Default::default()
        };
        let out = render(&r);
        assert!(out.contains("3 candidate breaks"), "{out}");
        assert!(!out.contains("0 candidate breaks"), "{out}");
        assert_eq!(candidate_count(&r), 3);
    }

    #[test]
    fn a_finding_citing_an_unfetchable_url_is_unconfirmed_not_verified() {
        // The agent holds WebFetch and reads this repo. A model-supplied URL
        // pointing at the metadata service must never reach it.
        for bad in ["", "   ", "not a url", "file:///etc/passwd", "http://169.254.169.254/"] {
            assert!(!crate::safe_url(bad), "{bad} must not be fetched");
        }
        assert!(crate::safe_url("https://github.com/django/django/releases"));
    }

    #[test]
    fn labels_stay_inside_the_stage_column() {
        // "dep/" plus a 40-char name is cut at the column, and the prefix survives.
        let long = label("dep/", &"n".repeat(40));
        assert_eq!(long.chars().count(), 32);
        assert!(long.starts_with("dep/"));
        // A short one is left alone.
        assert_eq!(label("dep/", "attrs"), "dep/attrs");
    }
}