#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const manifest = JSON.parse(
  fs.readFileSync(path.join(root, 'rust-backend/parity/routes.json'), 'utf8'),
);
const inventoryPath = path.join(root, 'rust-backend/parity/upstream-routes.json');
let routes = [];

if (fs.existsSync(inventoryPath)) {
  const inventory = JSON.parse(fs.readFileSync(inventoryPath, 'utf8'));
  routes = (inventory.routes || [])
    .map((route) => (typeof route === 'string' ? route : route.file))
    .filter(Boolean)
    .sort();
} else {
  const appRoot = path.join(root, 'frontend/src/app');
  const apiRoot = path.join(appRoot, 'api');
  const walk = (dir, out = []) => {
    if (!fs.existsSync(dir)) return out;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const item = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(item, out);
      else if (entry.isFile() && entry.name === 'route.js') {
        out.push(path.relative(appRoot, item).split(path.sep).join('/'));
      }
    }
    return out;
  };
  routes = walk(apiRoot).sort();
}

const native = new Set(manifest.native || []);
const covered = routes.filter((route) => native.has(route));
const missing = routes.filter((route) => !native.has(route));
const percentage = routes.length ? (covered.length / routes.length) * 100 : 0;

console.log(`9Router Rust ${manifest.version} route audit`);
console.log(`upstream: ${manifest.upstreamCommit}`);
console.log(
  `native route files: ${covered.length}/${routes.length} (${percentage.toFixed(1)}%)`,
);
if (missing.length) {
  console.log('\nNot yet native Rust:');
  for (const route of missing) console.log(`  - ${route}`);
}
if (manifest.knownNonParity?.length) {
  console.log('\nKnown semantic/non-route gaps:');
  for (const gap of manifest.knownNonParity) console.log(`  - ${gap}`);
}

const hasGaps = missing.length || manifest.knownNonParity?.length;
process.exitCode =
  hasGaps && process.env.NINEROUTER_ALLOW_PARITY_GAPS !== '1' ? 2 : 0;
