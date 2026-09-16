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
- `/v1/api/chat` Ollama response formatting;
- `/v1beta/models/<model>:generateContent` Gemini generation.

These delegated responses are marked `x-9router-runtime: upstream-compat` and are never counted as native coverage. The public `/v1/audio/voices` listing and the dashboard TTS voice pickers are native in every mode: Rust queries the Bing read-aloud catalog, the ElevenLabs/Deepgram/Inworld APIs with the stored connection key, and the host OS voice list directly, with no nested internal HTTP hop. `/v1/models`, `/v1/models/{kind}`, `/v1/models/{provider}/{model}`, `/v1/models/info`, the `/v1beta/models` Gemini list, and `/v1/responses/compact` (which targets the Codex `/compact` upstream path) are native too. `POST /v1/search` and `POST /v1/web/fetch` are native in every mode as well: Rust resolves the provider from the catalog, builds each provider request (auth headers, query strings, bodies), normalises the results, guards every outbound URL with the ported SSRF rules, and reproduces the upstream error envelopes. The chat-based search path (`searchViaChat` providers) is native as well: Rust reads each provider's registry `searchViaChat` block for the endpoint template and default model, replays the upstream request shapes (`google_search`/`googleSearch` grounding tools, the OpenAI-style `web_search` tool, Kimi's `$web_search` builtin tool, the Responses-API `input` bodies), extracts answers and citations exactly like the upstream parsers, and reproduces the upstream `401`/`502`/`504` failure envelopes. The `/v1/videos/**` family is native in both modes: `POST /v1/videos/{generations,edits,extensions}` and `GET /v1/videos/{id}` resolve the provider from the catalog (`xai`, `openrouter` and `vertex` own a `videoConfig`), forward the untouched body with the xAI default shape or the OpenRouter/Vertex adapters, echo `x-9router-connection-id`, rotate connections on `401`/`403`/`429` for creations, and return the upstream status with a sanitised `[provider] <body>` message when the provider rejects the call.

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
- OpenAI/Anthropic-style `/v1/messages/count_tokens` token estimation;
- native `/v1/audio/voices` listing and the dashboard TTS voice pickers (edge-tts, local-device, ElevenLabs, Deepgram, Inworld);
- `/v1/models` full listing (connections, combos, custom models, aliases, disabled filtering, capabilities, kind slugs, single-model lookup) and `/v1/models/info` metadata (alias resolution, kind mapping, TTS `voicesUrl`, virtual `search`/`fetch` models);
- the `/v1beta/models` Gemini-format model list;
- `/v1/responses/compact` (Codex `/compact` upstream target);
- `POST /v1/search` for the twelve dedicated `searchConfig` providers (serper, brave-search, exa, tavily, google-pse, linkup, searchapi, youcom, searxng, xquik, ollama-search, glm) with upstream request construction, result normalisation, the ported SSRF guard, and the upstream error envelopes, plus the nine chat-based `searchViaChat` providers (antigravity, gemini, kimi, minimax, openai, perplexity, perplexity-agent, vercel-ai-gateway, xai) with the upstream chat request shapes, answer/citation extraction and the same error mapping;
- `POST /v1/web/fetch` for the five `fetchConfig` providers (firecrawl, jina-reader, tavily, exa, ollama) with the upstream response payload (`content`, `metadata`, `usage`, `metrics`, optional `links`);
- the `/v1/videos/**` family: `POST /v1/videos/generations`, `POST /v1/videos/extensions`, `POST /v1/videos/edits` and `GET /v1/videos/{id}` with the upstream provider resolution (`xai` default, `getModelInfo` alias/combo/prefix handling, `videoConfig` gate), byte-preserving body forwarding, the xAI/OpenRouter/Vertex request shapes, the Vertex service-account token mint and operation normalisation, the connection rotation rules for creations, the `x-9router-connection-id` echo header, and the `[provider] <sanitised body>` error envelopes;
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
- native video generation/status adapters for the `xai`, `openrouter` and `vertex` wire formats are implemented, but the provider-account cooldown bookkeeping around them (`markAccountUnavailable` status/error fields, model locks, `allRateLimited` answers), `getProviderCredentials` strategy selection (round-robin/fill-first sticky counters, proxy pools, `noAuth` virtual connections) and the `401`/`403` refresh-and-retry branch (`refreshTokenByProvider`; the Vertex service-account mint is native) are not;
- the chat-based web search path (`searchViaChat` providers) is native, with two deliberate deviations: `openai` and `vercel-ai-gateway` resolve their chat-search URL from the provider chat transport (their registry entries carry no `searchViaChat.endpoint`, where upstream's `searchEndpoint()` builds an empty string and the call cannot be made at all), and the provider-account cooldown/backoff bookkeeping around the route is still not persisted (the native route walks the active connections and returns the last error);
- live model discovery for `qoder`, `github` (Copilot), `cursor` and `zed` (Rust serves those providers from the static catalog), plus the provider token-refresh step upstream runs inside the `kiro`/`grok-cli` model resolvers on a rejected token;
- the additive models.dev catalog layer in capability resolution and per-connection proxy routing during model discovery;
- the relative position of the eight TTS-catalog provider keys inside the `GET /v1/models/tts` listing: the id set is identical, but only the registry-derived provider order is reproducible from the exported catalog;
- native model filtering/metadata for `qoder`/`github`/`cursor`/`zed` connections, Ollama, and complete Gemini route adapters, plus Gemini/MiniMax voice listing;
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
