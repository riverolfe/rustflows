//! Example workflow: research a question, then check the citations.
//!
//! Shape is sweep -> barrier -> synthesize -> verify. The barrier before
//! synthesis is real: the synthesizer needs every result at once to spot
//! agreement and contradiction. Verification is adversarial on *citations*, not
//! on conclusions.
//!
//! Ported from `pyflows/examples/research.py`. See README "Deliberate
//! differences from pyflows" for where this diverges.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use crate::ext::Extensions;
use crate::stats::Spend;
use crate::{AskFn, MAX_CALLS, OrchError, gather};

/// Web tools and nothing else. A searcher with a filesystem cannot research a
/// question; it can only read the repo it is standing in.
pub const WEB: &[&str] = &["WebSearch", "WebFetch"];

/// The four search modalities. Not keyword variants: a searcher handed a
/// reworded query finds what the first one found, while a searcher handed a
/// different modality finds what a keyword variant cannot.
pub const ANGLE_NAMES: &[&str] = &["primary", "practitioner", "contrarian", "recent"];

pub const ANGLES: &[&str] = &[
    "official docs, specs, papers and standards",
    "forum threads, postmortems and issue trackers",
    "critics, alternatives, or approaches that were abandoned",
    "changelogs, releases and recent announcements",
];

pub const FINDINGS: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "claim": {"type": "string"},
                    "url": {"type": "string"},
                    "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
                },
                "required": ["claim", "url", "confidence"],
            },
        }
    },
    "required": ["findings"],
});

/// A claim plus a URL, never a bare URL. A verifier handed only a URL can check
/// nothing: it has no assertion to compare the page against, so it can only
/// report that the page exists — or default to unsupported, having verified
/// nothing.
pub const BRIEF: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "answer": {"type": "string"},
        "agreements": {"type": "array", "items": {"type": "string"}},
        "contradictions": {"type": "array", "items": {"type": "string"}},
        "gaps": {"type": "array", "items": {"type": "string"}},
        "sources": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "claim": {"type": "string"},
                    "url": {"type": "string"},
                },
                "required": ["claim", "url"],
            },
        },
    },
    "required": ["answer", "agreements", "contradictions", "gaps", "sources"],
});

pub const VERDICT: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "supported": {"type": "boolean"},
        "reason": {"type": "string"},
    },
    "required": ["supported", "reason"],
});

/// One claim from one searcher.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub claim: String,
    pub url: String,
    pub confidence: String,
}

/// A citation whose verifier did not answer. Not the same as one whose verifier
/// said no: it checked nothing.
#[derive(Debug, Clone)]
pub struct Unchecked {
    pub claim: String,
    pub url: String,
}

/// A citation whose verifier said it does not hold up.
#[derive(Debug, Clone)]
pub struct Unsupported {
    pub claim: String,
    pub url: String,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct ResearchResult {
    pub answer: String,
    pub agreements: Vec<String>,
    pub contradictions: Vec<String>,
    pub gaps: Vec<String>,
    /// `None` when verification was skipped. "0 verified" and "nothing to
    /// verify" are different runs.
    pub verified: Option<usize>,
    pub unsupported: Vec<Unsupported>,
    pub unchecked: Vec<Unchecked>,
    pub angles_alive: usize,
    pub angles_dead: usize,
    /// The brief cited nothing checkable. Printed as its own failure, because
    /// "All 0 checked citations held up" would be a lie.
    pub nothing_checked: bool,
    /// Citations the brief made that `--depth` excluded. Checked by nobody, so
    /// reported rather than dropped — the same rule `upgrade` applies to its
    /// `beyond --depth` findings.
    pub over_depth: usize,
    pub elapsed: f64,
}

pub struct ResearchArgs {
    pub question: String,
    /// Defaults to every [`ANGLES`]. Empty is a configuration error, not a run
    /// with no searchers.
    pub angles: Option<usize>,
    pub depth: usize,
    pub verify: bool,
    pub spend: Option<Arc<Spend>>,
    pub ext: Extensions,
    pub asker: AskFn,
}

#[derive(Debug)]
pub enum ResearchError {
    Failed(String),
    Orch(OrchError),
}

impl std::fmt::Display for ResearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResearchError::Failed(m) | ResearchError::Orch(OrchError::Budget(m)) => write!(f, "{m}"),
            ResearchError::Orch(e) => write!(f, "{e}"),
        }
    }
}

