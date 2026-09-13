#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
command -v cargo >/dev/null || { echo 'error: cargo not found' >&2; exit 127; }
command -v npm >/dev/null || { echo 'error: npm not found' >&2; exit 127; }
command -v python3 >/dev/null || { echo 'error: python3 not found' >&2; exit 127; }

export NINEROUTER_UI_ONLY=1
export NINEROUTER_COMPAT_API=${NINEROUTER_COMPAT_API:-1}
export NINEROUTER_UI_ORIGIN=${NINEROUTER_UI_ORIGIN:-http://127.0.0.1:20129}
export NINEROUTER_HOST=${NINEROUTER_HOST:-127.0.0.1}
export PORT=${PORT:-20128}
export NINEROUTER_DISABLE_LEGACY_BRIDGE=1
unset NINEROUTER_LEGACY_BACKEND_ORIGIN LEGACY_BACKEND_ORIGIN

if [[ -z "${NINEROUTER_UI_SECRET:-}" ]]; then
  NINEROUTER_UI_SECRET="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
  export NINEROUTER_UI_SECRET
fi

bash scripts/materialize-frontend.sh frontend
if [[ -f frontend/package-lock.json ]]; then
  npm --prefix frontend ci
else
  npm --prefix frontend install --no-audit --no-fund
fi
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
cargo build --release --locked --manifest-path rust-backend/Cargo.toml

cleanup(){ jobs -p | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT INT TERM
NINEROUTER_UI_ONLY=1 npm --prefix frontend run start:ui &
rust-backend/target/release/9router-rust
