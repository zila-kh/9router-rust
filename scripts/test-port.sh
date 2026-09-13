#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
command -v cargo >/dev/null || { echo 'error: cargo not found' >&2; exit 127; }
command -v npm >/dev/null || { echo 'error: npm not found' >&2; exit 127; }
command -v node >/dev/null || { echo 'error: node not found' >&2; exit 127; }
python3 ./scripts/static-audit.py
bash -n ./scripts/run-dev.sh ./scripts/run-prod.sh ./scripts/materialize-frontend.sh
if PORT=0 bash ./scripts/run-prod.sh . >/dev/null 2>&1; then
  echo 'error: production launcher accepted PORT=0' >&2
  exit 1
fi
if PORT=20129 NINEROUTER_UI_PORT=20129 bash ./scripts/run-dev.sh . >/dev/null 2>&1; then
  echo 'error: development launcher accepted colliding ports' >&2
  exit 1
fi
bash ./scripts/test-frontend-materialization.sh
cargo fmt --manifest-path rust-backend/Cargo.toml --all -- --check
cargo check --locked --all-targets --manifest-path rust-backend/Cargo.toml
cargo test --locked --all-targets --manifest-path rust-backend/Cargo.toml
cargo build --locked --manifest-path rust-backend/Cargo.toml
npm --prefix frontend ci
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
bash ./scripts/smoke-e2e.sh .
NINEROUTER_ALLOW_PARITY_GAPS=1 node ./scripts/audit-parity.mjs
