# Changelog

## Free-tier boundary documented and verified — 2026-09-21

No behavior changed. The free tier's audience and reach are now stated where
operators and reviewers look for them, and its headline live leg was run.

- `docs/FREE_COMBO_PLAN.md` states the registration boundary explicitly: the API
  key is the whole of it (this port has no user account or signup system), the
  tier is never public, and an API key — including one an operator creates for
  themselves — is a consumer that sees only `combo-free`. It records why the
  shipped defaults already deny anonymous access, why the expose toggle cannot
  rescue a non-member connection, and why a private or loopback upstream cannot
  be contributed to the pool, leaving the `builtinFreeCombo` switch as the route
  to direct addressing.
- The plan's live acceptance is no longer wholly pending. On a fresh data
  directory with shipped defaults, unauthenticated requests to `/v1/models`,
  `/v1/models/info`, and `/v1/chat/completions` each answered `401`; a newly
  created key's `/v1/models` listed exactly `combo-free`; and
  `POST /v1/chat/completions` with `model: "combo-free"` returned a real
  completion served by an anonymous registry member. Switch-off filtering and
  the rejection of an unexposed model name were confirmed as well. The
  keyed-member leg still needs a real provider key.
- `README.md` gains a built-in free tier section. The default changes what an
  API key can reach, and the README did not mention the tier at all.

## Release gates restored — 2026-09-20

The release gates that were failing on `main` now pass, and the drift one of
them should have caught is fixed rather than silenced.

- The release manifest is platform-independent. `update-manifest.mjs` hashed
  raw working-tree bytes, and the 1251-file manifest that replaced the old
  hand-maintained list was generated from a Windows working tree, so 1045 of
  its entries could never match the LF content of a Linux CI checkout. The gate
  had not been reached on CI yet because the boundary regression below failed
  first. Hashing now normalizes line endings for text files and leaves content
  that is not valid UTF-8 byte-exact, so the same commit verifies on either
  platform.
- `cargo clippy --locked --all-targets -- -D warnings` passes again. Three
  errors in `free_tier.rs` were repaired: a needless re-borrow of the registry
  URL and two negated `is_some()` chains on the member-update body.
- The overlay source of `dashboardGuard.js` was out of sync with the copy
  vendored into the frontend: `ALWAYS_PROTECTED` listed `/api/free-tier` in the
  frontend but not in `scripts/frontend-overrides/`, which is what
  re-materialization copies back. Left alone, the next vendor pass would have
  silently demoted the free-tier admin namespace from always-protected to
  subject to the optional-login setting, because the guard prefix-matches that
  list. Both copies now agree, and the boundary suite asserts that
  `/api/free-tier`, its nested routes, and its percent-escaped form stay behind
  a session while login is optional.
- The release-boundary suite no longer compares the overlay and its vendored
  copy byte for byte. `.gitattributes` pins line endings for `*.sh` only, so a
  Windows checkout legitimately normalized the two files differently and failed
  a content assertion it had already satisfied. The comparison normalizes line
  endings and the assertion itself is unchanged.
- The frontend lint gate keeps `--max-warnings=0` and passes. The 201 warnings
  it reported were entirely the pinned snapshot's own debt — the generated
  provider registry's anonymous default exports, dashboard effect dependencies,
  and `<img>` usage. Rewriting them here would be discarded on the next vendor
  pass, so `@next/next/no-img-element`,
  `import/no-anonymous-default-export`, and `react-hooks/exhaustive-deps` are
  exempted, together with unused-disable reporting that the already-disabled
  React Compiler rules leave behind. Errors still fail the gate everywhere, and
  the zero-warning budget still applies to everything these three rules do not
  cover.
- The strict end-to-end smoke test now follows the built-in free tier. The tier
  ships enabled and deliberately shows API-key consumers only the single
  `combo-free` model (see `docs/FREE_COMBO_PLAN.md`), so the provider
  connection the test creates was unreachable with the key it creates —
  `/v1/chat/completions` answered 404 — and the test had been failing since the
  tier landed. It now asserts the shipped default, switches the tier off
  through the documented admin setting, and then exercises the direct provider
  path as before. No product behavior changed.

