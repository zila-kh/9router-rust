#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
command -v cargo >/dev/null || { echo 'error: cargo not found' >&2; exit 127; }
command -v npm >/dev/null || { echo 'error: npm not found' >&2; exit 127; }
command -v node >/dev/null || { echo 'error: node not found' >&2; exit 127; }
python3 ./scripts/static-audit.py
cargo fmt --manifest-path rust-backend/Cargo.toml --all -- --check
cargo check --locked --all-targets --manifest-path rust-backend/Cargo.toml
cargo test --locked --all-targets --manifest-path rust-backend/Cargo.toml
cargo build --locked --manifest-path rust-backend/Cargo.toml
npm --prefix frontend ci
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
./scripts/smoke-e2e.sh .
NINEROUTER_ALLOW_PARITY_GAPS=1 node ./scripts/audit-parity.mjs
