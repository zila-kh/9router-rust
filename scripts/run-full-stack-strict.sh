#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="${NINEROUTER_FRONTEND_DIR:-$ROOT/frontend}"
PUBLIC_PORT="${PORT:-20128}"
UI_PORT="${NINEROUTER_UI_PORT:-20129}"
UI_HOST="127.0.0.1"
RUST_BIN="${NINEROUTER_RUST_BIN:-$ROOT/rust-backend/target/release/9router-rust}"

export NINEROUTER_UI_ONLY=1
export NINEROUTER_COMPAT_API=0
export NINEROUTER_UI_ORIGIN="http://${UI_HOST}:${UI_PORT}"
export NINEROUTER_DISABLE_LEGACY_BRIDGE=1
export PORT="$PUBLIC_PORT"
unset NINEROUTER_LEGACY_BACKEND_ORIGIN LEGACY_BACKEND_ORIGIN NINEROUTER_UI_SECRET

if [[ -n "${NINEROUTER_DATA_DIR:-}" && -n "${DATA_DIR:-}" && "$NINEROUTER_DATA_DIR" != "$DATA_DIR" ]]; then
  echo 'error: NINEROUTER_DATA_DIR and DATA_DIR must point to the same directory' >&2
  exit 2
fi
if [[ -n "${NINEROUTER_DATA_DIR:-}" ]]; then
  export DATA_DIR="$NINEROUTER_DATA_DIR"
elif [[ -n "${DATA_DIR:-}" ]]; then
  export NINEROUTER_DATA_DIR="$DATA_DIR"
fi

bash "$ROOT/scripts/materialize-frontend.sh" "$FRONTEND_DIR"

if [[ ! -d "$FRONTEND_DIR/.next" ]]; then
  (
    cd "$FRONTEND_DIR"
    if [[ -f package-lock.json ]]; then
      npm ci
    else
      npm install --no-audit --no-fund
    fi
    NEXT_TELEMETRY_DISABLED=1 NINEROUTER_UI_ONLY=1 npm run build
  )
fi

if [[ ! -x "$RUST_BIN" ]]; then
  cargo build --release --locked --manifest-path "$ROOT/rust-backend/Cargo.toml"
fi

TMP="${TMPDIR:-/tmp}/9router-rust-$PUBLIC_PORT"
mkdir -p "$TMP"
UI_PID=""
RUST_PID=""
cleanup() {
  set +e
  [[ -n "$RUST_PID" ]] && kill "$RUST_PID" 2>/dev/null
  [[ -n "$UI_PID" ]] && kill "$UI_PID" 2>/dev/null
  wait "$RUST_PID" 2>/dev/null
  wait "$UI_PID" 2>/dev/null
}
trap cleanup EXIT INT TERM

(
  cd "$FRONTEND_DIR"
  exec npm run start:ui
) >"$TMP/frontend.log" 2>&1 &
UI_PID=$!

"$RUST_BIN" >"$TMP/rust.log" 2>&1 &
RUST_PID=$!

bash "$ROOT/scripts/wait-http.sh" "http://${UI_HOST}:${UI_PORT}/login" 90
bash "$ROOT/scripts/wait-http.sh" "http://127.0.0.1:${PUBLIC_PORT}/api/health" 90

echo "9Router strict full stack is ready"
echo "Dashboard: http://127.0.0.1:${PUBLIC_PORT}/dashboard"
echo "API:       http://127.0.0.1:${PUBLIC_PORT}/v1"
echo "Rust log:  $TMP/rust.log"
echo "UI log:    $TMP/frontend.log"

wait -n "$UI_PID" "$RUST_PID"
status=$?
echo "A 9Router process exited with status $status" >&2
exit "$status"
