const http = require("http");
const path = require("path");
const fs = require("fs");
const crypto = require("crypto");
const { pathToFileURL } = require("url");

if (process.env.NINEROUTER_UI_ONLY === "1") {
  const uiPort = process.env.NINEROUTER_UI_PORT || "20129";
  if (!/^\d+$/.test(uiPort) || Number(uiPort) < 1 || Number(uiPort) > 65535) {
    throw new Error(`Invalid NINEROUTER_UI_PORT: ${uiPort}`);
  }
  // The compatibility listener is private by design. Never honor a public bind
  // address inherited from the Rust process or the user's shell.
  process.env.HOSTNAME = "127.0.0.1";
  process.env.PORT = uiPort;
}

const origCreate = http.createServer.bind(http);
const INTERNAL_SECRET_HEADER = "x-9router-ui-secret";

function timingSafeStringEqual(left, right) {
  const leftBuffer = Buffer.from(String(left || ""));
  const rightBuffer = Buffer.from(String(right || ""));
  return leftBuffer.length === rightBuffer.length && crypto.timingSafeEqual(leftBuffer, rightBuffer);
}

function isLoopbackAddress(value) {
  let address = String(value || "").trim().toLowerCase();
  if (address.startsWith("[") && address.endsWith("]")) address = address.slice(1, -1);
  if (address.startsWith("::ffff:")) address = address.slice(7);
  return address === "127.0.0.1" || address === "::1" || address === "localhost";
}

function trustedForwardedProto(value) {
  const proto = String(value || "").split(",")[0].trim().toLowerCase();
  return proto === "https" ? "https" : "http";
}

function trustedForwardedHost(value) {
  const authority = String(value || "").split(",")[0].trim();
  if (!authority || authority.length > 512) return "";
  try {
    const parsed = new URL(`http://${authority}`);
    return parsed.host;
  } catch {
    return "";
  }
}

// Per-process proof that the peer headers below were stamped by this wrapper.
const PEER_TOKEN = crypto.randomBytes(24).toString("hex");
process.env.NINEROUTER_PEER_TOKEN = PEER_TOKEN;

let backgroundRefreshStarted = false;

function startBackgroundTokenRefreshFromCustomServer() {
  if (backgroundRefreshStarted) return;
  backgroundRefreshStarted = true;
  const modPath = path.join(__dirname, "src", "sse", "services", "backgroundTokenRefresh.js");
  import(pathToFileURL(modPath).href)
    .then((module) => {
      try {
        module.startBackgroundTokenRefresh();
      } catch (error) {
        console.error(
          "[BackgroundTokenRefresh] start failed:",
          error && error.message ? error.message : error,
        );
      }
      const stop = () => {
        try {
          module.stopBackgroundTokenRefresh();
        } catch {
          // Best-effort shutdown.
        }
      };
      process.once("SIGINT", stop);
      process.once("SIGTERM", stop);
    })
    .catch((error) => {
      // Expected in published CLI standalone builds that omit src/.
      if (process.env.DEBUG_BACKGROUND_TOKEN_REFRESH) {
        console.error(
          "[BackgroundTokenRefresh] import failed:",
          error && error.message ? error.message : error,
        );
      }
    });
}

