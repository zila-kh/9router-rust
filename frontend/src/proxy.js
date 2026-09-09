import { NextResponse } from "next/server";

const RUST_BACKEND_PREFIXES = ["/api", "/v1", "/v1beta", "/responses", "/codex"];

function isBackendPath(pathname) {
  return RUST_BACKEND_PREFIXES.some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`),
  );
}

export default async function proxy(request) {
  if (process.env.NINEROUTER_UI_ONLY === "1") {
    if (isBackendPath(request.nextUrl.pathname)) {
      return NextResponse.json(
        { error: "Rust backend only", code: "RUST_BACKEND_REQUIRED" },
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
