//! Agent calls with a subprocess gate, a disk cache, and a token ceiling.
//!
//! One function is the whole "framework": [`ask`]. [`gather`] is its fan-out
//! sibling, [`guard`] its input-side counterpart.

pub mod cache;
pub mod clean;
pub mod ext;
pub mod panel;
pub mod runner;
pub mod schema;
pub mod sign;
pub mod stats;
pub mod workflows;

use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::sync::Semaphore;

/// Each call spawns a CLI subprocess. Without this cap, fan-out is unbounded.
pub static GATE: Semaphore = Semaphore::const_new(8);

/// Subscription ceiling. A dead agent returns `None` rather than raising, so a
/// failed stage costs one slot, not the round.
pub const MAX_CALLS: usize = 40;

/// The tools that take a filesystem path, and the input fields holding one.
pub const PATH_TOOLS: &[(&str, &[&str])] = &[
    ("Read", &["file_path"]),
    ("Grep", &["path"]),
    ("Glob", &["path", "pattern"]),
];

/// The one error type a workflow must handle. [`OrchError::Budget`] and
/// [`OrchError::Extension`] are fatal and bubble through [`gather`];
/// [`OrchError::Seam`] and [`OrchError::Schema`] are raised by [`guard`].
#[derive(Debug, Clone)]
pub enum OrchError {
    Budget(String),
    Extension(String),
    Seam(String),
    /// A schema that will not compile. Distinct from `Seam`: the payload may be
    /// fine, the rule it is being judged by is not.
    Schema(String),
}

