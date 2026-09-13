#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0

require_text() {
  local file="$1"
  local pattern="$2"
  local message="$3"
  if ! grep -qE "$pattern" "$file"; then
    echo "$message" >&2
    fail=1
  fi
}

# Dormant migration code may remain compiled during the port, but every strict
# launcher and strict CI job must explicitly disable both fallback mechanisms.
require_text scripts/run-full-stack-strict.sh \
  'NINEROUTER_DISABLE_LEGACY_BRIDGE=1' \
  'Strict launcher does not disable the legacy bridge.'
require_text scripts/run-full-stack-strict.sh \
  'NINEROUTER_COMPAT_API=0' \
  'Strict launcher does not disable compatibility APIs.'
require_text .github/workflows/strict-full-stack-v2.yml \
  'NINEROUTER_DISABLE_LEGACY_BRIDGE:[[:space:]]*"1"' \
  'Strict CI does not disable the legacy bridge.'
require_text .github/workflows/strict-full-stack-v2.yml \
  'NINEROUTER_COMPAT_API:[[:space:]]*"0"' \
  'Strict CI does not disable compatibility APIs.'

# Next may retain upstream handlers for compatibility mode, but direct public
# backend access must stay blocked without Rust's internal secret.
require_text frontend/src/proxy.js \
  'x-9router-ui-secret' \
  'Frontend proxy is missing the Rust internal-secret guard.'
require_text frontend/src/proxy.js \
  'RUST_BACKEND_REQUIRED' \
  'Frontend proxy no longer rejects direct backend requests.'

if find rust-backend -type f \( -name '*.js' -o -name '*.cjs' -o -name '*.mjs' \) -print -quit | grep -q .; then
  echo "JavaScript exists inside rust-backend." >&2
  find rust-backend -type f \( -name '*.js' -o -name '*.cjs' -o -name '*.mjs' \) -print >&2
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi

echo "Strict launch paths disable all compatibility and legacy bridges."
