# Parity status

The machine-readable source of truth for native Rust coverage is `rust-backend/parity/routes.json`. The upstream route inventory is generated from pinned 9Router `17c4cc76877bd1755030a8414f8d0083f48dcccf` (`0.5.75`).

## Two runtime modes

### Compatibility mode

`NINEROUTER_COMPAT_API=1` is the default used by `scripts/run-dev.sh` and `scripts/run-prod.sh`.

Rust remains the only public listener. For dashboard `/api/**` requests it:

1. keeps health, login, logout, session status, password reset, and parity reporting native;
2. mirrors upstream public, protected, always-protected, and local-only route gates;
3. validates dashboard sessions and upstream-compatible `x-9r-cli-token` credentials before protected requests reach the internal server;
4. delegates other dashboard APIs to the pinned upstream handlers so current pages retain their exact response contracts;
5. injects a private `x-9router-ui-secret` header;
6. labels delegated responses `x-9router-runtime: upstream-compat`.

The Next server rejects backend paths without the matching secret. Native Rust still owns the core chat-completions, Claude messages, Responses, embeddings, speech, transcription, image-generation, and Codex paths. Compatibility mode maps public endpoints whose current upstream contracts are not yet native, including:

- `/v1` and the upstream-compatible `/v1/v1` model-list alias;
- `/v1/models/**` filtering, metadata, and single-model lookup;
- `/v1/messages/count_tokens`;
- `/v1/audio/voices`;
- `/v1/api/chat` Ollama response formatting;
- `/v1/responses/compact`;
- `/v1beta/models/**` Gemini listing and native request behavior;
- `/v1/videos/**`, `/v1/search/**`, and `/v1/web/**`.

These delegated responses are marked `x-9router-runtime: upstream-compat` and are never counted as native coverage. The audio-voices adapter also authenticates its nested internal voice-catalog request with the per-process secret, so the frontend boundary remains closed to direct callers.

Host-sensitive operations such as MCP, tunnel control, CLI configuration, OAuth auto-import, and Headroom controls require either a valid CLI token or an authenticated direct-loopback request. Forwarded-peer headers prevent a reverse-proxy hop from being mistaken for a local user. Always-protected update, shutdown, database, and auto-import operations require a valid dashboard session or CLI token even when normal dashboard login is disabled. SSO login, callback, assertion-consumer, and metadata endpoints are public, while OIDC/SAML diagnostic test endpoints remain protected.

Using the upstream handler for management routes avoids partial-native response-shape drift and immediately restores newly added dashboard endpoints. Selective public-API compatibility also restores current upstream behavior while native Rust adapters are completed.

### Strict native mode

`NINEROUTER_COMPAT_API=0` and `NINEROUTER_DISABLE_LEGACY_BRIDGE=1` prohibit all fallback behavior. `scripts/run-full-stack-strict.sh` and `scripts/full-stack-smoke-v2.sh` exercise this mode.

Strict mode is the only mode that should be used to claim native parity. In strict mode, every compatibility-only endpoint returns a Rust error or the current native implementation instead of reaching Next.

## Implemented natively

Core pieces include:

- Rust public HTTP server and loopback Next UI reverse proxy;
- dashboard password sessions, upstream `x-9r-cli-token` validation, and API-key gates;
- public/protected/always-protected/local-only route classification with origin and forwarded-peer checks;
- existing SQLite schema/settings/connections/API keys/combos/KV/provider nodes/proxy pools/usage rows;
- provider/model catalog exported from the pinned upstream source;
- `/v1/models`, chat/completions, Claude messages, Responses, and Gemini compatibility foundations;
- OpenAI/Claude/Gemini/Responses translation layer;
- combo fallback and multi-account fallback;
- generic OpenAI/Claude/Gemini HTTP transports;
- Kiro AWS EventStream executor foundation;
- CommandCode NDJSON executor foundation;
- AWS EventStream, protobuf-wire, ConnectRPC envelope, gRPC-Web, and SSE codecs;
- native embeddings for OpenAI-compatible and Gemini providers;
- OpenAI-style TTS, STT multipart, and image-generation adapters where the provider catalog exposes compatible media endpoints;
- core settings/providers/provider-nodes/proxy-pools/API-keys/combos/model-alias/custom-model/usage management routes in strict native mode.

## Declared gaps blocking a 100%-native release

The manifest intentionally declares these classes of non-parity:

- interactive OAuth flows and provider-specific refresh/retry quirks;
- SAML/OIDC login endpoints;
- full Cursor AgentService/tool protocol;
- full Windsurf gRPC-Web executor;
- Kiro integrity repair/retry edge cases;
- true event-by-event cross-format streaming where formats differ;
- MITM certificate/DNS manager;
- CLI-tool host configuration APIs;
- Headroom and PXPIPE lifecycle/management APIs;
- Tailscale/cloudflared tunnel management;
- MCP endpoints;
- native video generation/status/download/cancel adapters;
- native search and web-fetch adapters;
- native model filtering/metadata, token-counting, Ollama, compact-response, voice-listing, and complete Gemini route adapters;
- remaining specialized media adapters and proxy-pool deployment/test subroutes.

These features are available through the pinned upstream handlers where applicable in compatibility mode, but remain real Rust-port gaps.

## How native parity is measured

```bash
node scripts/audit-parity.mjs
```

The audit compares every pinned upstream `src/app/api/**/route.js` against the native manifest, prints missing route files and semantic gaps, and exits non-zero unless `NINEROUTER_ALLOW_PARITY_GAPS=1` is set.

The release gate deliberately remains strict:

```bash
./scripts/release-gate.sh .
```

Compatibility coverage must never be counted as native Rust coverage.
