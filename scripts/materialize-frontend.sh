#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-$ROOT/frontend}"
UPSTREAM_URL="${NINEROUTER_UPSTREAM_URL:-https://github.com/decolua/9router.git}"
UPSTREAM_SHA="${NINEROUTER_UPSTREAM_SHA:-17c4cc76877bd1755030a8414f8d0083f48dcccf}"

case "$DEST" in
  /|""|.) echo "Refusing unsafe frontend destination: $DEST" >&2; exit 2 ;;
esac

keep_existing=0
if [[ -d "$DEST/.git" ]]; then
  current="$(git -C "$DEST" rev-parse HEAD 2>/dev/null || true)"
  origin="$(git -C "$DEST" remote get-url origin 2>/dev/null || true)"
  if [[ "$current" == "$UPSTREAM_SHA" && "$origin" == "$UPSTREAM_URL" ]]; then
    keep_existing=1
  fi
elif [[ -f "$DEST/package.json" && -f "$DEST/.upstream-commit" && -d "$DEST/src/app/api" ]]; then
  current="$(tr -d '\r\n' < "$DEST/.upstream-commit")"
  if [[ "$current" == "$UPSTREAM_SHA" ]]; then
    keep_existing=1
  fi
fi

if [[ "$keep_existing" != 1 && -e "$DEST" ]]; then
  rm -rf "$DEST"
fi

if [[ ! -d "$DEST" ]]; then
  git clone --filter=blob:none --no-checkout "$UPSTREAM_URL" "$DEST"
  git -C "$DEST" checkout --detach "$UPSTREAM_SHA"
fi

# The Next process renders the existing dashboard and retains upstream API route
# handlers only as an internal compatibility layer. Rust remains the sole public
# listener and authenticates every request before it may reach those handlers.
python3 - "$DEST" "$UPSTREAM_SHA" <<'PY'
from pathlib import Path
import json
import sys

root = Path(sys.argv[1])
upstream_sha = sys.argv[2]

proxy = root / "src/proxy.js"
proxy.write_text('''import { NextResponse } from "next/server";\n\nconst RUST_BACKEND_PREFIXES = ["/api", "/v1", "/v1beta", "/responses", "/codex"];\nconst INTERNAL_SECRET_HEADER = "x-9router-ui-secret";\n\nfunction isBackendPath(pathname) {\n  return RUST_BACKEND_PREFIXES.some(\n    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),\n  );\n}\n\nfunction isCompatApiPath(pathname) {\n  return pathname === "/api" || pathname.startsWith("/api/");\n}\n\nfunction isTrustedRustRequest(request) {\n  const configured = process.env.NINEROUTER_UI_SECRET;\n  const supplied = request.headers.get(INTERNAL_SECRET_HEADER);\n  return Boolean(configured && supplied && supplied === configured);\n}\n\nexport default async function proxy(request) {\n  if (process.env.NINEROUTER_UI_ONLY === "1") {\n    if (isCompatApiPath(request.nextUrl.pathname) && isTrustedRustRequest(request)) {\n      return NextResponse.next();\n    }\n    if (isBackendPath(request.nextUrl.pathname)) {\n      return NextResponse.json(\n        { error: "Rust backend required", code: "RUST_BACKEND_REQUIRED" },\n        { status: 421, headers: { "Cache-Control": "no-store" } },\n      );\n    }\n    return NextResponse.next();\n  }\n  const { proxy: dashboardProxy } = await import("./dashboardGuard");\n  return dashboardProxy(request);\n}\n\nexport const config = {\n  matcher: ["/((?!_next/static|_next/image|favicon\\\\.ico).*)"],\n};\n''', encoding="utf-8")

layout = root / "src/app/layout.js"
lines = layout.read_text(encoding="utf-8").splitlines()
blocked = (
    '@/lib/network/initOutboundProxy',
    '@/shared/services/bootstrap',
    '@/lib/consoleLogBuffer',
    'initConsoleLogCapture();',
)
lines = [line for line in lines if not any(token in line for token in blocked)]
layout.write_text('\n'.join(lines) + '\n', encoding="utf-8")

instrumentation = root / "src/instrumentation.js"
text = instrumentation.read_text(encoding="utf-8")
guard = '  if (process.env.NINEROUTER_UI_ONLY === "1") return;\n'
if guard not in text:
    needle = 'export async function register() {\n'
    if needle not in text:
        raise SystemExit("instrumentation.js shape changed; refusing an unsafe patch")
    text = text.replace(needle, needle + guard, 1)
instrumentation.write_text(text, encoding="utf-8")

next_config = root / "next.config.mjs"
text = next_config.read_text(encoding="utf-8")
guard = '  async rewrites() {\n    if (process.env.NINEROUTER_UI_ONLY === "1") return [];\n'
if guard not in text:
    needle = '  async rewrites() {\n'
    if needle not in text:
        raise SystemExit("next.config.mjs rewrite shape changed; refusing an unsafe patch")
    text = text.replace(needle, guard, 1)
next_config.write_text(text, encoding="utf-8")

package = root / "package.json"
data = json.loads(package.read_text(encoding="utf-8"))
data.setdefault('scripts', {})['dev:ui'] = 'next dev --webpack --hostname 127.0.0.1 --port 20129'
data['scripts']['start:ui'] = 'next start --hostname 127.0.0.1 --port 20129'
package.write_text(json.dumps(data, indent=2) + '\n', encoding="utf-8")

(root / ".upstream-commit").write_text(upstream_sha + "\n", encoding="utf-8")
(root / ".9router-ui-snapshot").write_text(
    "upstream=decolua/9router\n"
    f"commit={upstream_sha}\n"
    "mode=ui-plus-secured-api-compat\n",
    encoding="utf-8",
)
PY

printf 'Materialized existing Next/React frontend at %s\n' "$DEST"
printf 'Pinned commit: %s\n' "$UPSTREAM_SHA"
