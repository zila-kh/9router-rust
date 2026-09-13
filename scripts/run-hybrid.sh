#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "${1:-.}" && pwd)"
echo "run-hybrid.sh now uses the secured Rust-first compatibility stack." >&2
exec bash "$ROOT/scripts/run-dev.sh" "$ROOT"
