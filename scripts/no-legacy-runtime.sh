#!/usr/bin/env bash
set -euo pipefail

BASE="${1:-http://127.0.0.1:20128}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# The migration bridge may remain as dormant source while the port is audited,
# but a strict run must have no configured legacy origin and may never emit the
# bridge marker on any public request.
if [[ -n "${NINEROUTER_LEGACY_BACKEND_ORIGIN:-}" || -n "${LEGACY_BACKEND_ORIGIN:-}" ]]; then
  echo "Legacy backend origin is configured in strict mode." >&2
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
  if grep -qi '^x-9router-runtime:[[:space:]]*legacy-bridge' "$TMP/headers"; then
    echo "Legacy backend bridge served $path (HTTP $code)." >&2
    exit 1
  fi
done

echo "Strict runtime did not use a legacy backend bridge."
