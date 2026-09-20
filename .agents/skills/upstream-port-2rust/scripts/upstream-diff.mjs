#!/usr/bin/env node
// Survey the upstream 9Router diff for this Rust port.
//
// Prints the commit list, every changed file ordered by churn, and a triage tag
// per file, so a port pass starts from facts instead of re-deriving them from
// `git log` guesses. Reads the pinned commit out of frontend-source.lock.json
// and resolves the newest upstream tag unless told otherwise.
//
//   node scripts/upstream-diff.mjs
//   node scripts/upstream-diff.mjs --from 17c4cc76 --to v0.5.81
//   node scripts/upstream-diff.mjs --json            # machine-readable summary
//
// Requires `gh` (authenticated) for the GitHub API.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const UPSTREAM = "decolua/9router";
const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "..",
);
const isWin = process.platform === "win32";
const ghCmd = isWin ? "gh.exe" : "gh";

function getArg(name) {
  const index = process.argv.indexOf(`--${name}`);
  return index !== -1 && process.argv[index + 1] ? process.argv[index + 1] : null;
}

function gh(endpoint) {
  const result = spawnSync(ghCmd, ["api", endpoint], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
  if (result.status !== 0) {
    const message = (result.stderr || result.stdout || "").trim();
    throw new Error(`gh api ${endpoint} failed: ${message || "unknown error"}`);
  }
  return JSON.parse(result.stdout);
}

function pinnedCommit() {
  const lockPath = path.join(root, "frontend-source.lock.json");
  const lock = JSON.parse(fs.readFileSync(lockPath, "utf8"));
  return { commit: lock.commit, version: lock.upstreamVersion };
}

// Triage: which side of the port a changed file lands on.
function classify(file) {
  if (/^tests\//.test(file)) return "spec";
  if (/^CHANGELOG\.md$|^docs\//.test(file)) return "docs";
  if (/^open-sse\/providers\/(registry|index|models|catalogOverride)/.test(file)) return "data";
  if (/^open-sse\/providers\/(pricing|capabilities)\.js$/.test(file)) return "data";
  if (/^src\/lib\/modelCatalog\//.test(file)) return "data";
  if (/^src\/app\/|^src\/shared\/|^public\/i18n\//.test(file)) return "frontend-only";
  if (
    /^open-sse\/translator\//.test(file) ||
    /^open-sse\/utils\/(streamHelpers|responsesStreamHelpers|stream|usageTracking|sessionManager|error)\.js$/.test(file) ||
    /^open-sse\/services\/(accountFallback|combo|usage)\//.test(file) ||
    /^open-sse\/services\/accountFallback\.js$/.test(file) ||
    /^open-sse\/handlers\//.test(file) ||
    /^open-sse\/executors\//.test(file) ||
    /^open-sse\/config\/runtimeConfig\.js$/.test(file) ||
    /^open-sse\/providers\/(shared|visionPatterns)\.js$/.test(file) ||
    /^src\/sse\//.test(file) ||
    /^src\/lib\/db\/repos\/(usageRepo|pricingRepo)\.js$/.test(file)
  ) {
    return "rust-portable";
  }
  return "frontend-only";
}

const asJson = process.argv.includes("--json");
const from = getArg("from") || pinnedCommit().commit;
const pinnedVersion = pinnedCommit().version;

let to = getArg("to");
if (!to) {
  const tags = gh(`repos/${UPSTREAM}/tags?per_page=20`);
  if (!tags.length) throw new Error("upstream has no tags");
  to = tags[0].name;
}

const compare = gh(`repos/${UPSTREAM}/compare/${from}...${to}`);
const files = (compare.files || []).map((file) => ({
  file: file.filename,
  additions: file.additions,
  deletions: file.deletions,
  churn: file.additions + file.deletions,
  kind: classify(file.filename),
}));
const commits = (compare.commits || []).map((commit) => ({
  sha: commit.sha.slice(0, 8),
  subject: commit.commit.message.split("\n")[0],
}));

if (asJson) {
  console.log(
    JSON.stringify(
      {
        upstream: UPSTREAM,
        from,
        fromVersion: pinnedVersion,
        to,
        ahead: compare.ahead_by,
        totals: {
          commits: commits.length,
          files: files.length,
          additions: files.reduce((sum, file) => sum + file.additions, 0),
          deletions: files.reduce((sum, file) => sum + file.deletions, 0),
        },
        commits,
        files,
      },
      null,
      2,
    ),
  );
  process.exit(0);
}

const byKind = (kind) => files.filter((file) => file.kind === kind);
const line = (label, list) =>
  `${label.padEnd(14)} ${String(list.length).padStart(3)}  ${list
    .reduce((sum, file) => sum + file.churn, 0)
    .toString()
    .padStart(5)} lines changed`;

console.log(`${UPSTREAM}: ${from.slice(0, 8)} (v${pinnedVersion}) -> ${to}`);
console.log(
  `${compare.ahead_by} commits ahead, ${files.length} files, ` +
    `+${files.reduce((sum, file) => sum + file.additions, 0)} / ` +
    `-${files.reduce((sum, file) => sum + file.deletions, 0)} lines\n`,
);
if (compare.behind_by > 0 || commits.some((commit) => commit.sha === pinnedCommit().commit.slice(0, 8))) {
  // A baseline older than this repo's pin pulls in commits that are already
  // ported here, so the lists below overstate what is left to do.
  console.log(
    "note: this range contains the pinned commit, so it lists work already present\n" +
      "      in this port. Compare from the pin (no --from) for what is left.\n",
  );
}

console.log("commits");
for (const commit of commits) console.log(`  ${commit.sha}  ${commit.subject}`);

console.log("\nby triage");
for (const kind of ["rust-portable", "data", "spec", "frontend-only", "docs"]) {
  const list = byKind(kind);
  if (list.length) console.log(line(kind, list));
}

for (const kind of ["rust-portable", "data"]) {
  const list = byKind(kind).sort((left, right) => right.churn - left.churn);
  if (!list.length) continue;
  console.log(`\n${kind} files (by churn)`);
  for (const file of list) {
    console.log(`  ${String(file.additions).padStart(5)}+${String(file.deletions).padEnd(5)}-  ${file.file}`);
  }
}

const specs = byKind("spec");
if (specs.length) {
  console.log("\nspec (read the test before porting the fix)");
  for (const file of specs) console.log(`  ${file.file}`);
}
console.log("\nNext: read commit subjects, then fetch one patch at a time:");
console.log(`  gh api "repos/${UPSTREAM}/commits/<sha>" --jq '.files[] | "\\(.additions)+\\t\\(.filename)"'`);
console.log(`  gh api "repos/${UPSTREAM}/commits/<sha>" --jq '.files[] | select(.filename=="<path>") | .patch'`);
