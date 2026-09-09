#!/usr/bin/env bash
set -euo pipefail
EXPECTED_COMMIT=eb712ca821f0ba6bc41043fbd14494c5af5daba5
DEST=${1:-9router-rust-1.0.1}
OVERLAY_DIR=$(cd "$(dirname "$0")/.." && pwd)
if [ -e "$DEST" ]; then
  echo "error: destination already exists: $DEST" >&2
  exit 2
fi
git clone https://github.com/decolua/9router.git "$DEST"
cd "$DEST"
git checkout "$EXPECTED_COMMIT"
"$OVERLAY_DIR/scripts/install-overlay.sh" .
printf '\nReady in %s\n' "$(pwd)"
printf '%s\n' 'Run: ./scripts/test-port.sh .'
printf '%s\n' 'Then strict: ./scripts/run-dev.sh .'
printf '%s\n' 'Or hybrid: ./scripts/run-hybrid.sh .'
