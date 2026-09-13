#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


def replace_tail(path: str, marker: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text()
    index = text.find(marker)
    if index < 0:
        raise RuntimeError(f"{path}: marker not found: {marker!r}")
    file.write_text(text[:index] + replacement)


def overwrite_checked(path: str, required: str, content: str) -> None:
    file = Path(path)
    text = file.read_text()
    if required not in text:
        raise RuntimeError(f"{path}: required marker not found: {required!r}")
    file.write_text(content)


replace_once(
    "rust-backend/src/media.rs",
    "HeaderValue::from_str(&key).map_err(|error| {",
    "HeaderValue::from_str(key).map_err(|error| {",
)

replace_once(
    "rust-backend/src/config.rs",
    '''        let ui_only_header_secret = env::var("NINEROUTER_UI_SECRET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let legacy_backend_origin = if env_flag("NINEROUTER_DISABLE_LEGACY_BRIDGE", false)? {
''',
    '''        let compat_api_enabled = env_flag("NINEROUTER_COMPAT_API", false)?;
        let ui_only_header_secret = match env::var("NINEROUTER_UI_SECRET") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            Ok(_) => bail!("NINEROUTER_UI_SECRET must not be empty"),
            Err(env::VarError::NotPresent) if compat_api_enabled => {
                bail!("NINEROUTER_UI_SECRET is required when NINEROUTER_COMPAT_API is enabled")
            }
            Err(env::VarError::NotPresent) => uuid::Uuid::new_v4().to_string(),
            Err(error) => {
                return Err(error).context("NINEROUTER_UI_SECRET is not valid Unicode")
            }
        };
        let legacy_backend_origin = if env_flag("NINEROUTER_DISABLE_LEGACY_BRIDGE", false)? {
''',
)
replace_once(
    "rust-backend/src/config.rs",
    '''        let compat_api_enabled = env_flag("NINEROUTER_COMPAT_API", false)?;
        Ok(Self {
''',
    '''        Ok(Self {
''',
)

replace_once(
    "frontend/custom-server.js",
    '''const { pathToFileURL } = require("url");

const origCreate = http.createServer.bind(http);
''',
    '''const { pathToFileURL } = require("url");

if (process.env.NINEROUTER_UI_ONLY === "1") {
  const uiPort = process.env.NINEROUTER_UI_PORT || "20129";
  if (!/^\\d+$/.test(uiPort) || Number(uiPort) < 1 || Number(uiPort) > 65535) {
    throw new Error(`Invalid NINEROUTER_UI_PORT: ${uiPort}`);
  }
  // The compatibility listener is private by design. Never honor a public bind
  // address inherited from the Rust process or the user's shell.
  process.env.HOSTNAME = "127.0.0.1";
  process.env.PORT = uiPort;
}

const origCreate = http.createServer.bind(http);
''',
)
replace_once(
    "frontend/package.json",
    '"start:ui": "next start --hostname 127.0.0.1 --port 20129"',
    '"start:ui": "node .next/standalone/custom-server.js"',
)

run_prod = r'''#!/usr/bin/env bash
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
  echo "$component exited with status $status" >&2
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
wait_ready "Rust backend" "http://${NINEROUTER_HOST}:${PORT}/api/health" "$rust_pid" 90

printf '9Router is ready at http://%s:%s/dashboard\n' "$NINEROUTER_HOST" "$PORT"
monitor_stack
'''
overwrite_checked("scripts/run-prod.sh", "npm --prefix frontend run start:ui", run_prod)

run_dev = run_prod.replace(
    '''if [[ -f frontend/package-lock.json ]]; then
  npm --prefix frontend ci
else
  npm --prefix frontend install --no-audit --no-fund
fi
NINEROUTER_UI_ONLY=1 npm --prefix frontend run build
cargo build --release --locked --manifest-path rust-backend/Cargo.toml
''',
    '''if [[ ! -d frontend/node_modules ]]; then
  if [[ -f frontend/package-lock.json ]]; then
    npm --prefix frontend ci
  else
    npm --prefix frontend install --no-audit --no-fund
  fi
fi
cargo build --locked --manifest-path rust-backend/Cargo.toml
''',
).replace(
    '''(
  cd frontend
  exec node .next/standalone/custom-server.js
) &
ui_pid=$!
wait_ready "internal Next UI" "$NINEROUTER_UI_ORIGIN/login" "$ui_pid" 90

rust-backend/target/release/9router-rust &
''',
    '''(
  cd frontend
  exec node node_modules/next/dist/bin/next dev --webpack --hostname 127.0.0.1 --port "$UI_PORT"
) &
ui_pid=$!
wait_ready "internal Next UI" "$NINEROUTER_UI_ORIGIN/login" "$ui_pid" 90

rust-backend/target/debug/9router-rust &
''',
).replace(
    "printf '9Router is ready at http://%s:%s/dashboard\\n' \"$NINEROUTER_HOST\" \"$PORT\"\n",
    "printf '9Router development stack is ready at http://%s:%s/dashboard\\n' \"$NINEROUTER_HOST\" \"$PORT\"\n",
)
overwrite_checked("scripts/run-dev.sh", "npm --prefix frontend run dev:ui", run_dev)

strict = r'''#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIR="${NINEROUTER_FRONTEND_DIR:-$ROOT/frontend}"
PUBLIC_PORT="${PORT:-20128}"
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
  echo "$component exited with status $status" >&2
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
'''
overwrite_checked("scripts/run-full-stack-strict.sh", 'exec npm run start:ui', strict)

replace_once(
    "scripts/smoke-e2e.sh",
    'NINEROUTER_UI_ONLY=1 npm --prefix frontend run start:ui >"$UI_LOG" 2>&1 &',
    '''(
  cd frontend
  exec env NINEROUTER_UI_ONLY=1 node .next/standalone/custom-server.js
) >"$UI_LOG" 2>&1 &''',
)
replace_once(
    ".github/workflows/full-stack-ci.yml",
    '''          (cd frontend && npm run start:ui >"${RUNNER_TEMP}/ui.log" 2>&1 & echo $! >"${RUNNER_TEMP}/ui.pid")
''',
    '''          (
            cd frontend
            exec node .next/standalone/custom-server.js
          ) >"${RUNNER_TEMP}/ui.log" 2>&1 &
          echo $! >"${RUNNER_TEMP}/ui.pid"
''',
)

replace_tail(
    "rust-backend/src/providers.rs",
    "#[cfg(test)]\nmod tests {",
    r'''#[cfg(test)]
mod tests {
    use super::{resolve_env_placeholders_with, resolve_model};
    use crate::{
        config::Config, db::Db, error::AppError, state::AppState,
    };
    use serde_json::json;

    fn test_state() -> (tempfile::TempDir, AppState) {
        let temp = tempfile::tempdir().expect("temporary directory");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("create data directory");
        let db_path = data_dir.join("data.sqlite");
        let db = Db::open(&db_path).expect("open test database");
        let config = Config {
            listen: "127.0.0.1:0".parse().expect("test socket address"),
            ui_origin: "http://127.0.0.1:20129".into(),
            data_dir,
            db_path,
            upstream_timeout_secs: 5,
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = AppState::new(config, db).expect("test application state");
        (temp, state)
    }

    #[test]
    fn resolves_nested_environment_placeholders() {
        let mut value = json!({
            "clientId": "env:CLIENT_ID",
            "nested": ["keep", {"secret": "env:CLIENT_SECRET"}],
            "number": 7
        });
        resolve_env_placeholders_with(&mut value, &|name| match name {
            "CLIENT_ID" => Some("client-value".into()),
            "CLIENT_SECRET" => Some("secret-value".into()),
            _ => None,
        });
        assert_eq!(value["clientId"], "client-value");
        assert_eq!(value["nested"][0], "keep");
        assert_eq!(value["nested"][1]["secret"], "secret-value");
        assert_eq!(value["number"], 7);
    }

    #[test]
    fn missing_environment_placeholder_becomes_empty() {
        let mut value = json!({"token": "env:MISSING_TOKEN"});
        resolve_env_placeholders_with(&mut value, &|_| None);
        assert_eq!(value["token"], "");
    }

    #[test]
    fn model_alias_cycles_return_an_error_without_recursing() {
        let (_temp, state) = test_state();
        state
            .db
            .kv_set("modelAliases", "alias-a", &json!("alias-b"))
            .expect("store first alias");
        state
            .db
            .kv_set("modelAliases", "alias-b", &json!("alias-a"))
            .expect("store second alias");

        match resolve_model(&state, "alias-a") {
            Err(AppError::BadRequest(message)) => assert!(message.contains("cyclic")),
            other => panic!("expected cyclic alias error, got {other:?}"),
        }
    }

    #[test]
    fn model_aliases_support_current_and_legacy_orientation() {
        for (key, value, requested) in [
            ("friendly", "openai/model-x", "friendly"),
            ("openai/model-x", "legacy-friendly", "legacy-friendly"),
        ] {
            let (_temp, state) = test_state();
            state
                .db
                .create_connection(json!({"provider":"openai","apiKey":"test-key"}))
                .expect("create provider connection");
            state
                .db
                .kv_set("modelAliases", key, &json!(value))
                .expect("store model alias");

            let resolved = resolve_model(&state, requested).expect("resolve model alias");
            assert_eq!(resolved.requested, requested);
            assert_eq!(resolved.provider, "openai");
            assert_eq!(resolved.model, "model-x");
        }
    }
}
''',
)

with Path("rust-backend/src/db.rs").open("a") as file:
    file.write(r'''

#[cfg(test)]
mod tests {
    use super::{mask_key, Db};
    use serde_json::json;

    #[test]
    fn masks_unicode_api_keys_without_byte_slicing() {
        assert_eq!(mask_key(Some("密钥密钥密钥密钥密钥")), Some("密钥密钥密钥密钥***".into()));
        assert_eq!(mask_key(Some("é")), Some("é***".into()));
    }

    #[test]
    fn combo_upsert_returns_the_persisted_identifier() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let db = Db::open(temp.path().join("data.sqlite")).expect("open test database");
        let first = db
            .upsert_combo(json!({"name":"stable","models":["one"]}))
            .expect("insert combo");
        let second = db
            .upsert_combo(json!({"name":"stable","models":["two"]}))
            .expect("update combo");

        assert_eq!(first["id"], second["id"]);
        assert_eq!(second["models"], json!(["two"]));
    }
}
''')

print("second release hardening pass applied")
