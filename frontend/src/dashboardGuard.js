import { NextResponse } from "next/server";
import { getSettings, validateApiKey } from "@/lib/localDb";
import { getConsistentMachineId } from "@/shared/utils/machineId";
import { verifyDashboardAuthToken } from "@/lib/auth/dashboardSession";
import { hasTrustedPeerHeaders } from "@/lib/auth/trustedPeer";

const CLI_TOKEN_HEADER = "x-9r-cli-token";
const CLI_TOKEN_SALT = "9r-cli-auth";

let cachedCliToken = null;
async function getCliToken() {
  if (!cachedCliToken) cachedCliToken = await getConsistentMachineId(CLI_TOKEN_SALT);
  return cachedCliToken;
}

async function hasValidCliToken(request) {
  const token = request.headers.get(CLI_TOKEN_HEADER);
  if (!token) return false;
  return token === await getCliToken();
}

// Public API routes use an exact method+path allow-list. LLM routes are handled
// separately because they require their own API key for non-local requests.
const PUBLIC_API_ROUTES = new Set([
  "GET /api/health",
  "GET /api/init",
  "POST /api/locale",
  "POST /api/auth/login",
  "POST /api/auth/logout",
  "GET /api/auth/status",
  "GET /api/auth/oidc/start",
  "GET /api/auth/oidc/callback",
  "GET /api/auth/saml/start",
  "POST /api/auth/saml/acs",
  "GET /api/auth/saml/metadata",
  "GET /api/version",
  "GET /api/settings/require-login",
]);

// Public top-level prefixes (LLM API endpoints with their own API key auth).
// Keep root-level rewrites here too: middleware runs before Next.js rewrites.
const PUBLIC_PREFIXES = ["/v1", "/v1beta", "/api/v1", "/api/v1beta", "/codex", "/responses"];

// Always require JWT token regardless of requireLogin setting.
const ALWAYS_PROTECTED = [
  "/api/shutdown",
  "/api/settings/database",
  "/api/version/shutdown",
  "/api/version/update",
  "/api/oauth/cursor/auto-import",
  "/api/oauth/kiro/auto-import",
  "/api/oauth/xiaomi-mimo/auto-import",
];

// Routes that spawn child processes or read host secrets — restrict to localhost.
const LOCAL_ONLY_PATHS = [
  "/api/cli-tools/",
  "/api/mcp/",
  "/api/tunnel/",
  "/api/oauth/cursor/auto-import",
  "/api/oauth/kiro/auto-import",
  "/api/oauth/xiaomi-mimo/auto-import",
  "/api/auth/reset-password",
  "/api/headroom/",
  "/api/pxpipe/",
  "/api/shutdown",
  "/api/version/shutdown",
  "/api/version/update",
];

const LOCAL_ONLY_OAUTH_ACTIONS = new Set([
  "ide-status",
  "manual-code",
  "poll-status",
  "register-session",
  "start-proxy",
  "stop-proxy",
]);

const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "::1"]);

// Accepts a Host header, a URL hostname, or a raw socket address. Splitting on
// the first colon is invalid for IPv6 and IPv4-mapped IPv6 addresses.
function isLoopbackHostname(value) {
  if (!value) return false;
  let name = String(value).trim().toLowerCase();
  if (name.startsWith("[")) {
    const end = name.indexOf("]");
    if (end === -1) return false;
    name = name.slice(1, end);
  } else if (name.indexOf(":") !== -1 && name.indexOf(":") === name.lastIndexOf(":")) {
    name = name.slice(0, name.indexOf(":"));
  }
  if (name.startsWith("::ffff:")) name = name.slice(7);
  return LOOPBACK_HOSTS.has(name);
}

function isLoopbackPeer(request) {
  if (hasTrustedPeerHeaders(request)) {
    return isLoopbackHostname(request.headers.get("x-9r-real-ip"));
  }
  // Bare `next dev` forks its server, so the wrapper never loads and no peer
  // address reaches us. Host is spoofable, so this stays confined to development.
  if (process.env.NODE_ENV === "development") {
    return isLoopbackHostname(request.headers.get("host"));
  }
  return false;
}

