//! `rustflows` — CLI for the ported framework and its workflows.

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};
use rustflows::ext::Extensions;
use rustflows::stats::{log_run, with_stats};
use rustflows::workflows::decide;
use rustflows::workflows::research;
use rustflows::workflows::review::{
    ReviewArgs, ReviewError, check_target, choose_lenses, known_lens, print_confirmed, review,
};
use rustflows::workflows::upgrade;

#[derive(Parser)]
#[command(
    name = "rustflows",
    about = "Agent orchestration in Rust: a subprocess gate, a disk cache, and a token ceiling",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Review a source tree through independent lenses, then refute.
    Review {
        #[arg(default_value = ".")]
        target: PathBuf,
        /// One lens, one verifier — cheap, but no independent cross-check.
        #[arg(long)]
        quick: bool,
        /// Limit finder agents.
        #[arg(long)]
        lenses: Option<usize>,
        /// Run this lens (repeatable); --lenses then caps the chosen list.
        #[arg(long = "lens")]
        lens: Vec<String>,
        #[command(flatten)]
        ext: ExtArgs,
    },
    /// Research a question, then check its citations.
    Research {
        question: String,
        /// Limit search angles.
        #[arg(long)]
        angles: Option<usize>,
        /// Citations to verify.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Skip citation checking.
        #[arg(long = "no-verify")]
        no_verify: bool,
        #[command(flatten)]
        ext: ExtArgs,
    },
    /// Audit dependency upgrades against a repo.
    Upgrade {
        #[arg(default_value = ".")]
        target: PathBuf,
        /// Max dependencies to audit.
        #[arg(long)]
        limit: Option<usize>,
        /// Candidate breaks to verify.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Skip verification.
        #[arg(long = "no-verify")]
        no_verify: bool,
        #[command(flatten)]
        ext: ExtArgs,
    },
    /// Pick between options by knockout tournament.
    Decide {
        question: String,
        /// Two or more options to choose between.
        #[arg(required = true, num_args = 1..)]
        options: Vec<String>,
        /// Cap the field size.
        #[arg(long)]
        max: Option<i64>,
        #[command(flatten)]
        ext: ExtArgs,
    },
    /// Browse agents from past runs.
    Agents {
        /// How many runs back.
        #[arg(short = 'n', long, default_value_t = 1)]
        last: usize,
        /// Emit the raw records.
        #[arg(long)]
        json: bool,
    },
    /// Live view of the running agents.
    Watch {
        /// Refresh seconds.
        #[arg(long, default_value_t = 0.5)]
        interval: f64,
    },
}

/// Extensions (agents load none unless named).
#[derive(clap::Args)]
struct ExtArgs {
    /// Load a local plugin dir (repeatable).
    #[arg(long = "plugin", value_name = "DIR")]
    plugin: Vec<String>,
    /// Load your *user* settings, skills and MCP servers. The target repo's own
    /// settings are never loaded: a reviewed tree must not supply the rules.
    #[arg(long = "inherit-settings")]
    inherit_settings: bool,
    /// Grant a user skill. Rejected: the CLI has no flag to select a named skill.
    #[arg(long = "skill", value_name = "NAME")]
    skill: Vec<String>,
}

