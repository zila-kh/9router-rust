#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
PASSWORD="${NINEROUTER_TEST_PASSWORD:-123456}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

assert_native_headers() {
  local file="$1"
  if grep -Eqi '^x-9router-runtime:[[:space:]]*(legacy-bridge|upstream-compat)' "$file"; then
    echo "A fallback runtime served a strict-mode request" >&2
    cat "$file" >&2
    exit 1
  fi
}

request() {
  local name="$1"; shift
  curl --silent --show-error --fail-with-body \
    --dump-header "$TMP/$name.headers" \
    --output "$TMP/$name.body" "$@"
  assert_native_headers "$TMP/$name.headers"
}

request health "$BASE/api/health"
grep -Eq '"runtime"[[:space:]]*:[[:space:]]*"rust"' "$TMP/health.body"

request login_page "$BASE/login"
grep -Eqi '<!doctype html|<html' "$TMP/login_page.body"

curl --silent --show-error --fail-with-body \
  --cookie-jar "$TMP/cookies" \
  --header 'content-type: application/json' \
  --data "$(printf '{\"password\":\"%s\"}' "$PASSWORD")" \
  --dump-header "$TMP/login.headers" \
  --output "$TMP/login.body" \
  "$BASE/api/auth/login"
assert_native_headers "$TMP/login.headers"
grep -Eq '"success"[[:space:]]*:[[:space:]]*true' "$TMP/login.body"

request auth_status --cookie "$TMP/cookies" "$BASE/api/auth/status"
grep -Eq '"authenticated"[[:space:]]*:[[:space:]]*true' "$TMP/auth_status.body"

request settings --cookie "$TMP/cookies" "$BASE/api/settings"
request providers --cookie "$TMP/cookies" "$BASE/api/providers"
grep -Eq '"connections"[[:space:]]*:' "$TMP/providers.body"
request models --cookie "$TMP/cookies" "$BASE/api/models"
grep -Eq '"models"[[:space:]]*:' "$TMP/models.body"
request dashboard --cookie "$TMP/cookies" "$BASE/dashboard"
grep -Eqi '<!doctype html|<html' "$TMP/dashboard.body"

asset="$(grep -Eo '/_next/static/[^"[:space:]<>]+' "$TMP/dashboard.body" | head -n1 || true)"
if [[ -n "$asset" ]]; then
  request next_asset "$BASE$asset"
  [[ -s "$TMP/next_asset.body" ]]
fi

# A protected unknown management route must fail in Rust, never reach Next.
status="$(curl --silent --output "$TMP/unknown.body" --dump-header "$TMP/unknown.headers" --write-out '%{http_code}' --cookie "$TMP/cookies" "$BASE/api/__strict_unknown_route__")"
assert_native_headers "$TMP/unknown.headers"
case "$status" in
  404|405|501) ;;
  *) echo "Unexpected status from strict unknown route: $status" >&2; cat "$TMP/unknown.body" >&2; exit 1 ;;
esac

printf 'Strict Rust + existing Next frontend smoke passed: %s\n' "$BASE"
