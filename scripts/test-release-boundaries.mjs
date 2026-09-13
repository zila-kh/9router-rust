#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cases = JSON.parse(fs.readFileSync(path.join(root, 'rust-backend/assets/request-path-cases.json'), 'utf8'));
const guardPath = path.join(root, 'frontend/src/dashboardGuard.js');
assert.equal(fs.readFileSync(guardPath, 'utf8'), fs.readFileSync(path.join(root, 'scripts/frontend-overrides/src/dashboardGuard.js'), 'utf8'));
const source = fs.readFileSync(guardPath, 'utf8')
  .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '');
let settings = { requireLogin: false, requireApiKey: true };
const context = vm.createContext({
  URL, Headers, process: { env: { NODE_ENV: 'production' } },
  NextResponse: { json: (body, options) => ({ body, ...options }), next: () => ({ status: 200 }), redirect: () => ({ status: 307 }) },
  getSettings: async () => settings,
  validateApiKey: async (key) => key === 'test-api-key',
  getConsistentMachineId: async () => 'test-cli-token',
  verifyDashboardAuthToken: async (token) => token === 'test-session',
  hasTrustedPeerHeaders: () => true,
});
vm.runInContext(source + '\nglobalThis.hooks = __test__; globalThis.runGuard = proxy;', context);
for (const entry of cases) {
  if (entry.error) assert.throws(() => context.hooks.canonicalPath(entry.input), entry.input);
  else assert.equal(context.hooks.canonicalPath(entry.input), entry.expected, entry.input);
}
const request = (pathname, extra = {}) => ({
  nextUrl: { pathname, searchParams: new URLSearchParams() },
  method: 'POST', url: `http://example.test${pathname}`,
  headers: new Headers({ host: 'example.test', 'x-9r-real-ip': '203.0.113.7', ...extra }),
  cookies: { get: () => undefined },
});
for (const route of [
  '/api/oauth/codex/%73tart-proxy', '/%61pi/oauth/xiaomi%2dmimo/exchange',
  '/api/oauth/codex/%70oll-status', '/api/mcp', '/api/mcp/', '/api/tunnel',
]) assert.equal((await context.runGuard(request(route))).status, 403, route);
for (const entry of cases.filter((entry) => entry.error)) {
  assert.equal((await context.runGuard(request(entry.input))).status, 400, entry.input);
}
assert.equal((await context.runGuard(request('/api/oauth/github/device-code'))).status, 200);
assert.equal((await context.runGuard(request('/api/%73ettings/database'))).status, 401);
settings = { requireLogin: true, requireApiKey: true };
assert.equal((await context.runGuard(request('/api/locale/'))).status, 200);
assert.equal((await context.runGuard(request('/api/providers'))).status, 401);
assert.equal((await context.runGuard(request('/v1/models', { 'x-api-key': 'test-api-key' }))).status, 200);

// Exercise dangerous destination cases only in a disposable directory.
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), '9router-safe-materialize-'));
try {
  const scripts = path.join(tmp, 'project/scripts');
  fs.mkdirSync(scripts, { recursive: true });
  const installer = path.join(scripts, 'materialize-frontend.sh');
  fs.copyFileSync(path.join(root, 'scripts/materialize-frontend.sh'), installer);
  const cwd = path.join(tmp, 'project');
  const sentinel = path.join(tmp, 'keep.txt');
  fs.writeFileSync(sentinel, 'must survive');
  const existing = path.join(tmp, 'unrelated');
  fs.mkdirSync(existing);
  fs.writeFileSync(path.join(existing, 'keep.txt'), 'unrelated data');
  fs.symlinkSync(existing, path.join(tmp, 'alias'));
  for (const dest of ['.', '..', '../project', '../project/..', '../unrelated', '../alias', '../missing/..']) {
    const result = spawnSync('bash', [installer, dest], { cwd, encoding: 'utf8', timeout: 5000 });
    assert.equal(result.error, undefined, dest);
    assert.notEqual(result.status, 0, dest);
    assert.match(result.stderr, /Refusing/, dest);
    assert.equal(fs.readFileSync(sentinel, 'utf8'), 'must survive', dest);
    assert.equal(fs.readFileSync(path.join(existing, 'keep.txt'), 'utf8'), 'unrelated data', dest);
    assert.ok(fs.existsSync(installer), dest);
  }
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
console.log(`Release boundary regressions: ${cases.length} shared path fixtures, guard permissions, and 7 safe destination cases passed`);