## Prompt-cache economics and stream liveness — 2026-09-20

Cost, following the Anthropic and OpenAI caching rules (cache reads bill at 0.1x
base input, writes at 1.25x/2x):

- Claude upstream requests now carry Anthropic cache breakpoints. The Rust
  gateway previously emitted none and dropped the ones clients sent, so every
  turn re-billed the whole re-sent history at full input price. Two anchors (the
  end of the tools+system head and the newest turn) stay inside the four-marker
  budget; `promptCacheTtl` selects `5m` (default), `1h` or `off`.
- Cache tokens are recorded and priced. Canonical usage is cache-inclusive for
  all four wire formats, cache reads bill at their own rate, and the rate tables
  travel with the provider catalog from `open-sse/providers/pricing.js`, so a
  native Rust request lands in the usage ledger with the cost the dashboard would
  compute. The streaming passthrough previously recorded nothing at all;
  `streaming::UsageSniffer` now reads usage out of the bytes as they pass.
- Gemini history rebuilds use stable, correlated tool-call ids instead of a fresh
  uuid per request, which changed the prompt prefix on every call (implicit
  caching could never hit) and broke function-response correlation.
- A request-shaped 4xx (400/405/409/413/415/422/431/451) no longer rotates the
  conversation to another account or cools a free-tier member: the failure says
  nothing about the credential, and switching abandons the account whose prompt
  cache the conversation was using.
- Images a tool returned inside a Claude `tool_result` reach the upstream again,
  as tagged user content, instead of being dropped by the translation; base64 is
  never dumped into the tool message.
- Cross-format streaming is incremental for Chat Completions callers of
  Responses-format providers: upstream events are translated as they arrive
  instead of after the whole answer, and a stream that aborts, stalls (200 s to
  the first chunk, 360 s between chunks, overridable with `STREAM_*_TIMEOUT_MS`)
  or ends without a terminal event now reports that in-band in the client's own
  format rather than closing silently or claiming a successful finish.

## Release hardening — 2026-09-19

- Removed the Tailscale card from the dashboard API Endpoint page in the vendored
  frontend (`EndpointPageClient.js`). Tunnel management is a declared native-parity
  gap in this port and the card surfaced an unusable Enable flow; the Tunnel row,
  security banners, and the `/api/tunnel/**` routes are unchanged.
- Removed the Donate button and modal from the dashboard header (`Header.js`);
  it pointed at the upstream project's donation links and has no role in this
  Rust port.
- Removed the 9Remote sidebar entry and promo modal from the dashboard
  navigation (`Sidebar.js`); it advertised an upstream companion product that
  this port does not ship or support.
- The dashboard Quota Tracker no longer polls quota for inactive connections
  (`ProviderLimits`); a seeded connection awaiting its free-tier key no longer
  surfaces provider auth failures. The per-card Refresh button remains the
  explicit path for inactive connections.

- Updated `rustls` to `0.23.45` to remediate RUSTSEC-2026-0285 / GHSA-2mjx-qc3c-rqvc.
- Bound guarded search and web-fetch connections to the DNS addresses that were
  validated, rejecting DNS failures and mixed public/private answers to close
  the validation-to-connection rebinding window.
- Aligned live Codex requests with the ChatGPT backend (`store: false`, no
  unsupported output-limit field) and preserved or generated the stable session
  identifier required by OpenCode Go.
- Added scheduled and change-triggered RustSec/npm production dependency audits
  plus weekly Dependabot update checks for Cargo, npm, and GitHub Actions.
- Changed the direct Rust binary's default bind address from all interfaces to
  loopback; public exposure now requires an explicit `NINEROUTER_HOST`.
- Removed the API-key-shaped default from the live combo test and documented
  that the vendored frontend-only Docker files are not a deployment path for
  this Rust port.
- Excluded all generated `.next*` trees from ESLint so release linting does not
  traverse multi-gigabyte build/check output, disabled React Compiler-only
  rules for this non-Compiler frontend, and made the default lint command an
  actionable error gate.
