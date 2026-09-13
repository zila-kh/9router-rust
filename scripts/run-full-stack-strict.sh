#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="${NINEROUTER_FRONTEND_DIR:-$ROOT/frontend}"
PUBLIC_PORT="${PORT:-20130}"
UI_PORT="${NINEROUTER_UI_PORT:-20129}"
UI_HOST="127.0.0.1"
RUST_BIN="${NINEROUTER_RUST_BIN:-$ROOT/rust-backend/target/release/9router-rust}"
for command in cargo npm node curl; do
  command -v "$command" >/dev/null || { echo "error: $command not found" >&2; exit 127; }
done
if [[ ! "$UI_PORT" =~ ^[0-9]+$ ]] || (( 10#$UI_PORT < 1 || 10#$UI_PORT > 65535 )); then
  echo "error: invalid NINEROUTER_UI_PORT: $UI_PORT" >&2
  exit 2
fi

export NINEROUTER_UI_ONLY=1
export NINEROUTER_COMPAT_API=0
export NINEROUTER_UI_PORT="$UI_PORT"
export NINEROUTER_UI_ORIGIN="http://${UI_HOST}:${UI_PORT}"
export NINEROUTER_DISABLE_LEGACY_BRIDGE=1
export NEXT_TELEMETRY_DISABLED=${NEXT_TELEMETRY_DISABLED:-1}
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
if [[ ! -f "$FRONTEND_DIR/.next/standalone/custom-server.js" ]]; then
  (
    cd "$FRONTEND_DIR"
    if [[ -f package-lock.json ]]; then
      npm ci
    else
      npm install --no-audit --no-fund
    fi
    NINEROUTER_UI_ONLY=1 npm run build
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
  status=$?
  trap - EXIT INT TERM
  set +e
  [[ -n "$RUST_PID" ]] && kill "$RUST_PID" 2>/dev/null
  [[ -n "$UI_PID" ]] && kill "$UI_PID" 2>/dev/null
  [[ -n "$RUST_PID" ]] && wait "$RUST_PID" 2>/dev/null
  [[ -n "$UI_PID" ]] && wait "$UI_PID" 2>/dev/null
  return "$status"
}
on_signal() {
  trap - INT TERM
  exit "$1"
}
wait_ready() {
  name=$1
  url=$2
  pid=$3
  timeout=$4
  deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    if curl --fail --silent --show-error --max-time 2 "$url" >/dev/null 2>&1; then
      printf 'Ready: %s (%s)\n' "$name" "$url"
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      set +e
      wait "$pid"
      code=$?
      set -e
      echo "error: $name exited before becoming ready (status $code)" >&2
      return 1
    fi
    sleep 1
  done
  echo "error: timed out waiting for $name at $url after ${timeout}s" >&2
  return 1
}
monitor_stack() {
  while kill -0 "$UI_PID" 2>/dev/null && kill -0 "$RUST_PID" 2>/dev/null; do
    sleep 1
  done
  set +e
  if ! kill -0 "$UI_PID" 2>/dev/null; then
    wait "$UI_PID"
    status=$?
    component="internal Next UI"
  else
    wait "$RUST_PID"
    status=$?
    component="Rust backend"
  fi
  set -e
  if [[ "$status" == 0 ]]; then status=1; fi
  echo "$component exited unexpectedly with status $status" >&2
  return "$status"
}
trap cleanup EXIT
trap 'on_signal 130' INT
trap 'on_signal 143' TERM

(
  cd "$FRONTEND_DIR"
  exec node .next/standalone/custom-server.js
) >"$TMP/frontend.log" 2>&1 &
UI_PID=$!
wait_ready "internal Next UI" "http://${UI_HOST}:${UI_PORT}/login" "$UI_PID" 90

"$RUST_BIN" >"$TMP/rust.log" 2>&1 &
RUST_PID=$!
wait_ready "Rust backend" "http://127.0.0.1:${PUBLIC_PORT}/api/health" "$RUST_PID" 90

echo "9Router strict full stack is ready"
echo "Dashboard: http://127.0.0.1:${PUBLIC_PORT}/dashboard"
echo "API:       http://127.0.0.1:${PUBLIC_PORT}/v1"
echo "Rust log:  $TMP/rust.log"
echo "UI log:    $TMP/frontend.log"
monitor_stack
