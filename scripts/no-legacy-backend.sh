#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0

# Source may retain migration documentation, but executable Rust must not contain
# an HTTP fallback to the old JS server in a strict release.
if grep -RInE 'legacy_backend_origin|legacy_proxy::|legacy-bridge' rust-backend/src rust-backend/Cargo.toml 2>/dev/null; then
  echo "Executable legacy backend bridge references remain." >&2
  fail=1
fi

# Node is allowed only for rendering the retained Next/React frontend. Backend
# route handlers and open-sse are not copied into the Rust runtime tree.
if find rust-backend -type f \( -name '*.js' -o -name '*.cjs' -o -name '*.mjs' \) -print -quit | grep -q .; then
  echo "JavaScript exists inside rust-backend." >&2
  find rust-backend -type f \( -name '*.js' -o -name '*.cjs' -o -name '*.mjs' \) -print >&2
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi

echo "No executable legacy backend bridge detected."