impl From<OrchError> for ResearchError {
    fn from(e: OrchError) -> Self {
        ResearchError::Orch(e)
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Sweepers are blind to each other. A searcher that saw the others' claims
/// would echo them back instead of finding what they missed, so no barrier is
/// placed between them and no prompt carries another's findings.
fn sweep_prompt(question: &str, angle: &str) -> String {
    format!(
        "Research this question: {question}\n\nSearch specifically for {angle}. Report only \
         what a source actually states, with its URL. Do not speculate; if you find nothing, \
         return an empty findings array."
    )
}

/// Handed thirty claims, a synthesizer will otherwise average them into one
/// confident answer. It is told to report disagreement, not resolve it.
fn synthesize_prompt(question: &str, claims: &[Claim]) -> String {
    let gathered: Vec<String> = claims
        .iter()
        .map(|c| {
            format!(
                "- {} (source: {}, confidence: {})",
                c.claim, c.url, c.confidence
            )
        })
        .collect();
    format!(
        "Synthesize an answer to: {question}\n\nClaims gathered by independent searchers:\n{}\n\n\
         Name where they agree, where they contradict each other, and what nobody has covered. \
         Do not resolve contradictions by picking a side — report them.\n\nFor sources, list each \
         assertion from your answer together with the URL that backs it: {{claim, url}}. The \
         claim must be the specific assertion you are making, NOT the URL restated — a bare URL \
         cannot be checked.",
        gathered.join("\n")
    )
}

fn verify_prompt(claim: &str, url: &str) -> String {
    format!(
        "Does this source actually support the claim it is cited for?\n\nSource: {url}\n\
         Claim: {claim}\n\nFetch the source. Default to supported=false if it does not clearly \
         back the claim."
    )
}

/// The stage column is 32 wide.
fn verify_label(url: &str) -> String {
    let host = url.split_once("//").map(|(_, rest)| rest).unwrap_or(url);
    let tail: String = host.chars().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect();
    format!("verify/{tail}")
}

/// The citations worth checking, deduplicated by URL.
///
/// Skips a source that is a bare string (the pre-fix shape), an empty claim, an
/// empty URL, a URL that must not be fetched, and a claim that *is* its URL — a
/// self-referential claim carries no assertion, so a verifier could only confirm
/// the page exists.
pub fn citations(brief: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let Some(sources) = brief.get("sources").and_then(Value::as_array) else {
        return out;
    };
    for source in sources {
        if !source.is_object() {
            continue;
        }
        let claim = str_field(source, "claim").trim().to_string();
        let url = str_field(source, "url").trim().to_string();
        if claim.is_empty()
            || claim == url
            || !crate::safe_url(&url)
            || !seen.insert(url.clone())
        {
            continue;
        }
        out.push((claim, url));
    }
    out
}

/// How the verdicts fall out: how many held up, which did not, and which were
/// never judged.
///
/// A `None` verdict is `unchecked`, not `unsupported`. It verified nothing —
/// counting it as a refutation once made the run report a citation as
/// unsupported when no agent had looked at it.
pub fn classify(
    targets: &[(String, String)],
    verdicts: &[Option<Value>],
) -> (usize, Vec<Unsupported>, Vec<Unchecked>) {
    let mut verified = 0usize;
    let mut unsupported = Vec::new();
    let mut unchecked = Vec::new();
    for (i, (claim, url)) in targets.iter().enumerate() {
        let Some(Some(verdict)) = verdicts.get(i) else {
            unchecked.push(Unchecked {
                claim: claim.clone(),
                url: url.clone(),
            });
            continue;
        };
        if verdict.get("supported").and_then(Value::as_bool) == Some(true) {
            verified += 1;
        } else {
            unsupported.push(Unsupported {
                claim: claim.clone(),
                url: url.clone(),
                why: str_field(verdict, "reason"),
            });
        }
    }
    (verified, unsupported, unchecked)
}

pub async fn research(args: ResearchArgs) -> Result<ResearchResult, ResearchError> {
    let ResearchArgs {
        question,
        angles,
        depth,
        verify,
        spend,
        ext,
        asker,
    } = args;
    let started = Instant::now();
    let spend = spend.unwrap_or_else(|| Arc::new(Spend::new(MAX_CALLS)));

    let chosen = angles.unwrap_or(ANGLE_NAMES.len()).min(ANGLE_NAMES.len());
    // A configuration mistake, not dead agents: `gather` over no futures
    // succeeds, so without this it fell through to "all 0 search angles failed"
    // and pointed the reader at searchers that never ran. The CLI refuses
    // `--angles 0`, but `ResearchArgs` is public API.
    if chosen == 0 {
        return Err(ResearchError::Failed(
            "no search angles selected — name at least one".into(),
        ));
    }
    // No barrier between the sweepers — that blindness is the point.
    let found = gather(
        (0..chosen)
            .map(|i| {
                let angle = ANGLES[i];
                let prompt = sweep_prompt(&question, angle);
                // The label indexes with a modulo, so a caller passing more
                // angles than there are names cycles back rather than panicking.
                let label = format!("sweep/{}", ANGLE_NAMES[i % ANGLE_NAMES.len()]);
                let ext = ext.clone();
                let asker = Arc::clone(&asker);
                let spend = Arc::clone(&spend);
                async move {
                    asker(
                        prompt,
                        FINDINGS(),
                        crate::AskOpts {
                            tools: WEB.iter().map(|s| s.to_string()).collect(),
                            ext,
                            label,
                            system: Some(format!("You research via {angle}. Every claim needs a source URL.")),
                            spend: Some(spend),
                            ..Default::default()
                        },
                    )
                    .await
                }
            })
            .collect(),
    )
    .await?;

    let alive: Vec<&Value> = found.iter().flatten().collect();
    if alive.is_empty() {
        return Err(ResearchError::Failed(format!(
            "all {chosen} search angles failed"
        )));
    }

    // A claim with no URL is dropped here rather than counted and passed on: the
    // synthesizer would cite `(source: , …)`, and a claim it cannot source is one
    // the citation check can never catch.
    let mut claims: Vec<Claim> = Vec::new();
    let mut dropped = 0usize;
    for group in &alive {
        for f in group
            .get("findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let url = str_field(&f, "url");
            if url.trim().is_empty() {
                dropped += 1;
                continue;
            }
            claims.push(Claim {
                claim: str_field(&f, "claim"),
                url,
                confidence: match f.get("confidence").and_then(Value::as_str) {
                    Some(c) => c.to_string(),
                    None => "low".to_string(),
                },
            });
        }
    }
    if dropped > 0 {
        println!("  {dropped} claims had no URL and cannot be cited — dropped");
    }
    println!(
        "{} claims from {}/{} angles, synthesizing…",
        claims.len(),
        alive.len(),
        chosen
    );

    let synthesized = asker(
        synthesize_prompt(&question, &claims),
        BRIEF(),
        crate::AskOpts {
            tools: WEB.iter().map(|s| s.to_string()).collect(),
            ext: ext.clone(),
            label: "synthesize".into(),
            system: Some("You synthesize. Report disagreement rather than resolving it.".into()),
            spend: Some(Arc::clone(&spend)),
            ..Default::default()
        },
    )
    .await?;
    // A dead run must never read as "nothing found".
    let Some(brief) = synthesized else {
        return Err(ResearchError::Failed("synthesis failed".into()));
    };

    let mut result = ResearchResult {
        answer: str_field(&brief, "answer"),
        agreements: strings(&brief, "agreements"),
        contradictions: strings(&brief, "contradictions"),
        gaps: strings(&brief, "gaps"),
        angles_alive: alive.len(),
        angles_dead: chosen - alive.len(),
        ..Default::default()
    };
    if !verify {
        result.elapsed = started.elapsed().as_secs_f64();
        return Ok(result);
    }

    // `depth` is a hard cap applied after dedup, not a target count. What it
    // excludes is counted, not dropped: ten citations checked at depth 3 must
    // not report "all 3 held up" and exit 0 as if that were the whole brief.
    let all = citations(&brief);
    let over_depth = all.len().saturating_sub(depth);
    let targets: Vec<(String, String)> = all.into_iter().take(depth).collect();
    result.over_depth = over_depth;
    if targets.is_empty() {
        result.nothing_checked = true;
        result.elapsed = started.elapsed().as_secs_f64();
        return Ok(result);
    }
    println!("checking {} source citations…", targets.len());

    let verdicts = gather(
        targets
            .iter()
            .map(|(claim, url)| {
                let prompt = verify_prompt(claim, url);
                let label = verify_label(url);
                let ext = ext.clone();
                let asker = Arc::clone(&asker);
                let spend = Arc::clone(&spend);
                async move {
                    asker(
                        prompt,
                        VERDICT(),
                        crate::AskOpts {
                            tools: WEB.iter().map(|s| s.to_string()).collect(),
                            ext,
                            label,
                            system: Some("You check citations. Default to unsupported.".into()),
                            spend: Some(spend),
                            ..Default::default()
                        },
                    )
                    .await
                }
            })
            .collect(),
    )
    .await?;

    // Guarded on `targets` being non-empty, which it is above: an empty list
    // would vacuously satisfy this and report a clean run having checked nothing.
    if verdicts.iter().all(|v| v.is_none()) {
        return Err(ResearchError::Failed(format!(
            "all {} citation checks failed",
            targets.len()
        )));
    }

    let (verified, unsupported, unchecked) = classify(&targets, &verdicts);
    result.verified = Some(verified);
    result.unsupported = unsupported;
    result.unchecked = unchecked;
    result.elapsed = started.elapsed().as_secs_f64();
    Ok(result)
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Exit 1 for anything partial: an unchecked citation, a dead angle, or a brief
/// that cited nothing checkable. `verified == 0` on its own does not fail —
/// a brief whose every citation was refuted is a complete answer.
pub fn exit_code(r: &ResearchResult) -> i32 {
    i32::from(
        !r.unchecked.is_empty() || r.angles_dead > 0 || r.nothing_checked || r.over_depth > 0,
    )
}

pub fn render(r: &ResearchResult) -> String {
    let mut out = format!("\n{}\n", r.answer);
    for (heading, items) in [
        ("Agreements", &r.agreements),
        ("Contradictions", &r.contradictions),
        ("Gaps", &r.gaps),
    ] {
        if !items.is_empty() {
            out.push_str(&format!("\n{heading}:\n"));
            for i in items {
                out.push_str(&format!("  • {i}\n"));
            }
        }
    }
    if !r.unsupported.is_empty() {
        out.push_str(&format!("\nUnsupported citations ({}):\n", r.unsupported.len()));
        for u in &r.unsupported {
            out.push_str(&format!("  ✗ {}\n    {}\n    {}\n", u.url, u.claim, u.why));
        }
    }
    if !r.unchecked.is_empty() {
        out.push_str(&format!(
            "\nCitations NOT checked ({}) — their verifier did not answer:\n",
            r.unchecked.len()
        ));
        for u in &r.unchecked {
            out.push_str(&format!("  ? {}\n    {}\n", u.url, u.claim));
        }
    }
    if r.over_depth > 0 {
        out.push_str(&format!(
            "\n{}/{} citations were NOT checked — beyond --depth {}\n",
            r.over_depth,
            r.over_depth + r.verified.unwrap_or(0) + r.unsupported.len() + r.unchecked.len(),
            r.over_depth,
        ));
    }
    if r.angles_dead > 0 {
        out.push_str(&format!(
            "\n{} search angles failed — this brief is partial, not complete\n",
            r.angles_dead
        ));
    }
    if r.nothing_checked {
        out.push_str("\nNo citation could be checked — the brief cites no claim with a URL.\n");
    } else if let Some(v) = r.verified
        && r.unsupported.is_empty()
        && r.unchecked.is_empty()
        && r.over_depth == 0
    {
        out.push_str(&format!("\nAll {v} checked citations held up.\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::schema_error;

    struct Fake {
        dead: Vec<&'static str>,
        /// Kill only the first matching stage, so "one dead verifier" among
        /// several is expressible.
        dead_first: bool,
        killed: std::sync::Mutex<usize>,
        /// What the synthesizer's brief cites.
        sources: Vec<Value>,
        /// What the sweepers return.
        findings: Vec<Value>,
        /// What a citation check concludes.
        supported: bool,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    impl Fake {
        fn build(dead: Vec<&'static str>, dead_first: bool, sources: Vec<Value>) -> Arc<Fake> {
            Arc::new(Fake {
                dead,
                dead_first,
                killed: std::sync::Mutex::new(0),
                sources,
                findings: vec![json!({"claim": "c", "url": "http://a", "confidence": "high"})],
                supported: true,
                prompts: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn new() -> Arc<Fake> {
            Self::build(Vec::new(), false, vec![json!({"claim": "c", "url": "http://a"})])
        }

        fn killing(dead: Vec<&'static str>) -> Arc<Fake> {
            Self::build(dead, false, vec![json!({"claim": "c", "url": "http://a"})])
        }

        fn citing(sources: Vec<Value>) -> Arc<Fake> {
            Self::build(Vec::new(), false, sources)
        }

        /// Kills the first citation check, with two to check — so one dies and
        /// one lives, which is the case that must land in `unchecked`.
        fn first_check_dies() -> Arc<Fake> {
            Self::build(
                vec!["actually support"],
                true,
                vec![
                    json!({"claim": "one", "url": "http://1"}),
                    json!({"claim": "two", "url": "http://2"}),
                ],
            )
        }

        /// The sweepers return these findings instead of the default one.
        fn finding(mut self: Arc<Fake>, findings: Vec<Value>) -> Arc<Fake> {
            Arc::get_mut(&mut self).expect("no other handle yet").findings = findings;
            self
        }

        fn as_ask(self: &Arc<Self>) -> AskFn {
            let me = Arc::clone(self);
            Arc::new(move |prompt: String, schema: Value, _opts: crate::AskOpts| {
                let me = Arc::clone(&me);
                Box::pin(async move {
                    me.prompts.lock().unwrap().push(prompt.clone());
                    if me.dead.iter().any(|d| prompt.contains(d)) {
                        let mut killed = me.killed.lock().unwrap();
                        if !me.dead_first || *killed == 0 {
                            *killed += 1;
                            return Ok(None);
                        }
                    }
                    let out = if prompt.contains("Synthesize") {
                        json!({
                            "answer": "an answer",
                            "agreements": [],
                            "contradictions": [],
                            "gaps": [],
                            "sources": me.sources,
                        })
                    } else if prompt.contains("actually support") {
                        json!({"supported": me.supported, "reason": "because"})
                    } else if prompt.contains("Research this question") {
                        json!({"findings": me.findings})
                    } else {
                        panic!("unexpected stage: {}", &prompt[..prompt.len().min(40)])
                    };
                    assert!(
                        schema_error(&out, &schema).unwrap().is_none(),
                        "fake produced a payload its schema rejects"
                    );
                    Ok(Some(out))
                })
            })
        }

        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }

        fn count(&self, needle: &str) -> usize {
            self.prompts().iter().filter(|p| p.contains(needle)).count()
        }

        /// The prompt the given stage was called with, or a panic naming it.
        fn prompt_of(&self, needle: &str) -> String {
            self.prompts()
                .into_iter()
                .find(|p| p.contains(needle))
                .unwrap_or_else(|| panic!("{needle} never ran"))
        }
    }

    async fn run(fake: &Arc<Fake>) -> Result<ResearchResult, ResearchError> {
        research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 3,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: fake.as_ask(),
        })
        .await
    }

    #[tokio::test]
    async fn the_synthesizer_is_told_to_report_contradictions_not_resolve_them() {
        let p = synthesize_prompt("q", &[]);
        assert!(p.contains("Do not resolve contradictions by picking a side"), "{p}");
    }

    #[tokio::test]
    async fn sweep_agents_are_blind_to_each_other() {
        let f = Fake::new();
        run(&f).await.unwrap();
        let sweeps: Vec<String> = f
            .prompts()
            .into_iter()
            .filter(|p| p.contains("Research this question"))
            .collect();
        assert_eq!(sweeps.len(), ANGLE_NAMES.len());
        // Blind is the feature: a searcher that saw the others' claims would echo
        // them back instead of finding what they missed.
        for s in &sweeps {
            assert!(!s.contains("Claims gathered"), "{s}");
        }
    }

    #[tokio::test]
    async fn a_clean_run_verifies_its_citations_and_exits_zero() {
        let r = run(&Fake::new()).await.unwrap();
        assert_eq!(r.verified, Some(1));
        assert!(r.unsupported.is_empty());
        assert!(r.unchecked.is_empty());
        assert_eq!(r.angles_dead, 0);
        assert_eq!(exit_code(&r), 0);
    }

    #[tokio::test]
    async fn all_angles_dead_is_not_a_clean_run() {
        let f = Fake::killing(vec!["Research this question"]);
        assert!(matches!(
            run(&f).await,
            Err(ResearchError::Failed(m)) if m.contains("search angles failed")
        ));
    }

    #[tokio::test]
    async fn a_dead_synthesizer_is_a_failed_run() {
        let f = Fake::killing(vec!["Synthesize"]);
        assert!(matches!(
            run(&f).await,
            Err(ResearchError::Failed(m)) if m == "synthesis failed"
        ));
    }

    #[tokio::test]
    async fn a_dead_citation_check_is_unchecked_not_a_crash_and_not_unsupported() {
        // Every check dead is a failed run, not a brief with zero citations.
        let f = Fake::killing(vec!["actually support"]);
        assert!(matches!(
            run(&f).await,
            Err(ResearchError::Failed(m)) if m.contains("citation checks failed")
        ));

        // Mixed: one dead, one live. The dead one is `unchecked` — it verified
        // nothing — and the run must not claim everything held up.
        let f = Fake::first_check_dies();
        let r = run(&f).await.unwrap();
        assert_eq!(r.unchecked.len(), 1);
        assert!(r.unsupported.is_empty(), "a dead verifier refutes nothing");
        assert_eq!(r.verified, Some(1));
        assert_eq!(exit_code(&r), 1);
        let out = render(&r);
        assert!(out.contains("NOT checked"), "{out}");
        assert!(!out.contains("held up"), "{out}");
    }

    #[tokio::test]
    async fn claims_without_a_url_never_reach_the_synthesizer() {
        // The count and the filter must agree: these used to be counted as
        // dropped AND passed on to the synthesizer, which would then cite
        // `(source: , …)` — a claim nothing downstream can ever check.
        let f = Fake::new().finding(vec![
            json!({"claim": "sourced", "url": "http://a", "confidence": "high"}),
            json!({"claim": "unsourced", "url": "", "confidence": "low"}),
        ]);
        research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 1,
            verify: false,
            spend: None,
            ext: Extensions::none(),
            asker: f.as_ask(),
        })
        .await
        .unwrap();
        let synth = f.prompt_of("Synthesize");
        assert!(synth.contains("sourced (source: http://a"), "{synth}");
        assert!(!synth.contains("unsourced"), "{synth}");
    }

    #[test]
    fn a_citation_carries_a_real_claim_not_a_bare_url() {
        // All four skip rules at once.
        let brief = json!({"sources": [
            {"claim": "uv ships 3.14t builds", "url": "http://a"},
            {"claim": "http://b", "url": "http://b"},
            {"claim": "", "url": "http://c"},
            {"claim": "s", "url": ""},
            {"claim": "another claim", "url": "http://a"},
            "a bare string from the pre-fix shape",
        ]});
        assert_eq!(
            citations(&brief),
            vec![("uv ships 3.14t builds".to_string(), "http://a".to_string())]
        );
        assert!(citations(&json!({"sources": ["just a string"]})).is_empty());
        assert!(citations(&json!({})).is_empty());
    }

    #[test]
    fn the_verify_prompt_states_the_actual_assertion() {
        // Sources used to be bare URLs, so the verifier was handed (url, url) and
        // could only report "this page exists".
        let p = verify_prompt("uv ships 3.14t builds", "http://a");
        assert!(p.contains("Claim: uv ships 3.14t builds"), "{p}");
        assert!(p.contains("Source: http://a"), "{p}");
    }

    #[tokio::test]
    async fn citation_check_labels_keep_the_end_of_the_source() {
        let a = verify_label("file:///home/user/repo/fwlibs/anc-lib/doc/parser.md");
        let b = verify_label("file:///home/user/repo/fwlibs/anc-lib/doc/vpid.md");
        assert_ne!(a, b);
        assert!(a.ends_with("parser.md"), "{a}");
        assert!(b.ends_with("vpid.md"), "{b}");
        assert!(a.chars().count() <= 32, "{}", a.chars().count());
    }

    #[test]
    fn a_dead_verifier_is_unchecked_and_a_refutation_is_unsupported() {
        let targets = vec![
            ("held".to_string(), "http://a".to_string()),
            ("fell".to_string(), "http://b".to_string()),
            ("nobody looked".to_string(), "http://c".to_string()),
        ];
        let verdicts = vec![
            Some(json!({"supported": true, "reason": "yes"})),
            Some(json!({"supported": false, "reason": "no"})),
            None,
        ];
        let (verified, unsupported, unchecked) = classify(&targets, &verdicts);
        assert_eq!(verified, 1);
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0].why, "no");
        assert_eq!(unchecked.len(), 1);
        assert_eq!(unchecked[0].url, "http://c");

        // Everything unverified is not everything refuted.
        let (_, unsupported, unchecked) =
            classify(&targets, &[None, None, None]);
        assert!(unsupported.is_empty());
        assert_eq!(unchecked.len(), 3);
    }

    #[tokio::test]
    async fn a_dead_search_angle_is_a_partial_run_not_a_dead_one() {
        let me = Fake::killing(vec![ANGLES[0]]);
        let r = research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 1,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: me.as_ask(),
        })
        .await
        .unwrap();
        assert_eq!(r.angles_dead, 1);
        assert_eq!(r.angles_alive, ANGLE_NAMES.len() - 1);
        assert!(!r.answer.is_empty(), "the run still produced a brief");
        assert_eq!(exit_code(&r), 1);
        assert!(render(&r).contains("1 search angles failed"), "{}", render(&r));
    }

    #[tokio::test]
    async fn a_brief_with_no_checkable_citation_is_not_a_clean_run() {
        // The only source is its own URL: checkable in name only.
        let me = Fake::citing(vec![json!({"claim": "http://a", "url": "http://a"})]);
        let r = research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 3,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: me.as_ask(),
        })
        .await
        .unwrap();
        assert!(r.nothing_checked);
        assert_eq!(exit_code(&r), 1);
        let out = render(&r);
        assert!(out.contains("No citation could be checked"), "{out}");
        assert!(!out.contains("held up"), "{out}");
    }

    #[tokio::test]
    async fn only_depth_citations_are_checked() {
        let me = Fake::citing(vec![
            json!({"claim": "one", "url": "http://1"}),
            json!({"claim": "two", "url": "http://2"}),
            json!({"claim": "three", "url": "http://3"}),
        ]);
        let r = research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 1,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: me.as_ask(),
        })
        .await
        .unwrap();
        // A hard cap applied after dedup, not a target count.
        assert_eq!(me.count("actually support"), 1, "depth is a hard cap");
        assert_eq!(r.verified, Some(1));
    }

    #[tokio::test]
    async fn zero_angles_is_a_configuration_error_not_dead_searchers() {
        // `gather` over no futures succeeds, so `angles: Some(0)` fell through
        // to "all 0 search angles failed" — blaming searchers that never ran.
        // The CLI refuses it, but `ResearchArgs` is public API.
        let f = Fake::new();
        let err = research(ResearchArgs {
            question: "q".into(),
            angles: Some(0),
            depth: 3,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: f.as_ask(),
        })
        .await
        .unwrap_err();
        let ResearchError::Failed(m) = &err else {
            panic!("expected a configuration error, got {err:?}");
        };
        assert!(m.contains("no search angles selected"), "{m}");
        assert_eq!(f.count("Research this question"), 0, "no agent should have run");
    }

    #[tokio::test]
    async fn citations_beyond_depth_are_reported_not_silently_dropped() {
        // Ten citations at --depth 3 used to print "All 3 checked citations
        // held up." and exit 0, as if that were the whole brief.
        let me = Fake::citing(
            (1..=10)
                .map(|i| json!({"claim": format!("c{i}"), "url": format!("http://{i}")}))
                .collect(),
        );
        let r = research(ResearchArgs {
            question: "q".into(),
            angles: None,
            depth: 3,
            verify: true,
            spend: None,
            ext: Extensions::none(),
            asker: me.as_ask(),
        })
        .await
        .unwrap();
        assert_eq!(r.over_depth, 7);
        assert_eq!(me.count("actually support"), 3);
        assert_eq!(exit_code(&r), 1, "a partial verification set is not a clean run");
        let out = render(&r);
        assert!(out.contains("7/10 citations were NOT checked"), "{out}");
        assert!(!out.contains("held up"), "{out}");
    }

    #[test]
    fn a_citation_an_agent_would_fetch_must_not_point_at_this_host() {
        // Model text from web pages decides this URL, and the verifier holds
        // WebFetch. Without a check, a returned metadata-service URL is fetched.
        let brief = json!({"sources": [
            {"claim": "real", "url": "https://example.com/a"},
            {"claim": "metadata", "url": "http://169.254.169.254/latest/meta-data/"},
            {"claim": "local", "url": "file:///etc/passwd"},
            {"claim": "loopback", "url": "http://127.0.0.1:9000/"},
            {"claim": "no scheme", "url": "example.com/a"},
        ]});
        assert_eq!(
            citations(&brief),
            vec![("real".to_string(), "https://example.com/a".to_string())]
        );
    }

    #[test]
    fn every_stage_compiles_its_schema() {
        for s in [&FINDINGS(), &BRIEF(), &VERDICT()] {
            assert!(crate::schema::validator(s).is_ok());
        }
        // A bare URL is not a citation, so the schema cannot require one to differ
        // — the workflow skips those rather than trusting the agent.
        assert!(schema_error(&json!({}), &BRIEF()).unwrap().is_some());
    }
}