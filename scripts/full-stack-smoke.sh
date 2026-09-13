#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
UI_ORIGIN="${NINEROUTER_UI_ORIGIN:-}"
PASSWORD="${NINEROUTER_TEST_PASSWORD:-123456}"
CLI_TOKEN="${NINEROUTER_TEST_CLI_TOKEN:-}"
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

# Locale selection is available on the login page, so its actual POST method must
# be public before authentication and must return the locale cookie through Rust.
request locale_set \
  --cookie-jar "$TMP/locale-cookies" \
  --header 'content-type: application/json' \
  --data '{"locale":"en"}' \
  "$BASE/api/locale"
grep -Eq '"success"[[:space:]]*:[[:space:]]*true' "$TMP/locale_set.body"
grep -Eq '"locale"[[:space:]]*:[[:space:]]*"en"' "$TMP/locale_set.body"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/locale_set.headers"
grep -Eqi '^set-cookie:.*locale=en' "$TMP/locale_set.headers"

locale_get_status="$(curl --silent --show-error \
  --output "$TMP/locale-get.body" \
  --write-out '%{http_code}' \
  "$BASE/api/locale")"
[[ "$locale_get_status" == 401 ]]

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

# Authenticate through Rust before exercising protected dashboard APIs.
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

# An authenticated dashboard session reached through a reverse-proxy hop must not
# gain access to host-local credential import or callback-server controls.
remote_xiaomi_status="$(curl --silent --show-error \
  --cookie "$TMP/cookies" \
  --header 'x-forwarded-for: 198.51.100.23' \
  --dump-header "$TMP/remote-xiaomi.headers" \
  --output "$TMP/remote-xiaomi.body" \
  --write-out '%{http_code}' \
  "$BASE/api/oauth/xiaomi-mimo/auto-import")"
[[ "$remote_xiaomi_status" == 403 ]]
grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/remote-xiaomi.headers"

remote_proxy_status="$(curl --silent --show-error \
  --cookie "$TMP/cookies" \
  --header 'x-forwarded-for: 198.51.100.23' \
  --dump-header "$TMP/remote-proxy.headers" \
  --output "$TMP/remote-proxy.body" \
  --write-out '%{http_code}' \
  "$BASE/api/oauth/codex/start-proxy?app_port=20128")"
[[ "$remote_proxy_status" == 403 ]]
grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/remote-proxy.headers"

# Alternate spellings must be checked by Rust before reaching Next's dynamic routes.
for endpoint in \
  /api/oauth/codex/%73tart-proxy \
  /%61pi/oauth/xiaomi%2dmimo/exchange \
  /api/mcp \
  /api/tunnel/; do
  status="$(curl --path-as-is --silent --show-error \
    --cookie "$TMP/cookies" --header 'x-forwarded-for: 198.51.100.23' \
    --dump-header "$TMP/encoded-local.headers" --output "$TMP/encoded-local.body" \
    --write-out '%{http_code}' "$BASE$endpoint")"
  [[ "$status" == 403 ]]
  grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/encoded-local.headers"
done
for endpoint in /safe/../api/settings /safe/%2e%2e/api/settings /api//settings /api/%GG; do
  status="$(curl --path-as-is --silent --show-error \
    --cookie "$TMP/cookies" --dump-header "$TMP/ambiguous.headers" \
    --output "$TMP/ambiguous.body" --write-out '%{http_code}' "$BASE$endpoint")"
  [[ "$status" == 400 ]]
  grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/ambiguous.headers"
done

# Remote-safe, user-supplied token import instructions remain available to an
# authenticated remote dashboard, proving the local-only matcher is not broad.
request cursor_import_remote \
  --cookie "$TMP/cookies" \
  --header 'x-forwarded-for: 198.51.100.23' \
  "$BASE/api/oauth/cursor/import"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/cursor_import_remote.headers"
grep -Eq '"method"[[:space:]]*:[[:space:]]*"import_token"' "$TMP/cursor_import_remote.body"

# SSO diagnostics must stay protected even though browser login/callback endpoints
# are public. This guards against an overly broad /api/auth/{oidc,saml}/ prefix.
for endpoint in oidc saml; do
  status="$(curl --silent --show-error \
    --request POST \
    --header 'content-type: application/json' \
    --data '{}' \
    --output "$TMP/${endpoint}-test-unauth.body" \
    --write-out '%{http_code}' \
    "$BASE/api/auth/$endpoint/test")"
  [[ "$status" == 401 ]]
done

# Existing dashboard APIs use the exact pinned-upstream response contract in
# compatibility mode, even when a partial native implementation exists.
request settings --cookie "$TMP/cookies" "$BASE/api/settings"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/settings.headers"
request providers --cookie "$TMP/cookies" "$BASE/api/providers"
grep -Eq '"connections"[[:space:]]*:' "$TMP/providers.body"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/providers.headers"

# /api/tags follows Ollama's {"models":[...]} response contract and is protected
# by the same dashboard gate as other management APIs.
unauth_tags_status="$(curl --silent --show-error \
  --output "$TMP/tags-unauth.body" \
  --write-out '%{http_code}' \
  "$BASE/api/tags")"
[[ "$unauth_tags_status" == 401 ]]
request tags --cookie "$TMP/cookies" "$BASE/api/tags"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/tags.headers"
grep -Eq '"models"[[:space:]]*:[[:space:]]*\[' "$TMP/tags.body"

