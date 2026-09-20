# Port map: upstream JS → this Rust backend

Read this before searching the tree by hand. Paths on the left are upstream
(`decolua/9router`); paths on the right are this repo.

## Module map

| Upstream JS | Rust here | Notes |
| --- | --- | --- |
| `open-sse/translator/request/*` (claude-to-openai, openai-to-claude, openai-to-gemini, responses, kiro, commandcode) | `rust-backend/src/translate.rs` | `claude_to_openai_request` / `openai_to_claude_request` are the round trip every request takes; `normalize_request` (caller → canonical) and `provider_request` (canonical → provider) |
| `open-sse/translator/formats/claude.js` | `translate.rs` (`apply_claude_cache_breakpoints`, `openai_to_claude_request`) | cache breakpoints, 4-marker budget, `defer_loading` interactions |
| `open-sse/translator/concerns/*` (usage, thinking, toolCall, modality) | `translate.rs` (`canonical_usage_from_native`, `stored_tokens`, tool ids) | usage folds cache into `prompt_tokens`; `usageTracking.js` is the source of that convention |
| `open-sse/translator/response/*` | `translate.rs` (`*_to_*_response`), `streaming.rs` (`reduce_*`, `synthesize`) | response direction, including SSE reduction for buffered streams |
| `open-sse/utils/streamHelpers.js`, `responsesStreamHelpers.js`, `stream.js` | `rust-backend/src/streaming.rs` (`abort_terminal_frames`, `reduce_stream`, `synthesize`), `rust-backend/src/responses_stream.rs` | in-band terminal frames, SSE decoding, incremental cross-format encoding |
| `open-sse/config/runtimeConfig.js` (timeouts) | `rust-backend/src/config.rs` | `STREAM_FIRST_CHUNK_TIMEOUT_MS` / `STREAM_STALL_TIMEOUT_MS`, also accepted with a `NINEROUTER_` prefix |
| `open-sse/services/accountFallback.js`, `src/sse/services/auth.js` | `rust-backend/src/error.rs` (`request_scoped`, `auto_route_retryable`) + the connection loop in `gateway.rs::execute_target` | account rotation, cooldowns, model locks |
| `open-sse/providers/pricing.js`, `capabilities.js`, `registry/*`, `index.js` | `rust-backend/assets/provider-catalog.json` | generated: `node scripts/export-rust-catalog.mjs` (CI `.github/workflows/vendor-frontend.yml` re-runs it when vendoring). `pricing.rs` reads the exported tables |
| `open-sse/providers/visionPatterns.js` | `rust-backend/src/model_capabilities.rs` | capability heuristics, `match_pattern` globs shared with pricing |
| `open-sse/executors/<provider>.js` | `rust-backend/src/special/` (`kiro.rs`, `commandcode.rs`), `gateway.rs` (`apply_provider_body_requirements`, transport table) | only `openai`, `claude`, `gemini`, `openai-responses`/`responses`, `kiro`, `commandcode` transports are served natively — see `auto_router::rust_gateway_supports_provider` |
| `open-sse/handlers/chatCore/*`, `handlers/chat.js` | `rust-backend/src/gateway.rs` | request lifecycle, usage recording (`record_usage`), streaming branch selection |
| `src/lib/db/repos/usageRepo.js`, `usageDb.js` | `rust-backend/src/db.rs` (`usage_record_tokens`), `usage_mgmt.rs` | ledger shape: `tokens` JSON carries `cached_tokens` / `cache_creation_input_tokens` |
| `src/app/**`, `src/shared/components/**`, `public/i18n/**` | — | frontend only; arrives via re-vendor |

## Triage: is it portable?

Check the transport first. Providers whose catalog `format` is outside the
natively served list (`antigravity`, `cursor`, `windsurf`, …) never reach the
Rust code path, so their executor fixes are moot until that executor is ported:

```bash
node -e "const c=require('./rust-backend/assets/provider-catalog.json');
for (const id of ['antigravity','cerebras','opencode-go','zed']) console.log(id, c.providers[id]?.format)"
```

Then classify:

- **rust-portable** — translator, streaming, usage/cost, auth/fallback,
  capability, gateway logic (the paths above).
- **data** — registry, model catalog, pricing entries: extend
  `scripts/export-rust-catalog.mjs` or refresh the asset; never hand-port entry
  by entry.
- **spec** — `tests/**`: read as the specification for a port, and port the
  assertions into Rust tests.
- **frontend-only** — dashboard, i18n, badges, sidebar, OAuth modals: arrives
  with a re-vendor.

## Priority order for a port pass

1. Cache hit rate (breakpoints, prompt-prefix stability, cache-token accounting).
2. Billed cost (pricing corrections, cache-rate billing, avoiding wasted retries).
3. First-token latency and stream liveness (progressive streaming, stall guards,
   in-band terminal frames).
4. Fidelity that changes what the model sees (tool-result images, thinking
   blocks, tool-name round trips, message ordering).
5. Everything else, explicitly deferred with a reason.

## Upstream tickets

Code comments reference upstream issue numbers (`#3567`, `#3905`). Search the
tracker for context and to see whether a change you are considering is already
tracked or was closed as intentional:

```bash
gh api "search/issues?q=repo:decolua/9router+cache+in:title&per_page=40" \
  --jq '.items[] | "\(.number)\t\(.state)\t\(.title)"'
```

## Traps this port has already hit

- **rustfmt rewrites anchors.** If you patch with a script, match the *formatted*
  text, or run `cargo fmt` first and re-read the region. Two failed patches in
  one session came from stale anchors.
- **Private helpers are untestable across modules.** Put a helper where its test
  lives, or make it `pub(crate)`; `super::super::module::private_fn` does not
  compile.
- **`serde_json::Map` sorts keys**, so serialized order is alphabetical — never
  infer provenance from key order in a frame.
- **`finish_reason` exists but is null** on every non-terminal streamed chunk;
  `find_map` stops on the null, so use `filter_map(...).and_then(Value::as_str)`.
- **Frame builders must not be re-parsed.** A complete SSE frame starts with
  `data: `, so `serde_json::from_slice` on it fails; build the JSON value,
  attach usage, then frame once.
- **`Config` has many test literals.** Adding a field breaks every
  `Config { upstream_timeout_secs: N, .. }` in test modules; patch them all
  (`rust-backend/src/{app,auth,free_tier,gateway,models_list,providers,videos_api}.rs`).
- **`MANIFEST.sha256` tracks untracked-but-not-ignored files** (`git ls-files
  --others`), so a brand-new skill or doc changes it; it also re-hashes
  pre-existing drift in files you never touched.
- **Streaming cost accounting has three paths** — same-format passthrough
  (`UsageSniffer`), cross-format buffered (`reduce_stream`), incremental
  (`responses_stream`). A change to one is not a change to the others; check
  where usage is recorded in each.
