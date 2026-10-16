# rustflows

Agent orchestration in Rust. Part of a program is ordinary code; some
expressions are agent calls. The code decides what to call, in what order, and
when to stop. The agent decides the answer.

A port of [pyflows](../pyflows): the framework (`orch.py` + `panel.py`) plus all
four workflows — `review`, `research`, `upgrade` and `decide`.

## How it talks to Claude

No SDK. `claude` is spawned directly with
`--output-format stream-json --verbose`, and the terminal `result` line is
parsed for `structured_output`, `usage` and `total_cost_usd`.

That last part is load-bearing: **`--output-format json` has no
`structured_output` field.** The CLI injects a `StructuredOutput` *tool*, and
only the streaming result message reports what the model passed it. Using plain
`json` silently loses every stage's payload.

Uses your existing Claude Code login — Pro, Max, Team or Enterprise. No API key.
In CI, set `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`.

## Build and test

```bash
cargo build --release
cargo test                    # unit tests, no network, no cost
cargo test -- --ignored       # end-to-end: spawns real agent calls
```

`cargo install --path .` puts `rustflows` on your path. The `confine` hook ships
in the same crate; `rustflows` finds it next to its own binary.

## Commands

```bash
rustflows review src              # 4 lenses, batched verification
rustflows review src --quick      # 1 lens, 1 verifier — ~4× cheaper
rustflows review src --lenses 2   # middle ground
rustflows review src --lens performance --lens error-handling
rustflows review src --lens test-quality   # opt-in lens

rustflows research "does X do Y"    # multi-modal sweep → synthesize → verify
rustflows research "q" --angles 2 --depth 1   # fewer angles, more citations checked
rustflows research "q" --no-verify  # skip citation checking

rustflows upgrade .                # what would upgrading this repo's deps break?
rustflows upgrade . --limit 2      # audit the 2 highest-blast-radius dependencies
rustflows upgrade . --depth 5      # verify 5 candidate breaks instead of 3

rustflows decide "which queue?" redis rabbitmq kafka   # pick by tournament
rustflows decide "q" "a" "b" --max 4   # enter only the first 4 options

rustflows agents                  # browse agents from past runs
rustflows agents --json           # the raw run-log records
rustflows watch                   # live view as agents report in
```

## The framework

```rust
let found = gather(lenses.iter().map(|lens| ask(&prompt, &FINDINGS, AskOpts {
    cwd: target.clone(),
    depends_on: Some(target.clone()),
    label: format!("find/{lens}"),
    ..Default::default()
})).collect()).await?;
```

- **`ask`** — one agent call. Returns `Ok(None)` for a dead stage, so a failed
  call costs one slot rather than the round. One retry is spent on a schema
  mismatch: the CLI hands back `structured_output` unvalidated, and an invalid
  payload would otherwise be indistinguishable from a dead agent.
- **`gather`** — fan-out with `None` in the slot of every failed call.
  `BudgetExhausted` and extension errors bubble: those are the whole run's
  problem, not one stage's.
- **`guard`** — input-side guardrail. Same validator `ask` uses, so a payload is
  judged by one rule at both ends of a handoff.
- **`clean`** — strips terminal control characters and bidi overrides from model
  output before it reaches your terminal.
- **`stat_signature`** — tree fingerprint for the cache key.
- **`AskFn`** — the call seam the workflows use instead of `ask` directly, so
  the orchestration invariants are testable without spawning an agent.

## The four workflows

| | shape | what makes it the interesting one |
|---|---|---|
| `review` | find → barrier → dedup → verify | fixed fan-out over a lens tuple |
| `research` | sweep → synthesize → verify | four *modality* angles, blind to each other; the synthesizer reports contradictions rather than resolving them |
| `upgrade` | inventory → sweep → verify | fan-out width is **discovered at runtime** — the inventory decides how many agents run |
| `decide` | advocate → bracket → winner | a knockout bracket, not a pipeline: each verdict decides who plays next |

`decide` is also the cheapest: no agent reads a repo, so nothing in its cache key
depends on one and it costs nothing to re-run.

### Exit codes

Uniform across all four:

