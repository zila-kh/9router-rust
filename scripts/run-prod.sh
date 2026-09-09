#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
command -v cargo >/dev/null || { echo 'error: cargo not found' >&2; exit 127; }
command -v npm >/dev/null || { echo 'error: npm not found' >&2; exit 127; }
export NINEROUTER_UI_ONLY=1
export NINEROUTER_UI_ORIGIN=${NINEROUTER_UI_ORIGIN:-http://127.0.0.1:20129}
export NINEROUTER_HOST=${NINEROUTER_HOST:-127.0.0.1}
export PORT=${PORT:-20128}
cleanup(){ jobs -p | xargs -r kill 2>/dev/null || true; }
trap cleanup EXIT INT TERM
npm --prefix frontend ci
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
cargo build --release --locked --manifest-path rust-backend/Cargo.toml
NINEROUTER_UI_ONLY=1 npm --prefix frontend run start:ui &
rust-backend/target/release/nine-router-rs
