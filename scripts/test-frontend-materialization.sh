#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

tar --exclude='node_modules' --exclude='.next' -cf - -C "$ROOT" frontend | tar -xf - -C "$TMP"

# Simulate a stale/upstream refresh that lost the reviewed trust-boundary files.
printf '%s\n' '// intentionally stale custom server' > "$TMP/frontend/custom-server.js"
printf '%s\n' '// intentionally stale dashboard guard' > "$TMP/frontend/src/dashboardGuard.js"
printf '%s\n' '// intentionally stale login route' > "$TMP/frontend/src/app/api/auth/login/route.js"
printf '%s\n' '// intentionally stale settings route' > "$TMP/frontend/src/app/api/settings/route.js"

bash "$ROOT/scripts/materialize-frontend.sh" "$TMP/frontend"

cmp "$ROOT/scripts/frontend-overrides/custom-server.js" "$TMP/frontend/custom-server.js"
cmp "$ROOT/scripts/frontend-overrides/src/dashboardGuard.js" "$TMP/frontend/src/dashboardGuard.js"
cmp \
  "$ROOT/scripts/frontend-overrides/src/app/api/auth/login/route.js" \
  "$TMP/frontend/src/app/api/auth/login/route.js"
cmp \
  "$ROOT/scripts/frontend-overrides/src/app/api/settings/route.js" \
  "$TMP/frontend/src/app/api/settings/route.js"

node - "$TMP/frontend/package.json" <<'NODE'
const fs = require("node:fs");
const packagePath = process.argv[2];
const pkg = JSON.parse(fs.readFileSync(packagePath, "utf8"));
if (pkg.scripts?.["start:ui"] !== "node .next/standalone/custom-server.js") {
  throw new Error("start:ui was not restored to the hardened standalone wrapper");
}
if (pkg.scripts?.["cli:pack"] || pkg.scripts?.["cli:publish"]) {
  throw new Error("materialization restored CLI scripts for the non-vendored legacy CLI");
}
if (pkg.dependencies?.["monaco-editor"] !== "^0.56.0") {
  throw new Error("materialization did not restore the reviewed dependency manifest");
}
NODE

grep -q 'x-9router-ui-secret' "$TMP/frontend/custom-server.js"
grep -q 'PUBLIC_API_ROUTES' "$TMP/frontend/src/dashboardGuard.js"
grep -q 'mustChangePassword' "$TMP/frontend/src/app/api/auth/login/route.js"
grep -q 'INITIAL_PASSWORD' "$TMP/frontend/src/app/api/settings/route.js"

printf 'Frontend materialization security overlays: OK\n'