impl std::fmt::Display for OrchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchError::Budget(m)
            | OrchError::Extension(m)
            | OrchError::Seam(m)
            | OrchError::Schema(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for OrchError {}

use sha2::Digest;

use crate::clean::clean;
use crate::stats::{add_usage, with_stats};

static WARNED_UNCONFINED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What the CLI implies when it returns a result with no `structured_output`:
/// the agent answered in prose instead of calling the StructuredOutput tool.
const NARRATED: &str = "you returned no structured output — answer only through the StructuredOutput tool, not prose";

/// One gated agent call: hold a slot, then run.
async fn gated(spec: &runner::CallSpec) -> Result<runner::CallResult, String> {
    let _permit = GATE.acquire().await.expect("gate semaphore closed");
    runner::run(spec).await
}

pub struct AskOpts {
    pub tools: Vec<String>,
    pub use_cache: bool,
    pub system: Option<String>,
    pub cwd: PathBuf,
    pub label: String,
    pub depends_on: Option<PathBuf>,
    pub max_budget_usd: Option<f64>,
    pub ext: ext::Extensions,
    pub reads: Vec<PathBuf>,
    /// Run-wide countdown on agent calls. Taken once before the first call and
    /// again before a schema retry, so a retry cannot slip past the ceiling.
    pub spend: Option<std::sync::Arc<stats::Spend>>,
}

impl Default for AskOpts {
    fn default() -> Self {
        Self {
            tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            use_cache: true,
            system: None,
            cwd: PathBuf::from("."),
            label: String::new(),
            depends_on: None,
            max_budget_usd: None,
            ext: ext::Extensions::none(),
            reads: Vec::new(),
            spend: None,
        }
    }
}

/// Cache key: explicit fields, not a merged string — a system prompt
/// concatenated onto tools can collide with a different tools tuple.
#[allow(clippy::too_many_arguments)] // one field per key component, by design
pub fn cache_key(
    prompt: &str,
    schema: &Value,
    tools: &[String],
    system: &str,
    cwd: &str,
    depends_on: &str,
    extensions: &str,
    reads: &str,
) -> String {
    let mut tools_sorted = tools.to_vec();
    tools_sorted.sort();
    let mut fields = vec![
        Value::String(prompt.to_string()),
        schema.clone(),
        Value::Array(tools_sorted.into_iter().map(Value::String).collect()),
        Value::String(system.to_string()),
        Value::String(cwd.to_string()),
        Value::String(depends_on.to_string()),
    ];
    if !extensions.is_empty() {
        fields.push(Value::String(extensions.to_string()));
    }
    if !reads.is_empty() {
        fields.push(json_arr(vec![Value::String("reads".into()), Value::String(reads.to_string())]));
    }
    sign::hex(&sha2::Sha256::digest(Value::Array(fields).to_string().as_bytes()))[..16].to_string()
}

fn json_arr(v: Vec<Value>) -> Value {
    Value::Array(v)
}

/// One agent call. Returns output that validates against `schema`, else
/// `Ok(None)`.
///
/// One retry is spent on a schema mismatch before giving up — the CLI returns
/// `structured_output` unvalidated, and an invalid payload would otherwise be
/// indistinguishable from a dead agent.
///
/// `depends_on` points at a source tree to fingerprint into the cache key. Use
/// it whenever the answer depends on files the prompt never quotes — otherwise
/// the cache happily serves a review of code that has since changed.
///
/// `label` names this stage in failure output — without it a dead stage is just
/// an anonymous `None`, indistinguishable from a stage that found nothing.
///
/// `max_budget_usd` caps what ONE call may spend, checked by the CLI between
/// turns. Distinct from `spend`, which caps calls for the whole run: this one
/// stops a single runaway agent, that one bounds the fan-out. Exceeding it
/// raises [`OrchError::Budget`] rather than returning `None` — a run that ran
/// out of money is not a run with a dead stage.
///
/// `reads` grants extra read-only roots, enforced by the PreToolUse `confine`
/// hook. Their contents are not fingerprinted; put a digest of what matters in
/// the prompt.
pub async fn ask(prompt: &str, schema: &Value, opts: AskOpts) -> Result<Option<Value>, OrchError> {
    let AskOpts {
        tools,
        use_cache,
        system,
        cwd,
        label,
        depends_on,
        max_budget_usd,
        ext,
        reads,
        spend,
    } = opts;

    let resolved = ext.resolve()?;
    let mut extra_reads = reads
        .iter()
        .map(|r| ext::expanduser(r).canonicalize().unwrap_or_else(|_| ext::expanduser(r)))
        .collect::<Vec<_>>();

    let label = with_stats(|s| s.unique(&label));

    // A schema that will not compile validates every payload, so refuse before
    // spending a call or writing a cache entry. Logged as a finished stage so
    // the panel pairs the start with a terminal row rather than leaving it
    // hanging at "running".
    let compiled = match crate::schema::validator(schema) {
        Ok(v) => v,
        Err(bad) => {
            with_stats(|s| s.dead += 1);
            with_stats(|s| s.note(&label, "started"));
            with_stats(|s| s.finish(&label, "invalid_schema", stats::Usage::default(), false));
            println!("  stage invalid [{label}]: schema will not compile: {bad}");
            return Ok(None);
        }
    };

    // Whether a call was confined belongs in the key: an entry produced while
    // the hook was missing must not be served to a confined run, and vice versa.
    let confine = confine_bin();
    let confined = confine.is_some();
    if !confined && !WARNED_UNCONFINED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        println!(
            "  WARNING: the `confine` hook was not found, so reads are NOT confined to the \
working directory.\n           Every path the agent is given is readable — including files \
outside the target.\n           Build it (`cargo build --release`) or set RUSTFLOWS_CONFINE to \
its path."
        );
    }

    let key = cache_key(
        prompt,
        schema,
        &tools,
        system.as_deref().unwrap_or(""),
        &cwd.display().to_string(),
        &depends_on
            .as_deref()
            .map(|d| sign::stat_signature(d, None))
            .unwrap_or_default(),
        &format!("confined={confined};{}", ext.key()),
        &if extra_reads.is_empty() {
            String::new()
        } else {
            let mut v: Vec<String> = extra_reads.iter().map(|r| r.display().to_string()).collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect()).to_string()
        },
    );

    let cache = cache::default_cache();
    if use_cache
        && let Some(hit) = cache.get(&key, &label) {
            with_stats(|s| s.cache_hits += 1);
            with_stats(|s| s.note(&label, "cache_hit"));
            return Ok(Some(clean(&hit))); // entries cached before cleaning existed
        }

    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    let mut hook_roots = resolved.reads.clone();
    hook_roots.append(&mut extra_reads);

    let spec = runner::CallSpec {
        prompt: prompt.to_string(),
        schema: schema.clone(),
        tools,
        system,
        cwd: root.clone(),
        max_budget_usd,
        setting_sources: Some(resolved.setting_sources.clone()),
        settings_json: confine
            .as_deref()
            .and_then(|bin| runner::confine_settings(bin, &root, &hook_roots)),
        extra_allowed: resolved.extra_allowed.clone(),
        plugins: resolved.plugins.clone(),
        mcp_servers: resolved.mcp_servers.clone(),
    };

    if let Some(spend) = &spend {
        spend.take()?;
    }

    // Logged before the call so a watcher can see in-flight agents, not just
    // finished ones. Followed by exactly one terminal record per path below.
    with_stats(|s| s.note(&label, "started"));

    let result = match gated(&spec).await {
        Ok(r) => r,
        Err(e) => {
            with_stats(|s| s.dead += 1);
            with_stats(|s| s.note(&label, "failed"));
            println!("  stage failed [{label}]: {e}");
            return Ok(None);
        }
    };

    let mut usage = with_stats(|s| s.record(&result));
    // Checked before structured_output, because a budget stop arrives as a
    // well-formed result with is_error=true and no output — exactly the shape
    // of a dead agent. Counting that as dead makes "ran out of money" report
    // as "all lenses failed".
    if result.over_budget() {
        with_stats(|s| s.finish(&label, "over_budget", usage, false));
        return Err(budget_error(&label, max_budget_usd, result.total_cost_usd));
    }
    // An errored result is a dead stage even when it carries a payload: the CLI
    // flags `error_during_execution` with a partial structured_output, and that
    // used to be recorded `ok` and written to the disk cache — so a failed
    // stage's partial answer was served to every later run.
    if result.is_error {
        with_stats(|s| s.dead += 1);
        with_stats(|s| s.finish(&label, "error", usage, false));
        println!(
            "  stage failed [{label}]: {}",
            if result.subtype.is_empty() {
                "the CLI reported an error".to_string()
            } else {
                format!("the CLI reported {}", result.subtype)
            }
        );
        return Ok(None);
    }

    // An absent payload is as wrong as an invalid one. The CLI reports
    // `subtype: success, is_error: false, stop_reason: end_turn` when the agent
    // narrates in prose instead of calling StructuredOutput — that arrived as a
    // dead `no_output` with no retry, and it is what kept killing the
    // performance lens. Both failures get one correction.
    let mut out = result.structured_output.clone();
    let mut error = match &out {
        None => Some(NARRATED.to_string()),
        Some(v) => crate::schema::schema_error_with(&compiled, v).map_err(OrchError::Schema)?,
    };

    if let Some(reason) = error {
        if let Some(spend) = &spend
            && let Err(e) = spend.take() {
                with_stats(|s| s.finish(&label, "over_budget", usage, false));
                // `Spend::take` already names *its own* ceiling. Rebuilding the
                // message from the MAX_CALLS constant reported "exhausted
                // 40-call budget" for any Spend with a different budget.
                return Err(e);
            }
        let retry_prompt = format!(
            "{prompt}\n\nYour previous response did not satisfy the required schema: {reason}\n\
Return corrected JSON matching this schema exactly, by calling the StructuredOutput tool:\n{}",
            serde_json::to_string_pretty(schema).unwrap_or_default()
        );
        let retry_spec = runner::CallSpec { prompt: retry_prompt, ..spec.clone() };
        let retry = match gated(&retry_spec).await {
            Ok(r) => r,
            Err(e) => {
                with_stats(|s| s.dead += 1);
                with_stats(|s| s.finish(&label, "failed_retry", usage, true));
                println!("  stage failed [{label}] retry: {e}");
                return Ok(None);
            }
        };
        usage = add_usage(usage, with_stats(|s| s.record(&retry)));
        if retry.over_budget() {
            with_stats(|s| s.finish(&label, "over_budget", usage, true));
            return Err(budget_error(
                &format!("{label} (retry)"),
                max_budget_usd,
                retry.total_cost_usd,
            ));
        }
        // Same rule as the first call: an errored result is a dead stage even
        // when it carries a payload. This check existed only on the first-call
        // path, so a failed retry's partial answer was recorded `ok` and cached.
        if retry.is_error {
            with_stats(|s| s.dead += 1);
            with_stats(|s| s.finish(&label, "error", usage, true));
            println!(
                "  stage failed [{label}] retry: the CLI reported {}",
                if retry.subtype.is_empty() {
                    "an error".to_string()
                } else {
                    retry.subtype.clone()
                }
            );
            return Ok(None);
        }
        out = retry.structured_output;
        error = match &out {
            None => Some(NARRATED.to_string()),
            Some(v) => crate::schema::schema_error_with(&compiled, v).map_err(OrchError::Schema)?,
        };
        if let Some(bad) = error {
            with_stats(|s| s.dead += 1);
            with_stats(|s| s.finish(
                &label,
                if out.is_none() { "no_output" } else { "invalid" },
                usage,
                true,
            ));
            println!("  stage invalid [{label}]: {bad}");
            return Ok(None);
        }
        with_stats(|s| s.finish(&label, "ok", usage, true));
        let out = clean(&out.expect("payload validated above"));
        if use_cache {
            cache.set(&key, &out, &label);
        }
        return Ok(Some(out));
    }

    with_stats(|s| s.finish(&label, "ok", usage, false));
    // Cleaned once here, so no workflow has to remember to before printing.
    let out = clean(&out.expect("payload validated above"));
    if use_cache {
        cache.set(&key, &out, &label);
    }
    Ok(Some(out))
}