export function isLocalRequest(request) {
  // Stamped by custom-server.js when the socket peer is a proxy rather than the
  // end user. A loopback proxy hop must never grant local-only privileges.
  if (request.headers.get("x-9r-via-proxy")) return false;
  if (!isLoopbackPeer(request)) return false;
  const origin = request.headers.get("origin");
  if (origin) {
    try {
      if (!isLoopbackHostname(new URL(origin).hostname)) return false;
    } catch {
      return false;
    }
  }
  return true;
}

function canonicalPath(pathname) {
  if (typeof pathname !== "string" || !pathname.startsWith("/")) throw new Error("Invalid path");
  if (/%(?![0-9a-f]{2})/i.test(pathname)) throw new Error("Invalid path escape");
  const path = pathname.replace(/%([0-9a-f]{2})/gi, (escape, hex) => {
    const byte = Number.parseInt(hex, 16);
    if (byte < 32 || byte === 127 || byte === 92) throw new Error("Invalid path character");
    const char = String.fromCharCode(byte);
    return /[A-Za-z0-9._~-]/.test(char) ? char : escape.toUpperCase();
  });
  if (/[\\\x00-\x1f\x7f]/.test(path) || path.includes("//")
      || path.split("/").some((part) => part === "." || part === "..")) {
    throw new Error("Ambiguous path");
  }
  return path === "/" ? path : path.replace(/\/+$/, "");
}

