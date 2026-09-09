#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
RUST_PORT=${RUST_PORT:-20128}
LEGACY_PORT=${LEGACY_PORT:-20130}
LEGACY_ORIGIN="http://127.0.0.1:${LEGACY_PORT}"
export NINEROUTER_UI_ORIGIN="$LEGACY_ORIGIN"
export NINEROUTER_LEGACY_BACKEND_ORIGIN="$LEGACY_ORIGIN"
export PORT="$RUST_PORT"
cleanup(){ jobs -p | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Frozen upstream server remains complete during the migration. Rust owns the public port.
# Bind the fallback server to loopback so it cannot bypass Rust auth/security.
npm run build
NINEROUTER_UI_ONLY=0 node custom-server.js -H 127.0.0.1 --port "$LEGACY_PORT" &
cargo run --manifest-path rust-backend/Cargo.toml
