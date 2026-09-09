#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

request() {
  local name="$1"; shift
  curl --silent --show-error --fail-with-body \
    --dump-header "$TMP/$name.headers" \
    --output "$TMP/$name.body" "$@"
}

request health "$BASE/api/health"
grep -Eq '"runtime"[[:space:]]*:[[:space:]]*"rust"' "$TMP/health.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/health.headers"

request login_page "$BASE/login"
grep -Eqi '<!doctype html|<html' "$TMP/login_page.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/login_page.headers"

# Authenticate through Rust. CI calls from loopback, where the pinned application
# deliberately permits the initial password until it is changed.
curl --silent --show-error --fail-with-body \
  --cookie-jar "$TMP/cookies" \
  --header 'content-type: application/json' \
  --data '{"password":"123456"}' \
  --dump-header "$TMP/login.headers" \
  --output "$TMP/login.body" \
  "$BASE/api/auth/login"
grep -Eq '"success"[[:space:]]*:[[:space:]]*true' "$TMP/login.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/login.headers"

request auth_status --cookie "$TMP/cookies" "$BASE/api/auth/status"
grep -Eq '"authenticated"[[:space:]]*:[[:space:]]*true' "$TMP/auth_status.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/auth_status.headers"

request settings --cookie "$TMP/cookies" "$BASE/api/settings"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/settings.headers"

request providers --cookie "$TMP/cookies" "$BASE/api/providers"
grep -Eq '"connections"[[:space:]]*:' "$TMP/providers.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/providers.headers"

request dashboard --cookie "$TMP/cookies" "$BASE/dashboard"
grep -Eqi '<!doctype html|<html' "$TMP/dashboard.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/dashboard.headers"

# Verify an actual Next asset traverses the Rust UI proxy when one is referenced.
asset="$(grep -Eo '/_next/static/[^"[:space:]]+' "$TMP/dashboard.body" | head -n1 || true)"
if [[ -n "$asset" ]]; then
  request next_asset "$BASE$asset"
  [[ -s "$TMP/next_asset.body" ]]
fi

printf 'Strict full-stack smoke test passed against %s\n' "$BASE"
