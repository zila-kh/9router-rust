# Changelog

## Unreleased — 2026-09-13

- Updated the pinned upstream dashboard and provider catalog from 9Router `0.5.69` to `0.5.75` (`17c4cc76877bd1755030a8414f8d0083f48dcccf`).
- Restored upstream `src/app/api` handlers as an internal compatibility layer instead of deleting them from the vendored frontend.
- Added `NINEROUTER_COMPAT_API=1`: Rust authenticates requests, prefers native handlers, and forwards only unported management routes to loopback Next.
- Added a shared `NINEROUTER_UI_SECRET` guard; direct requests to internal Next backend paths remain blocked.
- Added `x-9router-runtime: upstream-compat` for delegated responses.
- Kept `/v1`, `/v1beta`, `/responses`, and `/codex` Rust-owned.
- Updated development and production launchers to generate the shared token and enable compatibility mode; strict mode explicitly disables all fallback.
- Repaired frontend materialization and CI when the upstream checkout has no `package-lock.json`.
- Corrected release binary references from `9router-rust` to `nine-router-rs`.
- Added full-stack coverage for an upstream-only endpoint and direct internal API isolation.

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
