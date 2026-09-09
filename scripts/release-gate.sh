#!/usr/bin/env bash
set -euo pipefail
ROOT=${1:-.}
cd "$ROOT"
./scripts/test-port.sh .
node ./scripts/audit-parity.mjs