impl ExtArgs {
    fn into_extensions(self) -> Extensions {
        Extensions {
            skills: self.skill,
            plugins: self.plugin,
            mcp: serde_json::Map::new(),
            inherit: self.inherit_settings,
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match Cli::parse().command {
        Command::Agents { last, json } => {
            std::process::ExitCode::from(rustflows::panel::show_agents(last, json) as u8)
        }
        Command::Watch { interval } => {
            // `Duration::from_secs_f64` panics on a non-finite input, and Rust's
            // f64 FromStr — which is what clap parses this with — accepts "inf".
            if !interval.is_finite() {
                println!("--interval must be a finite number of seconds");
                return std::process::ExitCode::from(2);
            }
            std::process::ExitCode::from(rustflows::panel::watch(interval).await as u8)
        }
        Command::Review { target, quick, lenses, lens, ext } => {
            std::process::ExitCode::from(run_review(target, quick, lenses, lens, ext).await as u8)
        }
        Command::Research { question, angles, depth, no_verify, ext } => std::process::ExitCode::from(
            run_research(question, angles, depth, no_verify, ext).await as u8,
        ),
        Command::Upgrade { target, limit, depth, no_verify, ext } => {
            std::process::ExitCode::from(run_upgrade(target, limit, depth, no_verify, ext).await as u8)
        }
        Command::Decide { question, options, max, ext } => {
            std::process::ExitCode::from(run_decide(question, options, max, ext).await as u8)
        }
    }
}

/// Shared preamble for the three non-review workflows.
fn begin(target: &str, mode: &str, fanout: usize, unit: &str) {
    rustflows::stats::with_stats(|s| s.reset());
    rustflows::stats::log_run(serde_json::json!({
        "outcome": "run_start",
        "target": target,
        "lenses": fanout,
        "mode": mode,
    }));
    println!("{mode}: {fanout} {unit}");
}

/// The trailing summary every workflow prints.
fn tail(started: Instant) {
    println!("{}", rustflows::stats::with_stats(|s| s.summary()));
    println!("wall clock: {:.0}s", started.elapsed().as_secs_f64());
}

async fn run_review(
    target: PathBuf,
    quick: bool,
    lenses: Option<usize>,
    lens: Vec<String>,
    ext: ExtArgs,
) -> i32 {
    if let Err(e) = check_target(&target) {
        println!("{e}");
        return 1;
    }
    for l in &lens {
        if !known_lens(l) {
            println!("unknown lens: {l}\n{}", lens_names());
            return 2;
        }
    }

    let chosen = choose_lenses(&lens, quick, lenses);

    // `--lenses 0` selects nothing. That is a configuration error, and
    // reporting it as a dead run blames agents that never ran.
    if chosen.is_empty() {
        println!("no lenses selected — --lenses caps the list and must be at least 1\n{}", lens_names());
        return 2;
    }
    // Quick mode forces one batch regardless of how many findings turn up.
    let batch_size = if quick { Some(rustflows::MAX_CALLS) } else { None };
    let ext = ext.into_extensions();
    if let Err(e) = ext.check() {
        println!("{e}");
        return 2;
    }

    with_stats(|s| s.reset());
    let started = Instant::now();
    let mode = if quick { "quick" } else { "full" };
    log_run(serde_json::json!({
        "outcome": "run_start",
        "target": target.display().to_string(),
        "lenses": chosen.len(),
        "mode": mode,
    }));
    println!(
        "{mode}: {} {} on {}",
        chosen.len(),
        if chosen.len() == 1 { "lens" } else { "lenses" },
        target.display()
    );

    let result = review(ReviewArgs { target, lenses: chosen.clone(), batch_size, ext }).await;
    let result = match result {
        Ok(r) => r,
        Err(ReviewError::Failed(m)) | Err(ReviewError::Orch(rustflows::OrchError::Budget(m))) => {
            println!("review did not complete: {m}\n{}", with_stats(|s| s.summary()));
            return 1;
        }
        Err(ReviewError::Orch(e)) => {
            println!("review did not complete: {e}\n{}", with_stats(|s| s.summary()));
            return 1;
        }
    };

    print_confirmed(&result.confirmed);
    if result.confirmed.is_empty() {
        println!("nothing survived verification");
    } else {
        println!("\n{} confirmed", result.confirmed.len());
    }
    if result.unconfirmed > 0 {
        println!(
            "{} findings NOT verified — this review is partial, not clean",
            result.unconfirmed
        );
    }
    if result.dead > 0 {
        println!(
            "{}/{} lenses failed — this review is partial, not clean",
            result.dead,
            chosen.len()
        );
    }
    println!("{}", with_stats(|s| s.summary()));
    println!("wall clock: {:.0}s", started.elapsed().as_secs_f64());
    // Same rule as a dead lens: findings nobody judged are not findings refuted.
    i32::from(result.unconfirmed > 0 || result.dead > 0)
}

/// Every lens this workflow knows, so a typo is self-correcting.
fn lens_names() -> String {
    let mut names: Vec<&str> = rustflows::workflows::review::LENSES.to_vec();
    names.extend_from_slice(rustflows::workflows::review::OPT_IN_LENSES);
    format!("known lenses: {}", names.join(", "))
}

async fn run_research(
    question: String,
    angles: Option<usize>,
    depth: usize,
    no_verify: bool,
    ext: ExtArgs,
) -> i32 {
    // A "how many" flag of 0 is a configuration error, not a request for zero
    // work: pyflows reads it as falsy and quietly runs all four angles.
    if angles == Some(0) || depth == 0 {
        println!("--angles and --depth must be at least 1 (--no-verify skips checking entirely)");
        return 2;
    }
    let ext = ext.into_extensions();
    if let Err(e) = ext.check() {
        println!("{e}");
        return 2;
    }
    let started = Instant::now();
    let n = angles.unwrap_or(rustflows::workflows::research::ANGLE_NAMES.len()).min(
        rustflows::workflows::research::ANGLE_NAMES.len(),
    );
    begin(&question, "research", n, if n == 1 { "angle" } else { "angles" });

    let result = research::research(research::ResearchArgs {
        question,
        angles,
        depth,
        verify: !no_verify,
        spend: None,
        ext,
        asker: rustflows::real_ask(),
    })
    .await;
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            println!("research did not complete: {e}\n{}", rustflows::stats::with_stats(|s| s.summary()));
            return 1;
        }
    };