// Wrap Next's HTTP server. The private listener trusts forwarded identity only
// when a loopback Rust proxy presents the shared per-process secret.
http.createServer = (...args) => {
  const handler = args.find((argument) => typeof argument === "function");
  const rest = args.filter((argument) => typeof argument !== "function");
  if (!handler) return origCreate(...args);

  const wrapped = (req, res) => {
    const socketIp = req.socket && req.socket.remoteAddress ? req.socket.remoteAddress : "";
    const configuredSecret = process.env.NINEROUTER_UI_SECRET || "";
    const suppliedSecret = req.headers[INTERNAL_SECRET_HEADER] || "";
    const trustedRustProxy = Boolean(
      isLoopbackAddress(socketIp)
        && configuredSecret
        && timingSafeStringEqual(suppliedSecret, configuredSecret),
    );

    const forwardedFor = trustedRustProxy ? req.headers["x-forwarded-for"] : "";
    const forwardedRealIp = trustedRustProxy ? req.headers["x-real-ip"] : "";
    const forwardedHost = trustedRustProxy
      ? trustedForwardedHost(req.headers["x-forwarded-host"])
      : "";
    const forwardedProto = trustedRustProxy
      ? trustedForwardedProto(req.headers["x-forwarded-proto"])
      : "";
    const proxyIp = forwardedRealIp
      || (forwardedFor ? String(forwardedFor).split(",")[0].trim() : "");
    const ip = proxyIp || socketIp;
    const viaProxy = Boolean(trustedRustProxy && proxyIp && !isLoopbackAddress(proxyIp));

    for (const name of [
      "forwarded",
      "x-forwarded-for",
      "x-forwarded-host",
      "x-forwarded-proto",
      "x-real-ip",
      "cf-connecting-ip",
      "true-client-ip",
      "x-client-ip",
      "x-cluster-client-ip",
      "x-9r-real-ip",
      "x-9r-via-proxy",
      "x-9r-peer-token",
    ]) {
      delete req.headers[name];
    }
    if (!trustedRustProxy) delete req.headers[INTERNAL_SECRET_HEADER];

    if (trustedRustProxy && forwardedHost) req.headers["x-forwarded-host"] = forwardedHost;
    if (trustedRustProxy && forwardedProto) req.headers["x-forwarded-proto"] = forwardedProto;
    req.headers["x-9r-real-ip"] = ip;
    req.headers["x-9r-peer-token"] = PEER_TOKEN;
    if (viaProxy) req.headers["x-9r-via-proxy"] = "1";

    return handler(req, res);
  };

  const server = origCreate(...rest, wrapped);
  server.once("listening", () => {
    startBackgroundTokenRefreshFromCustomServer();
  });

  const origEmit = server.emit;
  // JBR 25 sends h2c upgrades that the HTTP/1.1 server would otherwise close.
  server.emit = function (event, ...eventArgs) {
    const [req, socket, head] = eventArgs;
    if (event !== "upgrade" || String(req.headers.upgrade || "").toLowerCase() !== "h2c") {
      return origEmit.call(this, event, ...eventArgs);
    }

    const contentLength = Number(req.headers["content-length"] || 0);
    if (!Number.isSafeInteger(contentLength) || contentLength < 0) {
      socket.destroy();
      return true;
    }
    const chunks = [head];
    let received = head.length;
    const serve = () => {
      const replay = new http.IncomingMessage(socket);
      Object.assign(replay, {
        method: req.method,
        url: req.url,
        headers: req.headers,
        complete: true,
      });
      if (received) replay.push(Buffer.concat(chunks, received).subarray(0, contentLength));
      replay.push(null);
      const res = new http.ServerResponse(replay);
      res.shouldKeepAlive = false;
      res.assignSocket(socket);
      res.once("finish", () => socket.end());
      Promise.resolve()
        .then(() => wrapped(replay, res))
        .catch((error) => {
          console.error("Failed to downgrade h2c request", error);
          socket.destroy();
        });
    };
    if (received >= contentLength) {
      serve();
    } else {
      socket.on("data", function readBody(chunk) {
        chunks.push(chunk);
        received += chunk.length;
        if (received < contentLength) return;
        socket.off("data", readBody);
        serve();
      });
      socket.resume();
    }
    delete req.headers.upgrade;
    delete req.headers["http2-settings"];
    req.headers.connection = "close";
    return true;
  };
  return server;
};

if (require.main === module) {
  const standalone = path.join(__dirname, "server.js");
  if (fs.existsSync(standalone)) {
    require(standalone);
  } else {
    // A repo checkout has no standalone build next to this file. `next start`
    // still creates its HTTP server in-process, so the wrapper remains active.
    const nextBin = require.resolve("next/dist/bin/next");
    process.argv = [process.argv[0], nextBin, "start", ...process.argv.slice(2)];
    require(nextBin);
  }
}