/// The seam a workflow calls instead of [`ask`] directly.
///
/// pyflows' test suites monkeypatched their module's `ask` with a fake that
/// dispatched on the prompt text. Without that, the invariants that matter most
/// here are untestable: *a dead judge eliminates nobody*, *a dead verifier
/// leaves its claims unconfirmed rather than refuted*, *a budget cut audits only
/// what fits*. None of that is visible in a prompt string.
///
/// Per-run rather than a global override, so one test cannot leak into another.
pub type AskFn = std::sync::Arc<
    dyn Fn(String, Value, AskOpts)
        -> futures::future::BoxFuture<'static, Result<Option<Value>, OrchError>>
        + Send
        + Sync,
>;

/// The real thing, wrapped for [`AskFn`]. What every caller passes in production.
pub fn real_ask() -> AskFn {
    std::sync::Arc::new(|p, s, o| Box::pin(async move { ask(&p, &s, o).await }))
}

/// Whether a URL is safe to hand an agent that will fetch it.
///
/// A searcher's output is model text from web pages. Without a check, a single
/// returned `http://169.254.169.254/latest/meta-data/` becomes a fetch against
/// the host's metadata service by an agent holding the repo — and `file://` or a
/// bare path would not even need a server.
///
/// ponytail: string-level only. A public hostname that resolves to a private
/// address passes, and a redirect to one is not re-checked, because the agent
/// does the fetching and we never see the resolved address. Pin the egress
/// instead if that matters: give the call a proxy that only reaches the internet.
pub fn safe_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip the port, but only when it really is one.
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty())
        .map_or(authority, |(h, _)| h)
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && blocked_ip(ip)
    {
        return false;
    }
    true
}

