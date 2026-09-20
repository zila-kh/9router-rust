---
name: upstream-port-2rust
description: Survey a new upstream 9Router release and port the changes worth having into this native Rust backend. Use whenever the user asks what changed upstream, wants to sync/update/bump the port, asks to "port more", wants the "New version available" banner dealt with, asks about upstream tickets or issues (cache, usage, streaming, translator, provider bugs), or wants an upstream fix carried into the Rust path — even if they never say the word "port".
---

# Porting upstream 9Router changes into the Rust backend

This repo is a Rust port of `decolua/9router`. The public listener is Rust and
LLM paths (`/v1/**`, `/v1beta/**`, `/codex`) are served natively, so an upstream
JS fix only reaches users here if its logic is re-implemented in Rust. The
vendored `frontend/` snapshot supplies only the dashboard and non-LLM management
routes.

Two rules decide most of the work:

1. **A fix users feel is a fix worth porting** — prompt-cache hit rate, billed
   cost, first-token latency, stream liveness, vision/fidelity correctness.
   Cosmetic and i18n changes arrive with a re-vendor, not a port.
2. **Check the transport before porting.** A provider whose catalog `format` is
   not one the gateway serves natively is rejected before the fixed code path
   exists, so its fixes are moot until that executor is ported.

## Workflow

### 1. Establish both ends of the diff

```bash
cat frontend-source.lock.json        # pinned upstream commit + reported version
cat rust-backend/UPSTREAM_COMMIT     # same pin, used by the build
```

Then run the bundled survey, which reads the pin, resolves the latest upstream
tag, and classifies every changed file:

```bash
node .agents/skills/upstream-port-2rust/scripts/upstream-diff.mjs
```

It prints the commit list, the changed files ordered by churn, and a
`rust-portable` / `data` / `spec` / `frontend-only` tag per file. Add `--json`
when you need to filter further.

### 2. Read commit subjects first, patches second

Commit subjects name the fix ("fix(stream): report aborts after HTTP 200
in-band", "fix(translator): keep tool-result images"). Triage all of them, then
fetch only the patches you intend to port:

```bash
gh api "repos/decolua/9router/commits/<sha>" \
  --jq '.files[] | "\(.additions)+\(.deletions)-\t\(.filename)"'

gh api "repos/decolua/9router/commits/<sha>" \
  --jq '.files[] | select(.filename=="open-sse/translator/request/claude-to-openai.js") | .patch'
```

Never dump the whole `compare` patch — a release is ~90 files and will flood the
context. Summarize with `--jq`, then fetch per file.

### 3. Read the upstream test as the specification

Upstream ships a test per fix. It states the intended behavior more precisely
than the patch does — including the assertions the fix must satisfy:

```bash
gh api "repos/decolua/9router/commits/<sha>" \
  --jq '.files[] | select(.filename|test("tests/")) | "=== \(.filename) ===\n\(.patch)"'
```

Port the behavior and the assertions, not the JS implementation.

### 4. Map each change onto its Rust module

`references/port-map.md` holds the JS→Rust table (translator → `translate.rs`,
stream helpers → `streaming.rs`/`responses_stream.rs`, account fallback →
`error.rs` + the connection loop in `gateway.rs`, provider executors →
`special/`, registry/pricing → `assets/provider-catalog.json` via
`scripts/export-rust-catalog.mjs`). Read it before searching by hand.

### 5. Port, then port the test

Match the surrounding code's density and idiom — parts of `translate.rs` are
dense single-line builders, but new code should be normally formatted, because
`cargo fmt --check` is a CI gate. Name the upstream commit in a comment only
where the reason for the code is not visible from the code itself (a status-code
rule, a protocol quirk, a billing multiplier).

### 6. Verify with the repo's own gates

```bash
cargo fmt --manifest-path rust-backend/Cargo.toml --all -- --check
cargo clippy --manifest-path rust-backend/Cargo.toml --all-targets
cargo test --manifest-path rust-backend/Cargo.toml --all-targets
cargo build --release --locked --manifest-path rust-backend/Cargo.toml
node scripts/update-manifest.mjs && node scripts/update-manifest.mjs --check
```

Clippy is currently clean (the `free_tier.rs` warnings were fixed), so `-D
warnings` in `full-stack-ci.yml` is a real gate — a new warning fails CI. Note
that other work may be happening in this tree at the same time: `git status`
before and after a port pass, and never revert a change you did not make.

`MANIFEST.sha256` covers every file `git ls-files` reports, including brand new
ones, so refresh it after *each* edit or `--check` fails. It also re-hashes any
concurrent edits, which is expected rather than a sign you broke something.

### 7. Restart the stack if the user is running it

`npm run dev` rebuilds the debug binary, so a running instance is stale until it
restarts. Find the tree by the `npm run dev` PID and kill it whole, then start
again and poll health:

```bash
taskkill //PID <npm-run-dev-pid> //T //F
npm run dev &                     # background
curl -s http://127.0.0.1:20130/api/health
```

### 8. Report ported vs deferred, per commit

Lead with what the user gains. For each ported item give the upstream sha and
the test that proves it; for each deferred item give the reason (transport not
served natively / data re-vendor / cosmetic / too large for this pass) rather
than leaving it unmentioned. State plainly which claims are unit-tested only and
which need a live benchmark with provider credentials.

## Triage shortcuts that paid off

- **Cost fixes are cache fixes.** Anthropic bills cached reads at 0.1x base
  input, writes at 1.25x (5m TTL) or 2x (1h). A request that never emits a
  breakpoint, or that rewrites its prefix every turn, re-bills the whole history
  at full price — check `cache_control`, prompt-prefix stability (random ids in
  rebuilt history), and whether cache tokens survive into the usage ledger.
- **Account churn is a cache fix too.** Provider caches are per account, so
  anything that rotates accounts on a request-shaped error (400/413/422) or
  without conversation affinity throws away a warm prefix.
- **Latency fixes are streaming fixes.** Compare the streaming paths: a
  same-format passthrough is progressive, but a cross-format translation that
  buffers the whole answer before answering adds seconds of first-token delay.
- **Data changes are not ports.** Registry/catalog/pricing entries belong to the
  exporter (`scripts/export-rust-catalog.mjs`), which CI re-runs when it vendors
  the frontend; extend the exporter instead of hand-editing the JSON.
- **The frontend pin is not a port concern.** Logic fixed for Rust must not be
  hand-edited into `frontend/`; that tree is materialized from upstream.