- Replaced the stale hand-maintained SHA-256 file with a reproducible manifest
  of tracked release files and made strict CI reject a stale manifest.

## Unreleased — 2026-09-13

- Updated the pinned upstream dashboard and provider catalog from 9Router `0.5.69` to `0.5.75` (`17c4cc76877bd1755030a8414f8d0083f48dcccf`).
- Restored upstream `src/app/api` handlers as an internal compatibility layer instead of deleting them from the vendored frontend.
- Added `NINEROUTER_COMPAT_API=1`: Rust remains the public listener, enforces dashboard authentication, and uses the pinned upstream handlers for exact management-API contracts while the native port is completed.
- Kept login, logout, session status, password reset, health, and native parity reporting Rust-owned in compatibility mode.
- Mirrored upstream public, protected, always-protected, and local-only route classes at the Rust boundary.
- Ported upstream `x-9r-cli-token` validation from the shared `machine-id` and `auth/cli-secret` files for native and compatibility management APIs.
- Added direct-loopback, browser-origin, and forwarded-peer checks for host-control operations so a reverse proxy hop cannot be mistaken for a local request.
- Added a shared `NINEROUTER_UI_SECRET` guard; direct requests to internal Next backend paths remain blocked.
- Added `x-9router-runtime: upstream-compat` for delegated responses.
- Kept core `/v1` chat, model, embeddings, audio, image, Responses, Claude, Gemini, and Codex paths Rust-owned.
- Restored upstream 0.5.75 video generation/status/download/cancel, search, and web-fetch endpoints through a selective secured compatibility path for `/v1/videos/**`, `/v1/search/**`, and `/v1/web/**`.
- Added upstream-compatible `/api/health` JSON, CORS headers, and `OPTIONS` behavior.
- Updated strict `/api/init` and `/api/version` metadata to report the pinned upstream `0.5.75` snapshot.
- Added a dedicated no-redirect proxy client so OIDC, SAML, login, and other browser redirects are returned intact with the original public host and protocol.
- Restored upstream server bootstrap only in compatibility mode, including outbound proxy initialization, OAuth refresh scheduling, model-catalog synchronization, tunnel services, MCP bridges, and related runtime integrations; strict mode remains inert.
- Unified `NINEROUTER_DATA_DIR` and upstream `DATA_DIR` so Rust and Next read the same database and runtime files, and aligned the Windows default data directory with upstream.
- Updated development and production launchers to generate the shared token and enable compatibility mode; strict mode explicitly disables all fallback.
- Repaired frontend materialization and CI when the upstream checkout has no `package-lock.json`.
- Declared and consistently launched the release binary as `9router-rust`.
- Added full-stack coverage for exact upstream management responses, protected and upstream-only endpoints, upstream CLI-token access, browser redirect passthrough, shared state, direct internal API isolation, and the upstream video route.
- Replaced the racy Rust-format auto-commit workflow with deterministic formatting, build, test, and Clippy verification.

Native parity is still tracked independently. Compatibility coverage is not counted as a Rust port in `rust-backend/parity/routes.json`.

## 1.0.1 — 2026-09-09

Backend-only Rust port test package pinned to 9Router `eb712ca821f0ba6bc41043fbd14494c5af5daba5` (`0.5.69`).

Highlights:

- Rust owns the public listener while the existing Next/React dashboard remains the UI.
- Existing SQLite schema/path are reused without a format migration.
- Native core routing, model/provider/account resolution, fallback, translation, management APIs, usage aggregation, embeddings, OpenAI-style media endpoints, Kiro foundation, and CommandCode foundation.
- Next middleware/instrumentation can be switched to UI-only mode.
- Hybrid bridge mode labels every backend response as Rust or legacy for migration testing.
- Native dashboard DB export/import and exact HS256 dashboard-session protected-header compatibility.
- Strict route/semantic parity audit and release gate.

This version is a testable migration build, not a claimed 100% strict-Rust parity release. `scripts/release-gate.sh` remains the source of truth and intentionally fails while declared gaps remain.
