export async function register() {
  if (
    process.env.NINEROUTER_UI_ONLY === "1" &&
    process.env.NINEROUTER_COMPAT_API !== "1"
  ) {
    return;
  }
  if (process.env.NEXT_RUNTIME === "nodejs") {
    // The vendored layout intentionally omits backend side-effect imports. Bring
    // them back only when Rust has enabled the secured compatibility API.
    if (process.env.NINEROUTER_COMPAT_API === "1") {
      await import("@/lib/network/initOutboundProxy");
      await import("@/shared/services/bootstrap");
    }

    const { initConsoleLogCapture } = await import("@/lib/consoleLogBuffer");
    initConsoleLogCapture();

    // Server-only: lets capabilities.js read the synced catalog without pulling
    // node:fs into the dashboard's browser bundle.
    const { installCatalogSource } = await import("open-sse/providers/catalogOverride.js");
    await installCatalogSource();

    const { startModelCatalogSync } = await import("@/lib/modelCatalog/sync.js");
    startModelCatalogSync();
  }
}
