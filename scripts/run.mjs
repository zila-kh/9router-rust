#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "..");
const isWin = process.platform === "win32";
const cargoCmd = isWin ? "cargo.exe" : "cargo";

function runNpmSync(args, options = {}) {
  if (isWin) {
    return spawnSync("cmd.exe", ["/c", "npm", ...args], options);
  }
  return spawnSync("npm", args, options);
}

function getArg(name, defaultValue) {
  const idx = process.argv.indexOf(`--${name}`);
  if (idx !== -1 && process.argv[idx + 1]) {
    return process.argv[idx + 1];
  }
  return defaultValue;
}

function hasFlag(name) {
  return process.argv.includes(`--${name}`);
}

const command = process.argv[2] || "dev";

if (["--help", "-h", "help"].includes(command)) {
  console.log(`
9Router Rust Runner

Usage:
  node scripts/run.mjs [command] [options]

Commands:
  dev       Start the development stack (Next dev + Rust debug) [default]
  start     Start the production stack (Next standalone + Rust release)
  prod      Alias for start
  build     Build both frontend and Rust backend
  test      Run test suite

Options:
  --port <port>        Public port (default: 20130, env: PORT)
  --ui-port <port>     Internal UI port (default: 20129, env: NINEROUTER_UI_PORT)
  --host <host>        Bind host (default: 127.0.0.1, env: NINEROUTER_HOST)
  --data-dir <path>    Data directory (env: NINEROUTER_DATA_DIR or DATA_DIR)
`);
  process.exit(0);
}

function checkExecutable(cmd, name) {
  let result;
  if (isWin && name === "npm") {
    result = runNpmSync(["--version"], { stdio: "ignore" });
  } else {
    result = spawnSync(cmd, ["--version"], { stdio: "ignore" });
  }
  if (result.error || result.status !== 0) {
    console.error(`error: ${name} is required but was not found on PATH`);
    process.exit(127);
  }
}

function killProcess(child) {
  if (!child || child.killed || child.exitCode !== null) return;
  if (isWin) {
    try {
      spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
    } catch {}
  } else {
    try {
      child.kill("SIGTERM");
    } catch {}
  }
}

function waitReady(name, url, child, timeoutSec = 90) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutSec * 1000;
    const interval = setInterval(() => {
      if (child.exitCode !== null) {
        clearInterval(interval);
        return reject(new Error(`${name} exited before becoming ready (status ${child.exitCode})`));
      }
      if (Date.now() > deadline) {
        clearInterval(interval);
        return reject(new Error(`Timed out waiting for ${name} at ${url} after ${timeoutSec}s`));
      }
      const req = http.get(url, (res) => {
        if (res.statusCode >= 200 && res.statusCode < 400) {
          clearInterval(interval);
          resolve();
        }
      });
      req.on("error", () => {});
      req.setTimeout(2000, () => req.destroy());
    }, 500);
  });
}

async function runBuild() {
  checkExecutable(cargoCmd, "cargo");
  checkExecutable("npm", "npm");

  console.log("==> Building Rust release backend...");
  const cargoRes = spawnSync(
    cargoCmd,
    ["build", "--release", "--locked", "--manifest-path", "rust-backend/Cargo.toml"],
    { cwd: ROOT, stdio: "inherit" }
  );
  if (cargoRes.status !== 0) {
    throw new Error(`cargo build failed with exit code ${cargoRes.status}`);
  }

  const frontendDir = path.join(ROOT, "frontend");
  if (!fs.existsSync(path.join(frontendDir, "node_modules"))) {
    console.log("==> Installing frontend dependencies...");
    const installArg = fs.existsSync(path.join(frontendDir, "package-lock.json")) ? "ci" : "install";
    const npmInstall = runNpmSync([installArg], { cwd: frontendDir, stdio: "inherit" });
    if (npmInstall.status !== 0) {
      throw new Error(`npm ${installArg} failed with exit code ${npmInstall.status}`);
    }
  }

  console.log("==> Building Next.js frontend...");
  const buildEnv = {
    ...process.env,
    NINEROUTER_UI_ONLY: "1",
    NINEROUTER_COMPAT_API: "1",
    NEXT_TELEMETRY_DISABLED: "1",
    NINEROUTER_UI_SECRET: process.env.NINEROUTER_UI_SECRET || crypto.randomBytes(32).toString("hex"),
  };
  const npmBuild = runNpmSync(["run", "build"], {
    cwd: frontendDir,
    env: buildEnv,
    stdio: "inherit",
  });
  if (npmBuild.status !== 0) {
    throw new Error(`frontend build failed with exit code ${npmBuild.status}`);
  }
  console.log("==> Build finished successfully.");
}

