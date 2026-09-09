#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
command -v cargo >/dev/null || { echo 'error: cargo not found' >&2; exit 127; }
command -v node >/dev/null || { echo 'error: node not found' >&2; exit 127; }
python3 ./scripts/static-audit.py
cargo fmt --manifest-path rust-backend/Cargo.toml -- --check
cargo check --all-targets --manifest-path rust-backend/Cargo.toml
cargo test --all-targets --manifest-path rust-backend/Cargo.toml
NINEROUTER_ALLOW_PARITY_GAPS=1 node ./scripts/audit-parity.mjs
