#!/usr/bin/env bash
set -euo pipefail

EXPECTED_COMMIT=17c4cc76877bd1755030a8414f8d0083f48dcccf
DEST=${1:-9router-rust-1.0.1}
OVERLAY_DIR=$(cd "$(dirname "$0")/.." && pwd)

if [[ -e "$DEST" ]]; then
  echo "error: destination already exists: $DEST" >&2
  exit 2
fi

git clone https://github.com/decolua/9router.git "$DEST"
cd "$DEST"
git checkout "$EXPECTED_COMMIT"
"$OVERLAY_DIR/scripts/install-overlay.sh" .

printf '\nReady in %s\n' "$(pwd)"
printf '%s\n' 'Run checks:     ./scripts/test-port.sh .'
printf '%s\n' 'Compatibility: ./scripts/run-dev.sh .'
printf '%s\n' 'Strict native: ./scripts/run-full-stack-strict.sh'