# CLI clients use the same machine-id/secret-derived token as upstream and do not
# need a dashboard cookie for protected management and LLM compatibility APIs.
if [[ -n "$CLI_TOKEN" ]]; then
  request tags_cli --header "x-9r-cli-token: $CLI_TOKEN" "$BASE/api/tags"
  grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/tags_cli.headers"
  grep -Eq '"models"[[:space:]]*:[[:space:]]*\[' "$TMP/tags_cli.body"
fi

# Publicly reachable LLM paths must still require an API key or CLI token. This
# prevents a reverse proxy or loopback hop from turning /v1 into an anonymous API.
unauth_v1_status="$(curl --silent --show-error \
  --dump-header "$TMP/v1-unauth.headers" \
  --output "$TMP/v1-unauth.body" \
  --write-out '%{http_code}' \
  "$BASE/v1")"
[[ "$unauth_v1_status" == 401 ]]
grep -qi '^x-9router-runtime:[[:space:]]*rust' "$TMP/v1-unauth.headers"

if [[ -z "$CLI_TOKEN" ]]; then
  echo 'NINEROUTER_TEST_CLI_TOKEN is required to test protected /v1 compatibility APIs' >&2
  exit 1
fi
LLM_AUTH=(--header "x-9r-cli-token: $CLI_TOKEN")

# Non-native protected APIs must use upstream's current contracts instead of
# being swallowed by the broad native /v1 gateway.
request v1_root "${LLM_AUTH[@]}" "$BASE/v1"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/v1_root.headers"
grep -Eq '"object"[[:space:]]*:[[:space:]]*"list"' "$TMP/v1_root.body"

request v1_double_root "${LLM_AUTH[@]}" "$BASE/v1/v1"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/v1_double_root.headers"
grep -Eq '"object"[[:space:]]*:[[:space:]]*"list"' "$TMP/v1_double_root.body"

request image_models "${LLM_AUTH[@]}" "$BASE/v1/models/image"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/image_models.headers"
grep -Eq '"object"[[:space:]]*:[[:space:]]*"list"' "$TMP/image_models.body"

curl --silent --show-error --fail-with-body \
  "${LLM_AUTH[@]}" \
  --header 'content-type: application/json' \
  --data '{"messages":[{"role":"user","content":"hello"}]}' \
  --dump-header "$TMP/count_tokens.headers" \
  --output "$TMP/count_tokens.body" \
  "$BASE/v1/messages/count_tokens"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/count_tokens.headers"
grep -Eq '"input_tokens"[[:space:]]*:' "$TMP/count_tokens.body"

request gemini_models "${LLM_AUTH[@]}" "$BASE/v1beta/models"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/gemini_models.headers"
grep -Eq '"models"[[:space:]]*:' "$TMP/gemini_models.body"

# local-device returns an empty list on Linux when macOS/Windows speech tools are
# unavailable, but still exercises the nested secret-authenticated internal fetch.
request local_voices "${LLM_AUTH[@]}" "$BASE/v1/audio/voices?provider=local-device"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/local_voices.headers"
grep -Eq '"object"[[:space:]]*:[[:space:]]*"list"' "$TMP/local_voices.body"

# Upstream 0.5.75 added video APIs. A fresh database has no xAI account, so the
# expected response is an upstream validation/credential error, not Rust's old
# 404/501 "media route not implemented" response.
video_status="$(curl --silent --show-error \
  "${LLM_AUTH[@]}" \
  --header 'content-type: application/json' \
  --data '{}' \
  --dump-header "$TMP/video.headers" \
  --output "$TMP/video.body" \
  --write-out '%{http_code}' \
  "$BASE/v1/videos/generations")"
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/video.headers"
case "$video_status" in
  404|421|501)
    echo "Video compatibility route was not reached (HTTP $video_status)" >&2
    cat "$TMP/video.body" >&2
    exit 1
    ;;
esac
! grep -qi 'Rust media route not implemented yet' "$TMP/video.body"

# Exercise the actual web-fetch handler with deliberately invalid JSON. A 400
# from upstream proves Rust routed the authenticated request to compatibility
# code without making an external network request.
web_fetch_status="$(curl --silent --show-error \
  "${LLM_AUTH[@]}" \
  --header 'content-type: application/json' \
  --data '{' \
  --dump-header "$TMP/web-fetch.headers" \
  --output "$TMP/web-fetch.body" \
  --write-out '%{http_code}' \
  "$BASE/v1/web/fetch")"
[[ "$web_fetch_status" == 400 ]]
grep -qi '^x-9router-runtime:[[:space:]]*upstream-compat' "$TMP/web-fetch.headers"
grep -qi 'Invalid JSON body' "$TMP/web-fetch.body"

# A standards-compliant CORS preflight may be answered by Axum's outer CorsLayer
# with HTTP 200 or by the compatibility handler itself with HTTP 204. Validate the
# actual browser contract rather than coupling the smoke test to one middleware.
web_preflight_status="$(curl --silent --show-error \
  --request OPTIONS \
  --header 'Origin: https://client.example' \
  --header 'Access-Control-Request-Method: POST' \
  --dump-header "$TMP/web-preflight.headers" \
  --output "$TMP/web-preflight.body" \
  --write-out '%{http_code}' \
  "$BASE/v1/web/fetch")"
case "$web_preflight_status" in
  200|204) ;;
  *)
    echo "Unexpected web preflight response (HTTP $web_preflight_status)" >&2
    cat "$TMP/web-preflight.body" >&2
    exit 1
    ;;
esac
grep -qi '^access-control-allow-origin:[[:space:]]*\*' "$TMP/web-preflight.headers"
grep -Eqi '^access-control-allow-methods:[[:space:]]*(\*|.*POST)' "$TMP/web-preflight.headers"

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
