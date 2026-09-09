#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
export NINEROUTER_UI_ONLY=1
export NINEROUTER_UI_ORIGIN=${NINEROUTER_UI_ORIGIN:-http://127.0.0.1:20129}
export PORT=20128
cleanup(){ jobs -p | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT INT TERM
npm run build
PORT=20129 NINEROUTER_UI_ONLY=1 ./node_modules/.bin/next start -H 127.0.0.1 -p 20129 &
cargo run --manifest-path rust-backend/Cargo.toml