async function runTest() {
  checkExecutable(cargoCmd, "cargo");
  console.log("==> Running Rust test suite...");
  const cargoTest = spawnSync(
    cargoCmd,
    ["test", "--locked", "--manifest-path", "rust-backend/Cargo.toml", "--all-targets"],
    { cwd: ROOT, stdio: "inherit" }
  );
  if (cargoTest.status !== 0) {
    process.exit(cargoTest.status || 1);
  }
}

async function startStack(isDev) {
  checkExecutable(cargoCmd, "cargo");
  checkExecutable("npm", "npm");

  const appPort = parseInt(getArg("port", process.env.PORT || "20130"), 10);
  const uiPort = parseInt(getArg("ui-port", process.env.NINEROUTER_UI_PORT || "20129"), 10);
  const host = getArg("host", process.env.NINEROUTER_HOST || "127.0.0.1");

  if (isNaN(appPort) || appPort < 1 || appPort > 65535) {
    console.error(`error: invalid public port: ${appPort}`);
    process.exit(2);
  }
  if (isNaN(uiPort) || uiPort < 1 || uiPort > 65535) {
    console.error(`error: invalid UI port: ${uiPort}`);
    process.exit(2);
  }
  if (appPort === uiPort) {
    console.error(`error: public port (${appPort}) and internal UI port (${uiPort}) must be different`);
    process.exit(2);
  }

  const expectedUiOrigin = `http://127.0.0.1:${uiPort}`;
  const uiSecret = process.env.NINEROUTER_UI_SECRET || crypto.randomBytes(32).toString("hex");

  const commonEnv = {
    ...process.env,
    NINEROUTER_UI_ONLY: "1",
    NINEROUTER_COMPAT_API: process.env.NINEROUTER_COMPAT_API || "1",
    NINEROUTER_UI_PORT: String(uiPort),
    NINEROUTER_UI_ORIGIN: expectedUiOrigin,
    NINEROUTER_HOST: host,
    PORT: String(appPort),
    NINEROUTER_DISABLE_LEGACY_BRIDGE: "1",
    NEXT_TELEMETRY_DISABLED: "1",
    NINEROUTER_UI_SECRET: uiSecret,
  };
  delete commonEnv.NINEROUTER_LEGACY_BACKEND_ORIGIN;
  delete commonEnv.LEGACY_BACKEND_ORIGIN;

  const dataDir = getArg("data-dir", process.env.NINEROUTER_DATA_DIR || process.env.DATA_DIR);
  if (dataDir) {
    commonEnv.NINEROUTER_DATA_DIR = dataDir;
    commonEnv.DATA_DIR = dataDir;
  }

  const frontendDir = path.join(ROOT, "frontend");
  if (!fs.existsSync(path.join(frontendDir, "node_modules"))) {
    console.log("==> Installing frontend dependencies...");
    const installArg = fs.existsSync(path.join(frontendDir, "package-lock.json")) ? "ci" : "install";
    const res = runNpmSync([installArg], { cwd: frontendDir, stdio: "inherit" });
    if (res.status !== 0) throw new Error("npm install failed");
  }

  let rustExe;
  if (isDev) {
    console.log("==> Building Rust debug binary...");
    const res = spawnSync(
      cargoCmd,
      ["build", "--locked", "--manifest-path", "rust-backend/Cargo.toml"],
      { cwd: ROOT, stdio: "inherit" }
    );
    if (res.status !== 0) throw new Error("cargo build failed");
    rustExe = path.join(ROOT, "rust-backend", "target", "debug", isWin ? "9router-rust.exe" : "9router-rust");
  } else {
    rustExe = path.join(ROOT, "rust-backend", "target", "release", isWin ? "9router-rust.exe" : "9router-rust");
    const buildId = path.join(frontendDir, ".next", "BUILD_ID");
    if (!fs.existsSync(rustExe) || !fs.existsSync(buildId)) {
      console.log("==> Missing release artifacts; building...");
      await runBuild();
    }
  }

  if (!fs.existsSync(rustExe)) {
    throw new Error(`Rust binary not found at ${rustExe}`);
  }

  let uiProc = null;
  let rustProc = null;
  let shuttingDown = false;

  const cleanup = () => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log("\n==> Shutting down 9Router stack...");
    killProcess(rustProc);
    killProcess(uiProc);
    process.exit(0);
  };

  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);
  process.on("exit", cleanup);

  console.log(`==> Starting ${isDev ? "development" : "production"} UI on ${expectedUiOrigin}...`);
  if (isDev) {
    uiProc = spawn(
      process.execPath,
      ["node_modules/next/dist/bin/next", "dev", "--webpack", "--hostname", "127.0.0.1", "--port", String(uiPort)],
      { cwd: frontendDir, env: commonEnv, stdio: "inherit" }
    );
  } else {
    uiProc = spawn(
      process.execPath,
      [".next/standalone/custom-server.js"],
      { cwd: frontendDir, env: commonEnv, stdio: "inherit" }
    );
  }

  uiProc.on("error", (err) => {
    console.error("UI process error:", err);
    cleanup();
  });

  await waitReady("Internal Next UI", `${expectedUiOrigin}/login`, uiProc, 90);
  console.log("==> Internal UI is ready.");

  console.log(`==> Starting Rust backend on http://${host}:${appPort}...`);
  rustProc = spawn(rustExe, [], { cwd: ROOT, env: commonEnv, stdio: "inherit" });

  rustProc.on("error", (err) => {
    console.error("Rust backend process error:", err);
    cleanup();
  });

  await waitReady("Rust backend", `http://${host === "0.0.0.0" ? "127.0.0.1" : host}:${appPort}/api/health`, rustProc, 90);

  const publicUrl = `http://${host === "0.0.0.0" ? "127.0.0.1" : host}:${appPort}`;
  console.log("\n" + "=".repeat(60));
  console.log(`  9Router Rust ${isDev ? "development" : "production"} stack is running!`);
  console.log(`  Dashboard:  ${publicUrl}/dashboard`);
  console.log(`  Health API: ${publicUrl}/api/health`);
  console.log("=".repeat(60) + "\n");

  const monitor = setInterval(() => {
    if (uiProc.exitCode !== null || rustProc.exitCode !== null) {
      clearInterval(monitor);
      const failed = uiProc.exitCode !== null ? "UI process" : "Rust backend";
      const code = uiProc.exitCode !== null ? uiProc.exitCode : rustProc.exitCode;
      console.error(`\nerror: ${failed} exited unexpectedly with status ${code}`);
      cleanup();
    }
  }, 1000);
}

try {
  switch (command) {
    case "dev":
      await startStack(true);
      break;
    case "start":
    case "prod":
      await startStack(false);
      break;
    case "build":
      await runBuild();
      break;
    case "test":
      await runTest();
      break;
    default:
      console.error(`unknown command: ${command}. Use --help for usage.`);
      process.exit(1);
  }
} catch (error) {
  console.error(`error: ${error.message}`);
  process.exit(1);
}
