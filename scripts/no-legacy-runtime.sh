#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ -n "${NINEROUTER_LEGACY_BACKEND_ORIGIN:-}" || -n "${LEGACY_BACKEND_ORIGIN:-}" ]]; then
  echo "Legacy backend origin is configured in strict mode." >&2
  exit 1
fi
if [[ "${NINEROUTER_COMPAT_API:-0}" != 0 ]]; then
  echo "Compatibility API fallback is enabled in strict mode." >&2
  exit 1
fi

for path in \
  /api/health \
  /api/version \
  /api/auth/status \
  /api/settings/require-login \
  /login \
  /dashboard; do
  code="$(curl --silent --show-error --max-time 10 \
    --dump-header "$TMP/headers" \
    --output "$TMP/body" \
    --write-out '%{http_code}' \
    "$BASE$path" || true)"
  if grep -Eqi '^x-9router-runtime:[[:space:]]*(legacy-bridge|upstream-compat)' "$TMP/headers"; then
    echo "Fallback runtime served $path in strict mode (HTTP $code)." >&2
    exit 1
  fi
done

echo "Strict runtime did not use a compatibility or legacy backend bridge."
