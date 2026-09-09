#!/usr/bin/env bash
set -euo pipefail

URL="${1:?usage: wait-http.sh URL [SECONDS]}"
TIMEOUT="${2:-60}"
DEADLINE=$((SECONDS + TIMEOUT))

while (( SECONDS < DEADLINE )); do
  if curl --fail --silent --show-error --max-time 2 "$URL" >/dev/null 2>&1; then
    printf 'Ready: %s\n' "$URL"
    exit 0
  fi
  sleep 1
done

echo "Timed out waiting for $URL after ${TIMEOUT}s" >&2
exit 1
