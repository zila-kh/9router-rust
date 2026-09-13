#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
UI_ORIGIN="${NINEROUTER_UI_ORIGIN:-}"
PASSWORD="${NINEROUTER_TEST_PASSWORD:-123456}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

request() {
  local name="$1"; shift
  curl --silent --show-error --fail-with-body \
    --dump-header "$TMP/$name.headers" \
    --output "$TMP/$name.body" "$@"
}

request health "$BASE/api/health"
grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' "$TMP/health.body"
grep -Eq '"runtime"[[:space:]]*:[[:space:]]*"rust"' "$TMP/health.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/health.headers"

request login_page "$BASE/login"
grep -Eqi '<!doctype html|<html' "$TMP/login_page.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/login_page.headers"

# Public auth redirects must pass through unchanged rather than being followed by
# Rust's HTTP client. The forwarded host also has to remain the public Rust origin.
oidc_status="$(curl --silent --show-error \
  --dump-header "$TMP/oidc.headers" \
  --output "$TMP/oidc.body" \
  --write-out '%{http_code}' \
  "$BASE/api/auth/oidc/start")"
case "$oidc_status" in
  301|302|303|307|308) ;;
  *) echo "OIDC start did not return a browser redirect (HTTP $oidc_status)" >&2; exit 1 ;;
esac
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/oidc.headers"
oidc_location="$(awk 'BEGIN { IGNORECASE=1 } /^location:/ { sub(/^[^:]+:[[:space:]]*/, ""); gsub(/\r/, ""); print; exit }' "$TMP/oidc.headers")"
case "$oidc_location" in
  "$BASE"/login?error=oidc_not_configured*) ;;
  *) echo "OIDC redirect lost the public origin: $oidc_location" >&2; exit 1 ;;
esac

# Authenticate through Rust before exercising protected compatibility APIs.
curl --silent --show-error --fail-with-body \
  --cookie-jar "$TMP/cookies" \
  --header 'content-type: application/json' \
  --data "$(printf '{\"password\":\"%s\"}' "$PASSWORD")" \
  --dump-header "$TMP/login.headers" \
  --output "$TMP/login.body" \
  "$BASE/api/auth/login"
grep -Eq '"success"[[:space:]]*:[[:space:]]*true' "$TMP/login.body"
grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/login.headers"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/login.headers"

request auth_status --cookie "$TMP/cookies" "$BASE/api/auth/status"
grep -Eq '"authenticated"[[:space:]]*:[[:space:]]*true' "$TMP/auth_status.body"
grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/auth_status.headers"

# Existing dashboard APIs use the exact pinned-upstream response contract in
# compatibility mode, even when a partial native implementation exists.
request settings --cookie "$TMP/cookies" "$BASE/api/settings"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/settings.headers"
request providers --cookie "$TMP/cookies" "$BASE/api/providers"
grep -Eq '"connections"[[:space:]]*:' "$TMP/providers.body"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/providers.headers"

# /api/tags is upstream-only and protected by the same dashboard gate as upstream.
unauth_tags_status="$(curl --silent --show-error \
  --output "$TMP/tags-unauth.body" \
  --write-out '%{http_code}' \
  "$BASE/api/tags")"
[[ "$unauth_tags_status" == 401 ]]
request tags --cookie "$TMP/cookies" "$BASE/api/tags"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/tags.headers"
grep -Eq '^[[:space:]]*\[' "$TMP/tags.body"

# The internal Next listener must reject the same API request without Rust's secret.
if [[ -n "$UI_ORIGIN" ]]; then
  direct_status="$(curl --silent --show-error --output "$TMP/direct.body" --write-out '%{http_code}' "$UI_ORIGIN/api/tags" || true)"
  case "$direct_status" in
    403|421) ;;
    *)
      echo "Internal UI API was reachable without Rust authentication (HTTP $direct_status)" >&2
      cat "$TMP/direct.body" >&2
      exit 1
      ;;
  esac
fi

request dashboard --cookie "$TMP/cookies" "$BASE/dashboard"
grep -Eqi '<!doctype html|<html' "$TMP/dashboard.body"
! grep -qi 'x-9router-runtime: legacy-bridge' "$TMP/dashboard.headers"

# Verify an actual Next asset traverses the Rust UI proxy when one is referenced.
asset="$(grep -Eo '/_next/static/[^"[:space:]]+' "$TMP/dashboard.body" | head -n1 || true)"
if [[ -n "$asset" ]]; then
  request next_asset "$BASE$asset"
  [[ -s "$TMP/next_asset.body" ]]
fi

printf 'Rust + secured upstream compatibility smoke passed against %s\n' "$BASE"
