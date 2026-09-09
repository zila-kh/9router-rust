#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';

const root=process.cwd();
const manifest=JSON.parse(fs.readFileSync(path.join(root,'rust-backend/parity/routes.json'),'utf8'));
const apiRoot=path.join(root,'src/app/api');
function walk(dir,out=[]){
  if(!fs.existsSync(dir)) return out;
  for(const ent of fs.readdirSync(dir,{withFileTypes:true})){
    const p=path.join(dir,ent.name);
    if(ent.isDirectory()) walk(p,out); else if(ent.isFile()&&ent.name==='route.js') out.push(path.relative(path.join(root,'src/app'),p).split(path.sep).join('/'));
  }
  return out;
}
const routes=walk(apiRoot).sort();
const native=new Set(manifest.native);
const covered=routes.filter(r=>native.has(r));
const missing=routes.filter(r=>!native.has(r));
const pct=routes.length?covered.length/routes.length*100:0;
console.log(`9Router Rust ${manifest.version} route audit`);
console.log(`upstream: ${manifest.upstreamCommit}`);
console.log(`native route files: ${covered.length}/${routes.length} (${pct.toFixed(1)}%)`);
if(missing.length){console.log('\nNot yet native Rust (legacy bridge can service these during testing):');for(const r of missing)console.log(`  - ${r}`);}
if(manifest.knownNonParity?.length){console.log('\nKnown semantic/non-route gaps:');for(const g of manifest.knownNonParity)console.log(`  - ${g}`);}
const hasGaps=missing.length||manifest.knownNonParity?.length;
process.exitCode=hasGaps && process.env.NINEROUTER_ALLOW_PARITY_GAPS!=="1" ? 2 : 0;
