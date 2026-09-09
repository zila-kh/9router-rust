# Architecture

## Final backend-only target

```text
browser / CLI / SDK
        |
        v
Rust public listener :20128
  |-- /api/*        -> Rust management/auth/DB/system services
  |-- /v1/*         -> Rust gateway
  |-- /v1beta/*     -> Rust Gemini compatibility
  |-- /responses    -> Rust Responses compatibility
  |-- /codex/*      -> Rust Responses compatibility
  `-- UI paths      -> proxy to Next UI on loopback
                           |
                           v
                    Next/React :20129
                    (UI rendering only)
```

Rust accepts the real TCP connection, so local-only authorization and forwarding-header trust can be enforced at the same boundary that owns the API.

## Compatibility design

The port preserves these contracts rather than redesigning the application:

- same browser paths and same-origin `/api/...` fetches;
- same SQLite file and table/JSON-column layout;
- same provider/model catalog exported from the pinned upstream source at build time;
- OpenAI, Claude, Gemini, and Responses request/response normalization;
- SSE protocol generation/reduction;
- model aliases, custom provider nodes, combos, multi-account fallback;
- provider-specific binary transports are isolated from the generic JSON executor.

## Hybrid parity mode

`run-hybrid.sh` starts the frozen upstream server privately and configures Rust with `NINEROUTER_LEGACY_BACKEND_ORIGIN`. Rust still owns the public socket. Only native gaps can fall through to the private server, and responses are labeled `x-9router-runtime: legacy-bridge`.

Hybrid mode is a migration/test mechanism, not evidence of 100% Rust parity.