/// An address that reaches this host, this network, or nowhere.
fn blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            // `::ffff:a.b.c.d` is an IPv4 address wearing a hat, and
            // `is_loopback()` does not see through it — so unwrap first, or
            // `::ffff:127.0.0.1` walks straight into the V6 rules and out.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_ip(std::net::IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local, fe80::/10 link-local
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Turn a CLI budget stop into a budget error instead of a dead stage.
///
/// The CLI checks the ceiling between turns, so it can overshoot by the cost of
/// one turn — $0.01 came back at $0.0322 in testing. Raise with the real spend,
/// not the ceiling, or the message reads as "we spent $0.01".
fn budget_error(label: &str, max_budget_usd: Option<f64>, spent: f64) -> OrchError {
    OrchError::Budget(match max_budget_usd {
        Some(ceiling) => format!("stage '{label}' hit its ${ceiling} budget after ${spent:.4}"),
        None => format!("stage '{label}' exceeded its budget after ${spent:.4}"),
    })
}

/// `join_all` with `None` in the slot of every call that failed.
///
/// [`OrchError`] bubbles: a budget stop or a bad extension is the whole run's
/// problem, not one stage's.
pub async fn gather<F, T>(futures: Vec<F>) -> Result<Vec<Option<T>>, OrchError>
where
    F: std::future::Future<Output = Result<Option<T>, OrchError>> + Send,
    T: Send,
{
    // Genuinely concurrent. The hand-rolled version this replaced drained
    // `for fut in futures { fut.await }` — each call ran to completion before
    // the next was polled, so the 8-slot GATE was decorative and a 4-lens
    // review took the sum of four agent calls rather than the max.
    let results = futures::future::join_all(futures).await;
    for r in &results {
        if let Err(e) = r {
            return Err(e.clone());
        }
    }
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Ok(v) => out.push(v),
            Err(_) => out.push(None),
        }
    }
    Ok(out)
}