function isPublicLlmApi(pathname) {
  return PUBLIC_PREFIXES.some((prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`));
}

function isLocalOnlyPath(pathname) {
  const normalized = String(pathname || "").replace(/\/+$/, "") || "/";
  const matchesStaticPath = LOCAL_ONLY_PATHS.some((prefix) => {
    const root = prefix.endsWith("/") ? prefix.slice(0, -1) : prefix;
    return normalized === root || normalized.startsWith(prefix);
  });
  if (matchesStaticPath) return true;

  const match = normalized.match(/^\/api\/oauth\/([^/]+)\/([^/]+)$/);
  if (!match) return false;
  const [, provider, action] = match;
  if (LOCAL_ONLY_OAUTH_ACTIONS.has(action)) return true;
  return provider === "xiaomi-mimo" && (action === "authorize" || action === "exchange");
}

function extractApiKey(request) {
  const authHeader = request.headers.get("Authorization");
  if (authHeader) {
    const match = authHeader.trim().match(/^Bearer\s+(\S+)$/i);
    if (match) return match[1];
  }
  const apiKeyHeader = request.headers.get("x-api-key");
  if (apiKeyHeader) return apiKeyHeader.trim();
  const googleApiKeyHeader = request.headers.get("x-goog-api-key");
  if (googleApiKeyHeader) return googleApiKeyHeader.trim();
  return request.nextUrl.searchParams?.get("key")?.trim() || null;
}

async function hasValidApiKey(request) {
  const apiKey = extractApiKey(request);
  if (!apiKey) return false;
  return await validateApiKey(apiKey);
}

async function canAccessPublicLlmApi(request) {
  if (await hasValidCliToken(request)) return true;
  if (await hasValidApiKey(request)) return true;
  if (!isLocalRequest(request)) return false;
  const settings = await loadSettings();
  return settings?.requireApiKey === false;
}

async function canAccessLocalOnlyRoute(request) {
  if (await hasValidCliToken(request)) return true;
  if (isLocalRequest(request) && await isAuthenticated(request)) return true;
  return false;
}

async function hasValidToken(request) {
  const token = request.cookies.get("auth_token")?.value;
  return await verifyDashboardAuthToken(token);
}

// Read settings directly from DB to avoid a self-fetch deadlock in middleware.
async function loadSettings() {
  try {
    return await getSettings();
  } catch {
    return null;
  }
}

async function isAuthenticated(request) {
  if (await hasValidToken(request)) return true;
  const settings = await loadSettings();
  return settings?.requireLogin === false;
}

function isPublicApi(request) {
  const pathname = canonicalPath(request.nextUrl.pathname);
  const method = request.method.toUpperCase();
  if (PUBLIC_API_ROUTES.has(`${method} ${pathname}`)) return true;
  return method === "OPTIONS"
    && [...PUBLIC_API_ROUTES].some((route) => route.endsWith(` ${pathname}`));
}

function requestHostname(request) {
  const authority = hasTrustedPeerHeaders(request)
    ? request.headers.get("x-forwarded-host") || request.headers.get("host") || ""
    : request.headers.get("host") || "";
  if (!authority) return "";
  try {
    return new URL(`http://${authority}`).hostname.toLowerCase();
  } catch {
    return "";
  }
}

export const __test__ = {
  canonicalPath,
  isLocalRequest,
  isPublicLlmApi,
  isLocalOnlyPath,
  extractApiKey,
  canAccessPublicLlmApi,
  canAccessLocalOnlyRoute,
  isPublicApi,
  requestHostname,
};

export async function proxy(request) {
  let pathname;
  try {
    pathname = canonicalPath(request.nextUrl.pathname);
  } catch {
    return NextResponse.json({ error: "Invalid or ambiguous request path" }, {
      status: 400, headers: { "Cache-Control": "no-store" },
    });
  }

  if (isLocalOnlyPath(pathname)) {
    if (!(await canAccessLocalOnlyRoute(request))) {
      return NextResponse.json({ error: "Local only: CLI token required" }, { status: 403 });
    }
  }

  if (ALWAYS_PROTECTED.some((prefix) => pathname.startsWith(prefix))) {
    if (await hasValidCliToken(request) || await hasValidToken(request)) {
      return NextResponse.next();
    }
    return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
  }

  if (isPublicLlmApi(pathname)) {
    if (request.method.toUpperCase() === "OPTIONS") return NextResponse.next();
    if (await canAccessPublicLlmApi(request)) return NextResponse.next();
    return NextResponse.json({ error: "API key required for remote API access" }, { status: 401 });
  }

  // Deny by default for /api/*: only the exact public allow-list bypasses auth.
  if (pathname.startsWith("/api/")) {
    if (isPublicApi(request)) return NextResponse.next();
    if (await hasValidCliToken(request) || await isAuthenticated(request)) {
      return NextResponse.next();
    }
    return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
  }

  if (pathname.startsWith("/dashboard")) {
    let requireLogin = true;
    let tunnelDashboardAccess = true;

    try {
      const settings = await loadSettings();
      if (settings) {
        requireLogin = settings.requireLogin !== false;
        tunnelDashboardAccess = settings.tunnelDashboardAccess === true;

        if (!tunnelDashboardAccess) {
          const host = requestHostname(request);
          const tunnelHost = settings.tunnelUrl
            ? new URL(settings.tunnelUrl).hostname.toLowerCase()
            : "";
          const tailscaleHost = settings.tailscaleUrl
            ? new URL(settings.tailscaleUrl).hostname.toLowerCase()
            : "";
          if ((tunnelHost && host === tunnelHost) || (tailscaleHost && host === tailscaleHost)) {
            return NextResponse.redirect(new URL("/login", request.url));
          }
        }
      }
    } catch {
      // On error, keep the secure defaults.
    }

    if (!requireLogin) return NextResponse.next();

    const token = request.cookies.get("auth_token")?.value;
    if (token && await verifyDashboardAuthToken(token)) {
      return NextResponse.next();
    }
    return NextResponse.redirect(new URL("/login", request.url));
  }

  if (pathname === "/") {
    return NextResponse.redirect(new URL("/dashboard", request.url));
  }

  return NextResponse.next();
}
