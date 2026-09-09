# Changelog

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
