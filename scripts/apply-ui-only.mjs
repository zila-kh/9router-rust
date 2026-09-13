#!/usr/bin/env node
import fs from 'node:fs';

function read(path) {
  return fs.readFileSync(path, 'utf8');
}

function writeIfChanged(path, value) {
  const before = read(path);
  if (before === value) {
    console.log(`[ui-only] ${path}: already current`);
    return;
  }
  fs.writeFileSync(path, value);
  console.log(`[ui-only] updated ${path}`);
}

const proxy = `import { NextResponse } from "next/server";

const RUST_BACKEND_PREFIXES = ["/api", "/v1", "/v1beta", "/responses", "/codex"];
const INTERNAL_SECRET_HEADER = "x-9router-ui-secret";

function isBackendPath(pathname) {
  return RUST_BACKEND_PREFIXES.some(
    (prefix) => pathname === prefix || pathname.startsWith(\`${'${prefix}'}/\`),
  );
}

function isCompatApiPath(pathname) {
  return pathname === "/api" || pathname.startsWith("/api/");
}

function isTrustedRustRequest(request) {
  const configured = process.env.NINEROUTER_UI_SECRET;
  const supplied = request.headers.get(INTERNAL_SECRET_HEADER);
  return Boolean(configured && supplied && supplied === configured);
}

export default async function proxy(request) {
  if (process.env.NINEROUTER_UI_ONLY === "1") {
    if (isCompatApiPath(request.nextUrl.pathname) && isTrustedRustRequest(request)) {
      return NextResponse.next();
    }
    if (isBackendPath(request.nextUrl.pathname)) {
      return NextResponse.json(
        { error: "Rust backend required", code: "RUST_BACKEND_REQUIRED" },
        { status: 421, headers: { "Cache-Control": "no-store" } },
      );
    }
    return NextResponse.next();
  }
  const { proxy: dashboardProxy } = await import("./dashboardGuard");
  return dashboardProxy(request);
}

export const config = {
  matcher: ["/((?!_next/static|_next/image|favicon\\\\.ico).*)"],
};
`;
writeIfChanged('src/proxy.js', proxy);

let layout = read('src/app/layout.js');
layout = layout
  .split('\n')
  .filter((line) => ![
    '@/lib/network/initOutboundProxy',
    '@/shared/services/bootstrap',
    '@/lib/consoleLogBuffer',
    'initConsoleLogCapture();',
  ].some((token) => line.includes(token)))
  .join('\n');
if (!layout.endsWith('\n')) layout += '\n';
writeIfChanged('src/app/layout.js', layout);

let instrumentation = read('src/instrumentation.js');
const instrumentationGuard = '  if (process.env.NINEROUTER_UI_ONLY === "1") return;\n';
if (!instrumentation.includes(instrumentationGuard)) {
  const needle = 'export async function register() {\n';
  if (!instrumentation.includes(needle)) {
    throw new Error('src/instrumentation.js shape changed');
  }
  instrumentation = instrumentation.replace(needle, needle + instrumentationGuard);
}
writeIfChanged('src/instrumentation.js', instrumentation);

let nextConfig = read('next.config.mjs');
const rewriteGuard = '  async rewrites() {\n    if (process.env.NINEROUTER_UI_ONLY === "1") return [];\n';
if (!nextConfig.includes(rewriteGuard)) {
  const needle = '  async rewrites() {\n';
  if (!nextConfig.includes(needle)) {
    throw new Error('next.config.mjs rewrite shape changed');
  }
  nextConfig = nextConfig.replace(needle, rewriteGuard);
}
writeIfChanged('next.config.mjs', nextConfig);

const packageJson = JSON.parse(read('package.json'));
packageJson.scripts ||= {};
packageJson.scripts['dev:ui'] = 'next dev --webpack --hostname 127.0.0.1 --port 20129';
packageJson.scripts['start:ui'] = 'next start --hostname 127.0.0.1 --port 20129';
writeIfChanged('package.json', `${JSON.stringify(packageJson, null, 2)}\n`);