    print!("{}", research::render(&result));
    tail(started);
    research::exit_code(&result)
}

async fn run_upgrade(
    target: PathBuf,
    limit: Option<usize>,
    depth: usize,
    no_verify: bool,
    ext: ExtArgs,
) -> i32 {
    if limit == Some(0) || depth == 0 {
        println!("--limit and --depth must be at least 1 (--no-verify skips verification entirely)");
        return 2;
    }
    // Both checked before a single agent call: a wrong target should not cost money.
    if let Err(e) = check_target(&target) {
        println!("{e}");
        return 2;
    }
    let Some(manifest) = upgrade::find_manifest(&target) else {
        println!(
            "no manifest found (looked for {})",
            upgrade::MANIFESTS.join(", ")
        );
        return 2;
    };
    let ext = ext.into_extensions();
    if let Err(e) = ext.check() {
        println!("{e}");
        return 2;
    }
    let started = Instant::now();
    begin(
        &format!("{} ({manifest})", target.display()),
        "upgrade",
        0,
        "dependencies (count discovered at runtime)",
    );

    let result = upgrade::upgrade(upgrade::UpgradeArgs {
        target,
        limit,
        depth,
        verify: !no_verify,
        spend: None,
        ext,
        asker: rustflows::real_ask(),
    })
    .await;
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            println!("upgrade did not complete: {e}\n{}", rustflows::stats::with_stats(|s| s.summary()));
            return 1;
        }
    };

    print!("{}", upgrade::render(&result));
    tail(started);
    upgrade::exit_code(&result)
}

async fn run_decide(
    question: String,
    options: Vec<String>,
    max: Option<i64>,
    ext: ExtArgs,
) -> i32 {
    // `is not None`, not truthiness: `--max 0` is a request for an empty field,
    // and a negative one slices the option off the end. Both are user error.
    if let Some(m) = max
        && m < 2
    {
        println!("--max {m} leaves no tournament; it needs at least 2");
        return 2;
    }
    let field = decide::choose_options(&options);
    if field.len() < 2 {
        println!("need at least two distinct options");
        return 2;
    }
    let field: Vec<String> = match max {
        // A cap above the field size must not claim a truncation it did not do.
        Some(m) if (m as usize) < field.len() => {
            println!("capped from {} to {m}", field.len());
            field[..m as usize].to_vec()
        }
        _ => field,
    };
    let ext = ext.into_extensions();
    if let Err(e) = ext.check() {
        println!("{e}");
        return 2;
    }
    let started = Instant::now();

    // An advocate plus its judges is about two calls per option.
    let spend = std::sync::Arc::new(rustflows::stats::Spend::new(rustflows::MAX_CALLS));
    let (field, capped) = decide::cap_to_budget(&field, spend.remaining());
    if capped {
        println!("only {}/{} options fit the budget", field.len(), field.len());
    }
    begin(&question, "decide", field.len(), if field.len() == 1 { "option" } else { "options" });

    let result = decide::decide(decide::DecideArgs {
        question,
        options: field,
        spend: Some(spend),
        ext,
        asker: rustflows::real_ask(),
        capped,
    })
    .await;
    let brief = match result {
        Ok(b) => b,
        Err(e) => {
            println!("decision did not complete: {e}\n{}", rustflows::stats::with_stats(|s| s.summary()));
            return 1;
        }
    };

    print!("{}", decide::render(&brief));
    tail(started);
    decide::exit_code(&brief)
}