| | |
|---|---|
| **0** | clean |
| **1** | ran partially or not at all — something was unchecked, unconfirmed, dead, or cut short |
| **2** | configuration error: unknown lens, `--angles 0`, no manifest, fewer than two options |

The honesty rule: `safe to bump`, `All N checked citations held up` and
`best option:` print **only** when nothing is unchecked, unconfirmed, dead or
skipped. A partial run says what it could not check instead.

A subprocess gate caps concurrency at 8. `MAX_CALLS` (40) bounds fan-out per
run. `max_budget_usd` caps one call; the CLI checks it between turns and can
overshoot by the cost of one turn, so the raised error reports actual spend.

### Confinement

`cwd` alone is not a boundary. A bare `Read` in the allowlist approves the whole
tool before any permission callback runs, so absolute paths walk straight out. A
`PreToolUse` hook runs first: `confine` denies `Read`/`Grep`/`Glob` on any path
resolving outside `cwd` plus `reads`.

Paths are checked under **both** orderings of symlink resolution and `..`
elimination, because the CLI applies `..` lexically while `canonicalize` follows
links first — checking only one lets the other through.

Web tools are not confined: an agent with `WebFetch` can still send what it read
under `cwd` to a URL.

### Environment

| Variable | Meaning |
|---|---|
| `RUSTFLOWS_CACHE` | cache dir, default `~/.cache/rustflows` |
| `RUSTFLOWS_RUN_LOG` | run log, default `~/.cache/rustflows-runs/run.jsonl` |
| `RUSTFLOWS_MODEL` | model to pass as `--model` |
| `RUSTFLOWS_CLAUDE` | path to the `claude` binary |
| `RUSTFLOWS_CONFINE` | explicit path to the hook binary |
| `RUSTFLOWS_BATCH` | findings per verifying agent, default 5 |

The run log is deliberately **not** inside the cache dir: `rm -rf
~/.cache/rustflows` forces a cold run and must not delete the record of what it
cost.

## Deliberate differences from pyflows

- **Every "how many" flag rejects 0 as a usage error (exit 2)** —
  `--lenses`, `--angles`, `--limit`, `--depth`, `--max`. pyflows relies on
  Python truthiness, so `--angles 0` silently means *all four angles* and
  `--limit 0` silently means *no limit*: the user asks for one thing and gets
  another. `--no-verify` already covers "verify nothing".
- **`--max` is `Option<i64>` and rejects anything under 2.** `usize` cannot hold
  the `-1` argparse accepts, which would then slice an option off the end.
- **Config errors exit 2, not 1.** A missing manifest is a configuration error,
  so it joins `unknown lens` rather than the run-failed code.
- **Presentation order seeds from SHA-256, not CRC-32.** The property that
  matters is only "stable, and derived from the option's own text so it is not
  the argument order". `sha2` is already a dependency; `crc32fast` would not be.
  The two projects have separate caches, so parity buys nothing.
- **`render` returns a `String`** rather than printing, so the ~15 assertions on
  CLI wording (`"safe to bump"`, `"NOT checked"`, `"fit the budget"`) are unit
  tests. `review.rs` still prints inline.
- **A judge verdict whose `winner` is unreadable counts as a dead verdict**, not
  as index 0. pyflows would raise `IndexError`; the enum in `PAIR` plus `ask`'s
  validation makes it unreachable, so the safe reading is the honest one.
- **`decide` deduplicates its options before spending a call**, not in the CLI.
  Same outcome, but it holds whatever the caller is.

- **No named-skill support.** The CLI has no `--skills` flag, so a skill name is
  rejected with an error rather than silently ignored. Grant skills through a
  plugin dir (`--plugin`) or user settings (`--inherit-settings`).
- **Cache is one JSON file per key**, not sqlite. Small, no native dependency,
  atomic via rename.
- **Confinement is part of the cache key.** An entry produced while the hook was
  missing must not be served to a confined run — otherwise a leak gets cached and
  replayed on the next run. This actually happened during the port.
- **Confinement is skipped silently if the hook binary is missing.** The settings
  document is simply not sent. Loud failure would be better; add it if it bites.