/// Locate the `confine` PreToolUse hook binary: next to this executable first,
/// then `$RUSTFLOWS_CONFINE`.
///
/// `None` means **reads are not confined at all**. `cwd` alone is not a
/// boundary: `Read` is allowlisted wholesale, so `~/.ssh/id_rsa` and any other
/// absolute path on the host become readable. `ask` warns when this returns
/// `None` rather than letting the boundary vanish in silence.
pub fn confine_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RUSTFLOWS_CONFINE") {
        let p = PathBuf::from(p);
        if stats::is_executable_file(&p) {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for name in ["confine", "confine.exe"] {
        let candidate = dir.join(name);
        if stats::is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Read `$HOME`-relative paths. A bare `~` is not expanded by `Path::new`, which
/// would make the default land inside the repo.
pub fn env_path(var: &str, default: &str) -> PathBuf {
    let raw = std::env::var(var).unwrap_or_else(|_| default.to_string());
    ext::expanduser(Path::new(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_key_separates_prompt_from_schema() {
        let a = cache_key("p", &json!({"type": "object"}), &["Read".into()], "", ".", "", "", "");
        let b = cache_key("p", &json!({"type": "string"}), &["Read".into()], "", ".", "", "", "");
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn cache_key_ignores_tool_order() {
        let a = cache_key("p", &json!({}), &["Read".into(), "Grep".into()], "", ".", "", "", "");
        let b = cache_key("p", &json!({}), &["Grep".into(), "Read".into()], "", ".", "", "", "");
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_is_not_confused_by_field_boundaries() {
        // A system prompt appended to tools must not equal a different split.
        let a = cache_key("p", &json!({}), &["Read".into()], "Grep", ".", "", "", "");
        let b = cache_key("p", &json!({}), &["Read".into(), "Grep".into()], "", ".", "", "", "");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_reads_and_extensions() {
        let base = cache_key("p", &json!({}), &[], "", ".", "", "", "");
        assert_ne!(base, cache_key("p", &json!({}), &[], "", ".", "", "", "/tmp"));
        assert_ne!(base, cache_key("p", &json!({}), &[], "", ".", "", "ext", ""));
    }

    #[test]
    fn a_confined_key_differs_from_an_unconfined_one() {
        // An entry produced while the hook binary was missing must never be
        // served to a confined run — that is how a canary read got cached and
        // replayed on the next run.
        let base = cache_key("p", &json!({}), &[], "", ".", "", "", "");
        let confined = cache_key("p", &json!({}), &[], "", ".", "", "confined=true;", "");
        let unconfined = cache_key("p", &json!({}), &[], "", ".", "", "confined=false;", "");
        assert_ne!(base, confined);
        assert_ne!(confined, unconfined);
    }

    #[test]
    fn budget_error_reports_the_spend_not_the_ceiling() {
        let e = budget_error("find/security", Some(0.01), 0.0322).to_string();
        assert!(e.contains("$0.01"), "{e}");
        assert!(e.contains("$0.0322"), "{e}");
        let e = budget_error("x", None, 1.5).to_string();
        assert!(e.contains("$1.5000"), "{e}");
    }

    #[tokio::test]
    async fn gather_runs_its_futures_concurrently() {
        // The hand-rolled join drained them one at a time, so a 4-lens review
        // cost the sum of four agent calls. Gate each future on a shared start
        // and assert they all overlap.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let make = |i: usize| {
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, OrchError>(Some(i))
            }
        };
        let results = gather((0..4).map(make).collect()).await.unwrap();
        assert_eq!(results, vec![Some(0), Some(1), Some(2), Some(3)]);
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "gather ran its futures one after another"
        );
    }

    #[tokio::test]
    async fn gather_returns_none_slots_and_bubbles_fatal_errors() {
        use std::future::ready;
        let fatal = gather(vec![
            ready(Ok::<_, OrchError>(Some(1))),
            ready(Err::<Option<i32>, _>(OrchError::Budget("out".into()))),
            ready(Ok(Some(3))),
        ])
        .await;
        assert!(fatal.is_err(), "a budget stop is fatal for the whole run");

        let mixed = gather(vec![
            ready(Ok::<_, OrchError>(Some(1))),
            ready(Ok(None)),
            ready(Ok(Some(3))),
        ])
        .await
        .unwrap();
        assert_eq!(mixed, vec![Some(1), None, Some(3)]);
    }

    #[test]
    fn env_path_expands_a_leading_tilde() {
        // Path::new("~/.cache/x") is relative; the default must not be.
        assert!(env_path("RUSTFLOWS_DO_NOT_EXIST_XYZ", "~/.cache/rustflows-t").is_absolute());
    }

    #[test]
    fn a_url_an_agent_would_fetch_must_not_reach_the_host() {
        for ok in [
            "https://example.com",
            "http://example.com/page?q=1",
            "https://example.com:8443/x",
            "https://sub.domain.example.org/",
        ] {
            assert!(safe_url(ok), "{ok} should be allowed");
        }
        for bad in [
            "",
            "not a url",
            "file:///etc/passwd",
            "ftp://example.com",
            "http://",
            "http://localhost",
            "http://localhost:8080/x",
            "http://sub.localhost",
            "http://db.internal/",
            "http://redis.local/",
            // The cloud metadata service, on every host.
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9000/",
            "http://10.0.0.5/admin",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            // IPv6-mapped loopback.
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(!safe_url(bad), "{bad} must not be fetched");
        }
    }
}