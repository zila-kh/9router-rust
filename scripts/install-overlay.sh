#!/usr/bin/env bash
set -euo pipefail

ROOT=${1:-.}
OVERLAY_DIR=$(cd "$(dirname "$0")/.." && pwd)
EXPECTED_COMMIT=17c4cc76877bd1755030a8414f8d0083f48dcccf
cd "$ROOT"

if [[ ! -f package.json || ! -d open-sse || ! -d src/app ]]; then
  echo "error: target is not a 9Router checkout" >&2
  exit 2
fi

if command -v git >/dev/null 2>&1 && [[ -d .git ]]; then
  ACTUAL=$(git rev-parse HEAD 2>/dev/null || true)
  if [[ -n "$ACTUAL" && "$ACTUAL" != "$EXPECTED_COMMIT" ]]; then
    echo "warning: this overlay is pinned to $EXPECTED_COMMIT, target is $ACTUAL" >&2
    echo "warning: run the parity audit after installation; upstream drift is not silently accepted" >&2
  fi
fi

mkdir -p ./scripts
rm -rf ./rust-backend
cp -R "$OVERLAY_DIR/rust-backend" ./rust-backend

for file in \
  export-rust-catalog.mjs \
  apply-ui-only.mjs \
  audit-parity.mjs \
  materialize-frontend.sh \
  run-dev.sh \
  run-prod.sh \
  run-hybrid.sh \
  run-full-stack-strict.sh \
  full-stack-smoke.sh \
  full-stack-smoke-v2.sh \
  wait-http.sh \
  test-port.sh \
  release-gate.sh \
  static-audit.py \
  bootstrap.sh; do
  cp "$OVERLAY_DIR/scripts/$file" "./scripts/$file"
done

chmod +x \
  ./scripts/materialize-frontend.sh \
  ./scripts/run-dev.sh \
  ./scripts/run-prod.sh \
  ./scripts/run-hybrid.sh \
  ./scripts/run-full-stack-strict.sh \
  ./scripts/full-stack-smoke.sh \
  ./scripts/full-stack-smoke-v2.sh \
  ./scripts/wait-http.sh \
  ./scripts/test-port.sh \
  ./scripts/release-gate.sh \
  ./scripts/static-audit.py \
  ./scripts/bootstrap.sh

node ./scripts/apply-ui-only.mjs
node ./scripts/export-rust-catalog.mjs
printf '%s\n' '1.0.1' > rust-backend/VERSION
printf '%s\n' "$EXPECTED_COMMIT" > rust-backend/UPSTREAM_COMMIT
python3 ./scripts/static-audit.py

cat <<MSG
Installed 9Router Rust backend overlay 1.0.1.
Pinned upstream: $EXPECTED_COMMIT (9Router 0.5.75)

Build/test:
  cargo build --release --locked --manifest-path rust-backend/Cargo.toml
  cargo test --all-targets --manifest-path rust-backend/Cargo.toml

Usable Rust-first compatibility stack:
  ./scripts/run-dev.sh .

Strict native-only stack:
  ./scripts/run-full-stack-strict.sh

Native parity audit:
  node ./scripts/audit-parity.mjs
MSG
