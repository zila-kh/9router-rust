#!/usr/bin/env node
import fs from 'node:fs';

function patch(path, fn) {
  const before = fs.readFileSync(path, 'utf8');
  const after = fn(before);
  if (after === before) {
    console.log(`[ui-only] ${path}: already patched or no change needed`);
  } else {
    fs.writeFileSync(path, after);
    console.log(`[ui-only] patched ${path}`);
  }
}

patch('src/proxy.js', (s) => {
  if (s.includes('NINEROUTER_UI_ONLY')) return s;
  if (!s.includes('NextResponse')) s = `import { NextResponse } from "next/server";\n${s}`;
  return s.replace(
    'export default async function proxy(request) {\n  return dashboardProxy(request);\n}',
    'export default async function proxy(request) {\n  // Rust owns auth/API/security in backend-only port mode. Keep Next strictly UI-only.\n  if (process.env.NINEROUTER_UI_ONLY === "1") return NextResponse.next();\n  return dashboardProxy(request);\n}'
  );
});

patch('src/instrumentation.js', (s) => {
  if (s.includes('process.env.NINEROUTER_UI_ONLY !== "1"')) return s;
  return s.replace(
    'if (process.env.NEXT_RUNTIME === "nodejs") {',
    'if (process.env.NEXT_RUNTIME === "nodejs" && process.env.NINEROUTER_UI_ONLY !== "1") {'
  );
});
