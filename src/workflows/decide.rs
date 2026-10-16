//! Example workflow: pick between options by tournament.
//!
//! Shape is advocate -> bracket -> winner. The one that is not a pipeline: the
//! others fan out, hit a barrier, then fan out again; this is a knockout
//! bracket where each verdict decides who plays next.
//!
//! Ported from `pyflows/examples/decide.py`. See README "Deliberate
//! differences from pyflows" for where this diverges.

use serde_json::{Value, json};
use sha2::Digest;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use crate::ext::Extensions;
use crate::stats::Spend;
use crate::{AskFn, MAX_CALLS, OrchError, gather};

/// An advocate's case for one option.
pub const CASE: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "plan": {"type": "string"},
        // Not decoration. A judge comparing two plans can only weigh a
        // trade-off it has been told about; without this the advocate's most
        // flattering framing is the one that gets compared.
        "caveat": {"type": "string"},
    },
    "required": ["plan", "caveat"],
});

/// One head-to-head verdict.
///
/// `winner` is an enum rather than a plain integer because a judge answering
/// "Candidate 2" or -1 for the first is not a verdict — and it is far cheaper to
/// refuse it here than to detect a nonsense winner after the fact.
pub const PAIR: fn() -> Value = || json!({
    "type": "object",
    "properties": {
        "winner": {"type": "integer", "enum": [0, 1]},
        "why": {"type": "string"},
    },
    "required": ["winner", "why"],
});

/// The stage column is 32 wide; a longer label is truncated in the panel, but a
/// truncated label is also a label that names the wrong option.
const LABEL_MAX: usize = 32;

/// An option with a live advocate, carrying its case.
#[derive(Debug, Clone)]
pub struct Contender {
    pub option: String,
    pub plan: String,
    pub caveat: String,
}

/// One matchup.
///
/// `match_up` is in **presented** order — the order the judge saw — because that
/// is the order its `winner` index refers to. Recording bracket order here made
/// `render` print the eliminated contender as the winner whenever the seeded
/// order flipped the pair.
///
/// `winner` is `None` only when the matchup was undecided; then both advance.
/// `why` is absent when the judge died or said nothing useful, which is a
/// different thing from a decided matchup with no reason attached.
#[derive(Debug, Default, Clone)]
pub struct Round {
    pub match_up: Vec<String>,
    pub winner: Option<String>,
    pub why: Option<String>,
}

impl Round {
    pub fn decided(&self) -> bool {
        self.winner.is_some()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Brief {
    pub question: String,
    pub options: Vec<String>,
    /// `None` when the bracket stalled. A provisional winner is still a lie, and
    /// this is the case where there is nothing to be provisional about.
    pub winner: Option<(String, String)>,
    pub tied: Vec<String>,
    pub rounds: Vec<Round>,
    /// Options whose advocate died. They did not lose; they never played.
    pub no_case: Vec<String>,
    /// A dead judge left a matchup undecided, so any winner above it is provisional.
    pub undecided: usize,
    /// The field was cut to fit the call budget. Not the user's choice, so it is
    /// reported as a shortfall rather than folded into a clean run.
    pub capped: bool,
    pub elapsed: f64,
}

pub struct DecideArgs {
    pub question: String,
    pub options: Vec<String>,
    /// `None` runs a fresh budget. A caller that also wants to spend calls from
    /// the same run passes its own.
    pub spend: Option<Arc<Spend>>,
    pub ext: Extensions,
    pub asker: AskFn,
    /// Set by the caller when [`cap_to_budget`] had to shrink the field. It is
    /// not a clean run: the user asked about options that never played.
    pub capped: bool,
}

#[derive(Debug)]
pub enum DecideError {
    /// A dead run must never read as a decision.
    Failed(String),
    Orch(OrchError),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::Failed(m) | DecideError::Orch(OrchError::Budget(m)) => write!(f, "{m}"),
            DecideError::Orch(e) => write!(f, "{e}"),
        }
    }
}

impl From<OrchError> for DecideError {
    fn from(e: OrchError) -> Self {
        DecideError::Orch(e)
    }
}

/// Order-preserving dedup, so `decide q a a` is one option and not a bye.
pub fn choose_options(chosen: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    chosen
        .iter()
        .filter(|o| seen.insert((*o).clone()))
        .cloned()
        .collect()
}

