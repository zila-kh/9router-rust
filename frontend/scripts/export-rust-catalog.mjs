#!/usr/bin/env node
// Run from the 9Router repository root. This executes only at BUILD TIME.
// The runtime backend is Rust-only; this keeps upstream's provider/model catalog as the data source.
import fs from 'node:fs';
import path from 'node:path';
import REGISTRY from '../open-sse/providers/registry/index.js';
import { PROVIDERS, PROVIDER_MODELS, PROVIDER_OAUTH, PROVIDER_MEDIA } from '../open-sse/providers/index.js';

function clean(value, seen = new WeakSet()) {
  if (value === null || typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return value;
  if (typeof value === 'function' || typeof value === 'undefined' || typeof value === 'symbol') return undefined;
  if (value instanceof RegExp) return { __regex: value.source, __flags: value.flags };
  if (Array.isArray(value)) return value.map(v => clean(v, seen)).filter(v => v !== undefined);
  if (typeof value === 'object') {
    if (seen.has(value)) return undefined;
    seen.add(value);
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      const c = clean(v, seen);
      if (c !== undefined) out[k] = c;
    }
    seen.delete(value);
    return out;
  }
  return undefined;
}

const payload = clean({ registry: REGISTRY, providers: PROVIDERS, models: PROVIDER_MODELS, oauth: PROVIDER_OAUTH, media: PROVIDER_MEDIA });
const out = path.resolve('rust-backend/assets/provider-catalog.json');
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, JSON.stringify(payload, null, 2));
console.log(`wrote ${out} (${payload.registry.length} registry entries)`);
