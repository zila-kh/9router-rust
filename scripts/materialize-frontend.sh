#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-$ROOT/frontend}"
UPSTREAM_URL="${NINEROUTER_UPSTREAM_URL:-https://github.com/decolua/9router.git}"
UPSTREAM_SHA="${NINEROUTER_UPSTREAM_SHA:-eb712ca821f0ba6bc41043fbd14494c5af5daba5}"

case "$DEST" in
  /|""|.) echo "Refusing unsafe frontend destination: $DEST" >&2; exit 2 ;;
esac

if [[ -d "$DEST/.git" ]]; then
  current="$(git -C "$DEST" rev-parse HEAD 2>/dev/null || true)"
  origin="$(git -C "$DEST" remote get-url origin 2>/dev/null || true)"
  if [[ "$current" == "$UPSTREAM_SHA" && "$origin" == "$UPSTREAM_URL" ]]; then
    echo "Pinned frontend already present at $DEST"
  else
    rm -rf "$DEST"
  fi
elif [[ -e "$DEST" ]]; then
  rm -rf "$DEST"
fi

if [[ ! -d "$DEST/.git" ]]; then
  git clone --filter=blob:none --no-checkout "$UPSTREAM_URL" "$DEST"
  git -C "$DEST" checkout --detach "$UPSTREAM_SHA"
fi

# Next remains the existing React UI renderer only. Rust owns authentication,
# management APIs and all /v1 compatibility routes at the public listener.
python3 - "$DEST" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
proxy = root / "src/proxy.js"
proxy.write_text('''import { NextResponse } from "next/server";\nimport { proxy as dashboardProxy } from "./dashboardGuard";\n\nexport default async function proxy(request) {\n  // In UI-only mode the public Rust listener has already performed the security\n  // decision. Next must only render pages/assets and must never touch SQLite.\n  if (process.env.NINEROUTER_UI_ONLY === "1") return NextResponse.next();\n  return dashboardProxy(request);\n}\n\nexport const config = {\n  matcher: ["/((?!_next/static|_next/image|favicon\\\\.ico).*)"],\n};\n''', encoding="utf-8")

instrumentation = root / "src/instrumentation.js"
text = instrumentation.read_text(encoding="utf-8")
needle = 'if (process.env.NEXT_RUNTIME === "nodejs") {'
replacement = 'if (process.env.NEXT_RUNTIME === "nodejs" && process.env.NINEROUTER_UI_ONLY !== "1") {'
if needle not in text and replacement not in text:
    raise SystemExit("instrumentation.js shape changed; refusing an unsafe patch")
instrumentation.write_text(text.replace(needle, replacement, 1), encoding="utf-8")

# Mark the checkout so support reports can prove the exact UI snapshot/mode.
(root / ".9router-ui-snapshot").write_text(
    "upstream=decolua/9router\\n"
    "commit=eb712ca821f0ba6bc41043fbd14494c5af5daba5\\n"
    "mode=ui-only\\n",
    encoding="utf-8",
)
PY

printf 'Materialized existing Next/React frontend at %s\n' "$DEST"
printf 'Pinned commit: %s\n' "$UPSTREAM_SHA"
