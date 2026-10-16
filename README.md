# rustflows

Agent orchestration in Rust. Part of a program is ordinary code; some
expressions are agent calls. The code decides what to call, in what order, and
when to stop. The agent decides the answer.

A port of [pyflows](../pyflows): the framework (`orch.py` + `panel.py`) plus the
`review` workflow. `research`, `upgrade` and `decide` are not ported.

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