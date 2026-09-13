# Changelog

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