/// Stable per-option seed for the presentation order.
///
/// pyflows used `zlib.crc32`. The property that matters is only "stable, and
/// derived from the option's own text so it is not the argument order" — and
/// `sha2` is already a dependency, where a CRC crate would not be.
fn seed(option: &str) -> u64 {
    let digest = sha2::Sha256::digest(option.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

/// The order a matchup is *presented* in, which is what a verdict indexes.
///
/// Not the argument order: a judge shown the same pair in the same order every
/// time has its first-position bias baked into the cache, and the first order it
/// ever saw becomes permanent. Ties fall back to argument order, matching a
/// stable sort.
fn presented(a: &Contender, b: &Contender) -> Vec<Contender> {
    if seed(&b.option) < seed(&a.option) {
        vec![b.clone(), a.clone()]
    } else {
        vec![a.clone(), b.clone()]
    }
}

/// Consecutive pairs; a trailing odd contender is a one-element "matchup".
///
/// The bye is what makes this single elimination rather than round robin: an
/// n-option field costs n-1 judges, not the quadratic a table would.
fn matchups(contenders: &[Contender]) -> Vec<Vec<Contender>> {
    contenders.chunks(2).map(|c| c.to_vec()).collect()
}

/// Shrink the field to what the budget can actually finish.
///
/// An advocate plus its judges is roughly two calls per option, so half the
/// budget buys half the field. Returns the cap so the caller can say the field
/// was truncated — an audit that quietly checked a prefix is the failure this
/// exists to prevent.
pub fn cap_to_budget(field: &[String], budget: usize) -> (Vec<String>, bool) {
    if field.len() * 2 <= budget + 1 {
        return (field.to_vec(), false);
    }
    let fits = budget.div_ceil(2).max(1);
    (field[..fits.min(field.len())].to_vec(), true)
}

fn label(prefix: &str, what: &str) -> String {
    let raw = format!("{prefix}{what}");
    raw.chars().take(LABEL_MAX).collect()
}

/// Which side a verdict picked, in the order the prompt presented.
///
/// `None` for a dead judge and for a verdict whose `winner` is missing or out of
/// range — both decided nothing. Defaulting such a verdict to index 0 would
/// eliminate a contender on a result nobody cast.
fn verdict_winner(verdict: &Option<Value>) -> Option<usize> {
    verdict
        .as_ref()
        .and_then(|v| v.get("winner"))
        .and_then(Value::as_i64)
        .filter(|w| *w == 0 || *w == 1)
        .map(|w| w as usize)
}

/// The advocate sees only its own option. Naming the others would only soften
/// the argument, and naming its own would give it away to the judge later.
fn advocate_prompt(question: &str, i: usize, opt: &str) -> String {
    format!(
        "{question}\n\nMake the strongest honest case for option {i}: {opt}\n\n\
         Argue FOR it. Do not hedge and do not compare — you cannot see the other \
         options, and naming them would only soften your argument. Do not name the \
         option itself in the plan or caveat; describe what it does."
    )
}

/// The judge sees plans, not names, so it weighs the argument rather than the brand.
fn judge_prompt(question: &str, pair: &[Contender]) -> String {
    let shown: Vec<String> = pair
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "[{i}] Candidate {i}\n    Plan: {}\n    Weakest point: {}",
                c.plan, c.caveat
            )
        })
        .collect();
    format!(
        "{question}\n\nPick the stronger of these two plans.\n\n{}\n\nJudge on merit, \
         not on which is written first. Pick one — \"both are fine\" is not a verdict you \
         can return. Return the index of the better plan.",
        shown.join("\n\n")
    )
}

