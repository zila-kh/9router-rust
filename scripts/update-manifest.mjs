#!/usr/bin/env node
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = path.join(root, "MANIFEST.sha256");
const excluded = new Set([
  "MANIFEST.sha256",
  // This pointer is written by CI after a successful strict run. Including it
  // would create a self-perpetuating status-commit/manifest cycle.
  "ci/strict-status.json",
]);

const tracked = spawnSync(
  "git",
  ["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
  {
    cwd: root,
    encoding: "buffer",
  },
);
if (tracked.status !== 0) {
  process.stderr.write(tracked.stderr || "git ls-files failed\n");
  process.exit(tracked.status || 1);
}

const files = tracked.stdout
  .toString("utf8")
  .split("\0")
  .filter(Boolean)
  .map((file) => file.replaceAll("\\", "/"))
  .filter((file) => !excluded.has(file))
  .sort((left, right) => left.localeCompare(right, "en"));

// Line endings are a checkout artifact rather than content: .gitattributes pins
// only *.sh, so the same commit hashes differently in a Windows working tree
// than in the Linux CI checkout, and a manifest generated on Windows can never
// verify on Linux. Normalize text before hashing and keep anything that is not
// valid UTF-8 byte-exact.
const stableDigest = (file) => {
  const bytes = fs.readFileSync(path.join(root, file));
  let stable = bytes;
  if (!bytes.includes(0)) {
    const text = bytes.toString("utf8");
    if (Buffer.from(text, "utf8").equals(bytes)) {
      stable = Buffer.from(text.replace(/\r\n/g, "\n"), "utf8");
    }
  }
  return crypto.createHash("sha256").update(stable).digest("hex");
};

const generated = files
  .map((file) => `${stableDigest(file)}  ./${file}`)
  .join("\n") + "\n";

if (process.argv.includes("--check")) {
  const current = fs.existsSync(manifestPath)
    ? fs.readFileSync(manifestPath, "utf8").replaceAll("\r\n", "\n")
    : "";
  if (current !== generated) {
    console.error("MANIFEST.sha256 is stale; run: node scripts/update-manifest.mjs");
    process.exit(1);
  }
  console.log(`MANIFEST.sha256: OK (${files.length} tracked files)`);
} else {
  fs.writeFileSync(manifestPath, generated);
  console.log(`Updated MANIFEST.sha256 (${files.length} tracked files)`);
}
