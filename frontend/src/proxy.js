import { NextResponse } from "next/server";

const RUST_BACKEND_PREFIXES = ["/api", "/v1", "/v1beta", "/responses", "/codex"];
const INTERNAL_SECRET_HEADER = "x-9router-ui-secret";

function isBackendPath(pathname) {
  return RUST_BACKEND_PREFIXES.some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),
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
  matcher: ["/((?!_next/static|_next/image|favicon\\.ico).*)"],
};
