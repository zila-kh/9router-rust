#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
OVERLAY_DIR=$(cd "$(dirname "$0")/.." && pwd)
EXPECTED_COMMIT=eb712ca821f0ba6bc41043fbd14494c5af5daba5
cd "$ROOT"
if [ ! -f package.json ] || [ ! -d open-sse ] || [ ! -d src/app ]; then
  echo "error: target is not a 9router checkout" >&2
  exit 2
fi
if command -v git >/dev/null 2>&1 && [ -d .git ]; then
  ACTUAL=$(git rev-parse HEAD 2>/dev/null || true)
  if [ -n "$ACTUAL" ] && [ "$ACTUAL" != "$EXPECTED_COMMIT" ]; then
    echo "warning: 1.0.1 was built against $EXPECTED_COMMIT, target is $ACTUAL" >&2
    echo "warning: run the parity audit after installation; upstream drift is not silently accepted" >&2
  fi
fi
mkdir -p ./scripts
rm -rf ./rust-backend
cp -R "$OVERLAY_DIR/rust-backend" ./rust-backend
for f in export-rust-catalog.mjs apply-ui-only.mjs audit-parity.mjs run-dev.sh run-hybrid.sh test-port.sh release-gate.sh static-audit.py bootstrap.sh; do
  cp "$OVERLAY_DIR/scripts/$f" "./scripts/$f"
done
chmod +x ./scripts/run-dev.sh ./scripts/run-hybrid.sh ./scripts/test-port.sh ./scripts/release-gate.sh ./scripts/static-audit.py ./scripts/bootstrap.sh
node ./scripts/apply-ui-only.mjs
node ./scripts/export-rust-catalog.mjs
printf '%s\n' '1.0.1' > rust-backend/VERSION
printf '%s\n' "$EXPECTED_COMMIT" > rust-backend/UPSTREAM_COMMIT
python3 ./scripts/static-audit.py
cat <<MSG
Installed 9Router Rust backend overlay 1.0.1.
Pinned upstream: $EXPECTED_COMMIT

Build/test:
  cargo build --release --manifest-path rust-backend/Cargo.toml
  cargo test --all-targets --manifest-path rust-backend/Cargo.toml

Strict Rust run:
  ./scripts/run-dev.sh .

Hybrid parity run (unported endpoints fall through to the frozen JS backend):
  ./scripts/run-hybrid.sh .

Audit:
  node ./scripts/audit-parity.mjs
MSG
