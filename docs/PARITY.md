# Parity status

The machine-readable source of truth is `rust-backend/parity/routes.json`.

## Implemented natively in the current package

Core pieces include:

- Rust public HTTP server and loopback Next UI reverse proxy;
- dashboard password session/auth gate and API-key gate;
- existing SQLite schema/settings/connections/API keys/combos/KV/provider nodes/proxy pools/usage rows;
- provider/model catalog exported from the pinned upstream source;
- `/v1/models`, chat/completions, Claude messages, Responses, Gemini compatibility paths;
- OpenAI/Claude/Gemini/Responses translation layer;
- combo fallback and multi-account fallback;
- generic OpenAI/Claude/Gemini HTTP transports;
- Kiro AWS EventStream executor foundation;
- CommandCode NDJSON executor foundation;
- AWS EventStream, protobuf-wire, ConnectRPC envelope, gRPC-Web, and SSE codecs;
- native embeddings for OpenAI-compatible and Gemini providers;
- OpenAI-style TTS, STT multipart, and image-generation adapters when the provider catalog exposes compatible media endpoints;
- core settings/providers/provider-nodes/proxy-pools/API-keys/combos/model-alias/custom-model/usage management routes.

## Declared gaps blocking a 100% release

The current manifest intentionally declares the following classes of non-parity:

- interactive OAuth flows and provider-specific refresh/retry quirks;
- SAML/OIDC login endpoints;
- full Cursor AgentService/tool protocol;
- full Windsurf gRPC-Web executor;
- Kiro integrity repair/retry edge cases;
- true event-by-event cross-format streaming (currently reduced and re-synthesized where formats differ);
- MITM certificate/DNS manager;
- CLI-tool host configuration APIs;
- Headroom and PXPIPE lifecycle/management APIs;
- Tailscale/cloudflared tunnel management;
- MCP endpoints;
- remaining specialized media adapters and proxy-pool deployment/test subroutes.

Because these are real upstream behaviors, calling the current strict mode “100%” would be incorrect. `scripts/release-gate.sh` is designed to prevent that mistake.

## How parity is measured

After installation into the pinned upstream checkout:

```bash
node scripts/audit-parity.mjs
```

The script recursively enumerates every upstream `src/app/api/**/route.js`, compares it to the native route manifest, prints missing route files, prints semantic gaps, and exits non-zero while either set is non-empty.
