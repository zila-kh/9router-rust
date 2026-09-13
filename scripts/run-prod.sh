#!/usr/bin/env bash
set -Eeuo pipefail
ROOT=${1:-.}
cd "$ROOT"
for command in cargo npm node python3 curl; do
  command -v "$command" >/dev/null || { echo "error: $command not found" >&2; exit 127; }
done

UI_PORT=${NINEROUTER_UI_PORT:-20129}
if [[ ! "$UI_PORT" =~ ^[0-9]+$ ]] || (( 10#$UI_PORT < 1 || 10#$UI_PORT > 65535 )); then
  echo "error: invalid NINEROUTER_UI_PORT: $UI_PORT" >&2
  exit 2
fi
EXPECTED_UI_ORIGIN="http://127.0.0.1:${UI_PORT}"
if [[ -n "${NINEROUTER_UI_ORIGIN:-}" && "$NINEROUTER_UI_ORIGIN" != "$EXPECTED_UI_ORIGIN" ]]; then
  echo "error: NINEROUTER_UI_ORIGIN must be $EXPECTED_UI_ORIGIN for the private UI listener" >&2
  exit 2
fi

export NINEROUTER_UI_ONLY=1
export NINEROUTER_COMPAT_API=${NINEROUTER_COMPAT_API:-1}
export NINEROUTER_UI_PORT="$UI_PORT"
export NINEROUTER_UI_ORIGIN="$EXPECTED_UI_ORIGIN"
export NINEROUTER_HOST=${NINEROUTER_HOST:-127.0.0.1}
export PORT=${PORT:-20128}
export NINEROUTER_DISABLE_LEGACY_BRIDGE=1
export NEXT_TELEMETRY_DISABLED=${NEXT_TELEMETRY_DISABLED:-1}
unset NINEROUTER_LEGACY_BACKEND_ORIGIN LEGACY_BACKEND_ORIGIN

case "$NINEROUTER_HOST" in
  0.0.0.0) PUBLIC_READY_HOST=127.0.0.1 ;;
  ::) PUBLIC_READY_HOST='[::1]' ;;
  *:*) PUBLIC_READY_HOST="[$NINEROUTER_HOST]" ;;
  *) PUBLIC_READY_HOST="$NINEROUTER_HOST" ;;
esac
PUBLIC_BASE_URL="http://${PUBLIC_READY_HOST}:${PORT}"

if [[ -n "${NINEROUTER_DATA_DIR:-}" && -n "${DATA_DIR:-}" && "$NINEROUTER_DATA_DIR" != "$DATA_DIR" ]]; then
  echo 'error: NINEROUTER_DATA_DIR and DATA_DIR must point to the same directory' >&2
  exit 2
fi
if [[ -n "${NINEROUTER_DATA_DIR:-}" ]]; then
  export DATA_DIR="$NINEROUTER_DATA_DIR"
elif [[ -n "${DATA_DIR:-}" ]]; then
  export NINEROUTER_DATA_DIR="$DATA_DIR"
fi

if [[ -z "${NINEROUTER_UI_SECRET:-}" || "$NINEROUTER_UI_SECRET" =~ ^[[:space:]]*$ ]]; then
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

ui_pid=""
rust_pid=""
cleanup() {
  status=$?
  trap - EXIT INT TERM
  set +e
  [[ -n "$rust_pid" ]] && kill "$rust_pid" 2>/dev/null
  [[ -n "$ui_pid" ]] && kill "$ui_pid" 2>/dev/null
  [[ -n "$rust_pid" ]] && wait "$rust_pid" 2>/dev/null
  [[ -n "$ui_pid" ]] && wait "$ui_pid" 2>/dev/null
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
  while kill -0 "$ui_pid" 2>/dev/null && kill -0 "$rust_pid" 2>/dev/null; do
    sleep 1
  done
  set +e
  if ! kill -0 "$ui_pid" 2>/dev/null; then
    wait "$ui_pid"
    status=$?
    component="internal Next UI"
  else
    wait "$rust_pid"
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
  cd frontend
  exec node .next/standalone/custom-server.js
) &
ui_pid=$!
wait_ready "internal Next UI" "$NINEROUTER_UI_ORIGIN/login" "$ui_pid" 90

rust-backend/target/release/9router-rust &
rust_pid=$!
wait_ready "Rust backend" "$PUBLIC_BASE_URL/api/health" "$rust_pid" 90

printf '9Router is ready at %s/dashboard\n' "$PUBLIC_BASE_URL"
monitor_stack