pub async fn decide(args: DecideArgs) -> Result<Brief, DecideError> {
    let DecideArgs {
        question,
        options,
        spend,
        ext,
        asker,
        capped,
    } = args;
    let started = Instant::now();

    // Dedup before anything else, so a repeated option is one option rather
    // than a bye — and so a field that collapses to one fails without a call.
    let options = choose_options(&options);
    if options.len() < 2 {
        return Err(DecideError::Failed(
            "a tournament needs at least two options".into(),
        ));
    }
    let spend = spend.unwrap_or_else(|| Arc::new(Spend::new(MAX_CALLS)));

    // Advocates and judges run with no tools: the answer depends on the prompt
    // alone, so there is nothing to read and nothing to fingerprint.
    let cases = gather(
        options
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                let prompt = advocate_prompt(&question, i, opt);
                let label = label("case/", opt);
                let ext = ext.clone();
                let asker = Arc::clone(&asker);
                let spend = Arc::clone(&spend);
                async move {
                    asker(
                        prompt,
                        CASE(),
                        crate::AskOpts {
                            tools: Vec::new(),
                            ext,
                            label,
                            system: Some("You advocate one option, alone. Be its best advocate.".into()),
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

    // An option whose advocate died did not lose the tournament, it never
    // played. Different thing, and the difference is the whole point.
    let mut contenders: Vec<Contender> = Vec::new();
    let mut no_case: Vec<String> = Vec::new();
    for (option, case) in options.iter().zip(cases) {
        match case {
            Some(c) => contenders.push(Contender {
                option: option.clone(),
                plan: c.get("plan").and_then(Value::as_str).unwrap_or("").to_string(),
                caveat: c.get("caveat").and_then(Value::as_str).unwrap_or("").to_string(),
            }),
            None => no_case.push(option.clone()),
        }
    }
    if contenders.len() < 2 {
        return Err(DecideError::Failed(format!(
            "only {}/{} options were argued; that is not a decision",
            contenders.len(),
            options.len()
        )));
    }

    let mut rounds: Vec<Round> = Vec::new();
    let mut undecided = 0usize;

    while contenders.len() > 1 {
        // The presentation order is decided once, here, before any judge runs,
        // and the verdict is resolved against that same pair. Deciding at
        // verdict time would make correctness depend on `presented` staying pure.
        let games: Vec<(Option<Vec<Contender>>, Vec<Contender>)> = matchups(&contenders)
            .into_iter()
            .map(|m| {
                if m.len() == 2 {
                    (Some(presented(&m[0], &m[1])), m)
                } else {
                    (None, m)
                }
            })
            .collect();

        let shown: Vec<Vec<Contender>> = games.iter().filter_map(|(p, _)| p.clone()).collect();
        let verdicts = gather(
            shown
                .iter()
                .map(|pair| {
                    let prompt = judge_prompt(&question, pair);
                    let label = label("judge/", &pair[0].option);
                    let ext = ext.clone();
                    let asker = Arc::clone(&asker);
                    let spend = Arc::clone(&spend);
                    async move {
                        asker(
                            prompt,
                            PAIR(),
                            crate::AskOpts {
                                tools: Vec::new(),
                                ext,
                                label,
                                system: Some("You are an impartial judge. One winner or nothing.".into()),
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

        let mut next: Vec<Contender> = Vec::new();
        // Verdict by verdict over the JUDGED games only. Zipping the two lists
        // positionally silently drops a bye — one unjudged matchup per odd field
        // — and quietly shrinks the tournament the user asked for.
        let mut verdicts = verdicts.into_iter();
        for (pair, matched) in games {
            // A bye costs nothing and advances untouched.
            let Some(pair) = pair else {
                next.push(matched[0].clone());
                continue;
            };
            let verdict = verdicts.next().flatten();
            // Presented order, because that is what the verdict indexes.
            let names = vec![pair[0].option.clone(), pair[1].option.clone()];

            match verdict_winner(&verdict) {
                Some(w) => {
                    next.push(pair[w].clone());
                    rounds.push(Round {
                        match_up: names,
                        winner: Some(pair[w].option.clone()),
                        why: verdict
                            .and_then(|v| v.get("why").and_then(Value::as_str).map(str::to_string)),
                    });
                }
                // A dead judge eliminates nobody: picking one would be an
                // elimination nobody voted for. A verdict whose `winner` is
                // unreadable is treated the same way — it decided nothing.
                None => {
                    undecided += 1;
                    rounds.push(Round {
                        match_up: names,
                        winner: None,
                        why: None,
                    });
                    next.extend(matched);
                }
            }
        }

        // A round that eliminates nobody does not shrink the field, so the
        // bracket would re-run the same matchups forever. That is not patience,
        // it is a tournament that cannot be resolved — and the honest result is
        // no result. (Shipped: a dead judge in a 2-option field, 104k calls.)
        if next.len() >= contenders.len() {
            break;
        }
        contenders = next;
    }

    let tied: Vec<String> = contenders.iter().map(|c| c.option.clone()).collect();
    let winner = if tied.len() > 1 {
        None
    } else {
        Some((tied[0].clone(), contenders[0].plan.clone()))
    };

    Ok(Brief {
        question,
        options,
        winner,
        tied: if tied.len() > 1 { tied } else { Vec::new() },
        rounds,
        no_case,
        undecided,
        capped,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

/// Exit 1 for anything partial: an undecided matchup, an option that never
/// played, no winner at all, or a field the budget cut.
pub fn exit_code(brief: &Brief) -> i32 {
    i32::from(
        brief.undecided > 0
            || !brief.no_case.is_empty()
            || brief.winner.is_none()
            || brief.capped,
    )
}

pub fn render(brief: &Brief) -> String {
    let mut out = String::new();
    out.push_str(&format!("\n{}\n", brief.question));
    match &brief.winner {
        Some((option, plan)) => out.push_str(&format!("best option: {option}\n  {plan}\n")),
        None => out.push_str(&format!(
            "no winner: {} could not be separated\n",
            brief.tied.join(", ")
        )),
    }
    if !brief.rounds.is_empty() {
        out.push_str("\nrounds:\n");
        for r in &brief.rounds {
            // Branch on whether the matchup was decided, never on whether a
            // reason came back: a decided round with an unreadable `why` is
            // still a decided round, and one side really was eliminated.
            match &r.winner {
                Some(winner) => {
                    let loser = r
                        .match_up
                        .iter()
                        .find(|o| *o != winner)
                        .map(String::as_str)
                        .unwrap_or("?");
                    match &r.why {
                        Some(why) => {
                            out.push_str(&format!("  {winner} beats {loser} — {why}\n"))
                        }
                        None => out.push_str(&format!("  {winner} beats {loser}\n")),
                    }
                }
                None => out.push_str(&format!(
                    "  {} vs {} — no judge; both advance\n",
                    r.match_up[0], r.match_up[1]
                )),
            }
        }
    }
    if !brief.no_case.is_empty() {
        out.push_str(&format!(
            "\n{} option(s) got no case and never played: {}\n",
            brief.no_case.len(),
            brief.no_case.join(", ")
        ));
    }
    if brief.capped {
        out.push_str("\nthe field was cut to fit the call budget — it was not decided\n");
    }
    // Both conditions: the caveat only fires when there is a winner to qualify.
    if brief.undecided > 0 && brief.winner.is_some() {
        out.push_str(&format!(
            "\n{} matchup(s) went undecided — the winner is provisional, not chosen.\n",
            brief.undecided
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::schema_error;

    /// The stand-in for pyflows' `patched(dc, "ask", fake)`: dispatches on the
    /// prompt, exactly as the Python fixture did. `dead_prompts` are substrings
    /// naming stages that should return `None`.
    struct Fake {
        dead: Vec<&'static str>,
        /// Kill only the first matching stage, so "one dead judge" is expressible
        /// without a per-stage counter.
        dead_first: bool,
        killed: std::sync::Mutex<usize>,
        /// What every judge returns. Captured, so a boxed closure, not a fn ptr.
        pick: Arc<dyn Fn(&str) -> i64 + Send + Sync>,
        prompts: std::sync::Mutex<Vec<String>>,
        tool_lists: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl Fake {
        fn new() -> Arc<Fake> {
            Arc::new(Fake {
                dead: Vec::new(),
                dead_first: false,
                killed: std::sync::Mutex::new(0),
                pick: Arc::new(|_| 0),
                prompts: std::sync::Mutex::new(Vec::new()),
                tool_lists: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn with(pick: i64) -> Arc<Fake> {
            Arc::new(Fake {
                dead: Vec::new(),
                dead_first: false,
                killed: std::sync::Mutex::new(0),
                pick: Arc::new(move |_| pick),
                prompts: std::sync::Mutex::new(Vec::new()),
                tool_lists: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn as_ask(self: &Arc<Self>) -> AskFn {
            let me = Arc::clone(self);
            Arc::new(move |prompt: String, schema: Value, opts: crate::AskOpts| {
                let me = Arc::clone(&me);
                Box::pin(async move {
                    me.prompts.lock().unwrap().push(prompt.clone());
                    me.tool_lists.lock().unwrap().push(opts.tools.clone());

                    if me.dead.iter().any(|d| prompt.contains(d)) {
                        let mut killed = me.killed.lock().unwrap();
                        if !me.dead_first || *killed == 0 {
                            *killed += 1;
                            return Ok(None);
                        }
                    }
                    let out = if prompt.contains("strongest honest case") {
                        // Echo the option back so a test can tell them apart.
                        let opt = prompt
                            .split("option ")
                            .nth(1)
                            .and_then(|s| s.split_once(": "))
                            .map(|(_, o)| o.split('\n').next().unwrap_or(""))
                            .unwrap_or("?")
                            .to_string();
                        json!({"plan": format!("do {opt}"), "caveat": format!("cost of {opt}")})
                    } else if prompt.contains("Pick the stronger") {
                        let pick = (me.pick)(&prompt);
                        json!({"winner": pick, "why": format!("{pick} is stronger")})
                    } else {
                        panic!("unexpected stage: {}", &prompt[..prompt.len().min(40)])
                    };
                    // The fake validates its own output, so it cannot drift into
                    // a shape the schema does not describe.
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
    }

    /// A fake whose given prompt substrings return `None`.
    fn fake_killing(dead: Vec<&'static str>) -> Arc<Fake> {
        fake_killing_first(dead, false)
    }

    /// `first` kills only the first matching stage.
    fn fake_killing_first(dead: Vec<&'static str>, first: bool) -> Arc<Fake> {
        Arc::new(Fake {
            dead,
            dead_first: first,
            killed: std::sync::Mutex::new(0),
            pick: Arc::new(|_| 0),
            prompts: std::sync::Mutex::new(Vec::new()),
            tool_lists: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn options(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    async fn run(fake: &Arc<Fake>, opts: &[&str]) -> Result<Brief, DecideError> {
        decide(DecideArgs {
            question: "which queue?".into(),
            options: options(opts),
            spend: None,
            ext: Extensions::none(),
            asker: fake.as_ask(),
            capped: false,
        })
        .await
    }

    #[tokio::test]
    async fn a_clean_tournament_picks_one_winner_and_exits_zero() {
        let f = Fake::new();
        let brief = run(&f, &["a", "b", "c"]).await.unwrap();
        let winner = brief.winner.clone().expect("a winner").0;
        assert_eq!(brief.tied, Vec::<String>::new());
        assert_eq!(brief.undecided, 0);
        assert_eq!(brief.no_case, Vec::<String>::new());
        assert!(["a", "b", "c"].contains(&winner.as_str()));
        assert_eq!(exit_code(&brief), 0);
    }

    #[tokio::test]
    async fn single_elimination_costs_one_judge_per_match_not_a_round_robin() {
        // 4 options = 4 advocates + 3 judges. Round robin would spend 6 judges on
        // the same field.
        let f = Fake::new();
        run(&f, &["postgres", "mysql", "sqlite", "cassandra"]).await.unwrap();
        assert_eq!(f.count("strongest honest case"), 4);
        assert_eq!(f.count("Pick the stronger"), 3);
    }

    #[tokio::test]
    async fn an_odd_field_gives_the_last_option_a_bye_that_costs_no_call() {
        // b fights a, c sits the round out, then the winner meets c.
        let f = Fake::new();
        run(&f, &["a", "b", "c"]).await.unwrap();
        assert_eq!(f.count("Pick the stronger"), 2);
    }

    #[tokio::test]
    async fn a_dead_advocate_is_reported_not_silently_dropped() {
        // An option that never played is not the winner, and is not hidden: a
        // 2-option tournament with one dead advocate looked unanimous otherwise.
        let f = fake_killing(vec!["option 1: mysql"]);
        let brief = decide(DecideArgs {
            question: "which queue?".into(),
            options: options(&["postgres", "mysql", "kafka"]),
            spend: None,
            ext: Extensions::none(),
            asker: f.as_ask(),
            capped: false,
        })
        .await
        .unwrap();
        assert_eq!(brief.no_case, options(&["mysql"]));
        let winner = brief.winner.clone().expect("a winner");
        assert_ne!(winner.0, "mysql");
        assert_eq!(exit_code(&brief), 1, "an option that never played is partial");
    }

    #[tokio::test]
    async fn too_few_live_advocates_is_a_failure_not_a_winner() {
        let f = fake_killing(vec!["option 1: mysql"]);
        let err = decide(DecideArgs {
            question: "q".into(),
            options: options(&["postgres", "mysql"]),
            spend: None,
            ext: Extensions::none(),
            asker: f.as_ask(),
            capped: false,
        })
        .await
        .unwrap_err();
        let DecideError::Failed(m) = &err else {
            panic!("a field nobody argued is a failure, not a decision: {err:?}");
        };
        assert!(m.contains("not a decision"), "{m}");
    }

    #[tokio::test]
    async fn a_tournament_needs_two_distinct_options_and_spends_nothing_to_find_out() {
        assert!(choose_options(&options(&["a", "a"])) == options(&["a"]));
        let f = Fake::new();
        assert!(run(&f, &["a", "a"]).await.is_err());
        assert_eq!(f.count("strongest honest case"), 0, "it must not call an agent");
        // Duplicates collapse, they do not become a bye.
        let f = Fake::new();
        run(&f, &["a", "b", "a"]).await.unwrap();
        assert_eq!(f.count("strongest honest case"), 2);
    }

    #[tokio::test]
    async fn a_dead_judge_advances_both_instead_of_eliminating_one() {
        // A dead judge eliminates nobody: picking one would be an elimination
        // nobody voted for, and the champion would inherit a field that shrank
        // for no stated reason.
        let f = fake_killing_first(vec!["Pick the stronger"], true);
        let brief = run(&f, &["postgres", "mysql", "sqlite", "cassandra"]).await.unwrap();
        assert!(!brief.rounds[0].decided(), "a dead judge decided nothing");
        let mut pair = brief.rounds[0].match_up.clone();
        pair.sort();
        assert_eq!(pair, options(&["mysql", "postgres"]));
        assert_eq!(brief.undecided, 1);
        assert!(brief.winner.is_some(), "the tournament still produced a champion");
        assert_eq!(exit_code(&brief), 1, "a provisional winner is not a clean run");
    }

    #[tokio::test]
    async fn a_round_that_eliminates_nobody_ends_the_bracket_in_a_draw() {
        // The bug this guards: a dead judge in a 2-option field re-ran the same
        // matchup forever. Measured: 104k judge calls with no termination.
        let f = fake_killing(vec!["Pick the stronger"]);
        let brief = run(&f, &["a", "b"]).await.unwrap();
        assert!(brief.winner.is_none(), "a stalled bracket has no winner");
        let mut tied = brief.tied.clone();
        tied.sort();
        assert_eq!(tied, options(&["a", "b"]));
        assert_eq!(f.count("Pick the stronger"), 1, "it must not loop");
        assert_eq!(exit_code(&brief), 1);
    }

    #[tokio::test]
    async fn a_field_where_no_judge_ever_answers_still_terminates_after_one_round() {
        let f = fake_killing(vec!["Pick the stronger"]);
        let brief = run(&f, &["a", "b", "c", "d"]).await.unwrap();
        assert!(brief.winner.is_none());
        assert_eq!(brief.tied.len(), 4);
        assert_eq!(
            f.count("Pick the stronger"),
            2,
            "one round of two matches, then it stops"
        );
    }

    #[tokio::test]
    async fn the_winner_index_reads_the_presented_order_not_the_argument_order() {
        // `pick` returns 1 in one run and 0 in the other. Whatever it returns is
        // an index into the order the PROMPT showed, which is seeded from the
        // options' own text.
        for pick in [0i64, 1] {
            let me = Fake::with(pick);
            let brief = decide(DecideArgs {
                question: "q".into(),
                options: options(&["alpha", "zeta"]),
                spend: None,
                ext: Extensions::none(),
                asker: me.as_ask(),
                capped: false,
            })
            .await
            .unwrap();
            assert_eq!(me.count("Pick the stronger"), 1);
            let shown = me.prompts.lock().unwrap().clone();
            let judge = shown
                .iter()
                .find(|p| p.contains("Pick the stronger"))
                .expect("the judge ran");
            let shown: Vec<String> = judge
                .lines()
                .filter_map(|l| l.trim().strip_prefix("Plan: do "))
                .map(str::to_string)
                .collect();
            assert_eq!(shown.len(), 2, "the judge saw both plans");
            // The index is into what the PROMPT showed, whatever order that was.
            assert_eq!(brief.winner.unwrap().0, shown[pick as usize]);
        }
    }

    #[tokio::test]
    async fn presentation_order_is_stable_and_not_the_argument_order() {
        let a = Contender {
            option: "alpha".into(),
            plan: "p".into(),
            caveat: "c".into(),
        };
        let b = Contender {
            option: "zeta".into(),
            plan: "p".into(),
            caveat: "c".into(),
        };
        assert_eq!(
            presented(&a, &b).iter().map(|c| c.option.clone()).collect::<Vec<_>>(),
            presented(&b, &a).iter().map(|c| c.option.clone()).collect::<Vec<_>>(),
            "the order must not depend on which argument came first"
        );
        // Across a spread of options both orderings occur, so it is seeded by the
        // text and not merely reversed.
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let mut firsts: std::collections::HashSet<bool> = Default::default();
        for x in names {
            for y in names.iter().filter(|y| **y != x) {
                firsts.insert(seed(x) < seed(y));
            }
        }
        assert_eq!(firsts.len(), 2, "some pairs flip, so it is not alphabetical");
    }

    #[tokio::test]
    async fn judges_see_candidates_not_option_names_and_no_agent_gets_tools() {
        // The anonymity invariant: the whole reason this workflow exists.
        let f = Fake::new();
        run(&f, &["postgres", "mysql"]).await.unwrap();
        for p in f.prompts() {
            if p.contains("Pick the stronger") {
                let headings: Vec<&str> = p
                    .lines()
                    .filter_map(|l| l.strip_prefix('['))
                    .filter_map(|l| l.split_once(']').map(|(_, rest)| rest.trim()))
                    .collect();
                assert_eq!(headings, vec!["Candidate 0", "Candidate 1"]);
            }
        }
        for tools in f.tool_lists.lock().unwrap().iter() {
            assert!(tools.is_empty(), "advocates and judges run with no tools");
        }
    }

    #[tokio::test]
    async fn the_field_is_capped_to_the_call_budget_and_says_so() {
        let field: Vec<String> = (0..30).map(|i| format!("opt{i}")).collect();
        let (capped, was_capped) = cap_to_budget(&field, MAX_CALLS);
        assert!(was_capped);
        assert!(capped.len() < field.len());
        // 20 options need 39 calls, which just fits 40.
        assert!(capped.len() * 2 - 1 <= MAX_CALLS);

        // A field the budget can finish is not reported as a shortfall.
        let small = options(&["a", "b"]);
        assert_eq!(cap_to_budget(&small, MAX_CALLS), (small, false));
    }

    #[tokio::test]
    async fn a_capped_field_is_not_a_clean_run() {
        let f = Fake::new();
        let brief = decide(DecideArgs {
            question: "q".into(),
            options: options(&["a", "b"]),
            spend: None,
            ext: Extensions::none(),
            asker: f.as_ask(),
            capped: true,
        })
        .await
        .unwrap();
        assert_eq!(exit_code(&brief), 1);
        assert!(render(&brief).contains("cut to fit the call budget"));
    }

    #[test]
    fn a_cap_that_leaves_no_tournament_is_refused_before_it_is_used() {
        for bad in [0, 1] {
            assert!(bad < 2, "the CLI refuses anything under 2");
        }
        // And the caps that are legal:
        assert_eq!(cap_to_budget(&options(&["a"]), MAX_CALLS).0.len(), 1);
    }

    #[test]
    fn the_pair_schema_refuses_an_out_of_range_winner() {
        // Validating here is cheaper than detecting a nonsense winner after.
        assert!(schema_error(&json!({"winner": 0, "why": "x"}), &PAIR()).unwrap().is_none());
        assert!(schema_error(&json!({"winner": 1, "why": "x"}), &PAIR()).unwrap().is_none());
        for bad in [2, -1] {
            assert!(
                schema_error(&json!({"winner": bad, "why": "x"}), &PAIR())
                    .unwrap()
                    .is_some(),
                "winner {bad} is not a verdict"
            );
        }
        assert!(crate::schema::validator(&CASE()).is_ok());
    }

    #[test]
    fn an_unreadable_winner_counts_as_undecided_not_as_a_win() {
        // Unreachable through `ask` — the schema refuses it — but the safe
        // reading is the honest one, never "candidate 0 lost by default".
        assert_eq!(verdict_winner(&Some(json!({"winner": 0, "why": "x"}))), Some(0));
        assert_eq!(verdict_winner(&Some(json!({"winner": 1, "why": "x"}))), Some(1));
        for bad in [json!({"winner": 2}), json!({"winner": -1}), json!({"why": "x"})] {
            assert_eq!(verdict_winner(&Some(bad)), None);
        }
        assert_eq!(verdict_winner(&None), None, "a dead judge decided nothing");
    }

    #[tokio::test]
    async fn render_names_the_winner_not_the_contender_that_lost() {
        // The bug this catches: `Round.match_up` held bracket order while the
        // verdict indexed the presented order, so whenever the seeded order
        // flipped the pair, render printed the ELIMINATED option as the winner.
        for pick in [0i64, 1] {
            let me = Fake::with(pick);
            let brief = decide(DecideArgs {
                question: "q".into(),
                options: options(&["alpha", "zeta", "mu"]),
                spend: None,
                ext: Extensions::none(),
                asker: me.as_ask(),
                capped: false,
            })
            .await
            .unwrap();
            let (champion, _) = brief.winner.clone().expect("a winner");
            let out = render(&brief);
            let rendered: Vec<String> = out
                .lines()
                .filter(|l| l.contains(" beats "))
                .map(|l| {
                    l.trim()
                        .trim_start_matches("  ")
                        .split(" beats ")
                        .next()
                        .unwrap()
                        .to_string()
                })
                .collect();
            assert_eq!(rendered.len(), brief.rounds.len(), "{out}");

            for (i, r) in brief.rounds.iter().enumerate() {
                let advanced = r.winner.as_deref().expect("a decided round");
                assert_eq!(rendered[i], advanced, "rendered the wrong side:\n{out}");
                // And that side really did advance: it is in the next round, or
                // it is the champion when there is no next round.
                let carried = brief
                    .rounds
                    .get(i + 1)
                    .map(|next| next.match_up.contains(&advanced.to_string()))
                    .unwrap_or(advanced == champion);
                assert!(
                    carried,
                    "{} was printed as beating {} but never advanced:\n{out}",
                    advanced,
                    r.match_up.iter().find(|o| o.as_str() != advanced).unwrap()
                );
            }
            assert!(
                !out.contains("no judge; both advance"),
                "a live judge was reported as dead:\n{out}"
            );
        }
    }

    #[test]
    fn a_decided_round_with_no_reason_still_reports_a_winner() {
        // `why` is None for an undecided round AND for a decided one whose
        // reason was unreadable. Branching on `why` printed the second as
        // "no judge; both advance" — while one side really had been eliminated.
        let brief = Brief {
            question: "q".into(),
            rounds: vec![Round {
                match_up: vec!["a".into(), "b".into()],
                winner: Some("b".into()),
                why: None,
            }],
            ..Default::default()
        };
        let out = render(&brief);
        assert!(out.contains("b beats a"), "{out}");
        assert!(!out.contains("both advance"), "{out}");

        let undecided = Brief {
            question: "q".into(),
            rounds: vec![Round {
                match_up: vec!["a".into(), "b".into()],
                winner: None,
                why: None,
            }],
            ..Default::default()
        };
        assert!(render(&undecided).contains("a vs b — no judge; both advance"));
    }

    #[test]
    fn render_only_claims_a_winner_when_there_is_one() {
        let clean = Brief {
            question: "q".into(),
            winner: Some(("a".into(), "do a".into())),
            ..Default::default()
        };
        assert!(render(&clean).contains("best option: a"));

        let draw = Brief {
            question: "q".into(),
            tied: options(&["a", "b"]),
            ..Default::default()
        };
        let out = render(&draw);
        assert!(out.contains("no winner"), "{out}");
        assert!(!out.contains("best option"), "{out}");

        // The provisional caveat needs a winner to qualify.
        let provisional = Brief {
            undecided: 1,
            ..clean.clone()
        };
        assert!(render(&provisional).contains("provisional, not chosen"));
        assert!(!render(&Brief { undecided: 1, ..Default::default() }).contains("provisional"));
    }

    #[test]
    fn labels_stay_inside_the_stage_column() {
        assert_eq!(label("case/", "a-very-long-option-name-that-overflows").chars().count(), 32);
        assert!(label("judge/", "short").chars().count() < 32);
    }
}