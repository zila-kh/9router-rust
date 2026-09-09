# Testing

## First pass

From the pinned 9Router checkout after installing the overlay:

```bash
./scripts/test-port.sh .
```

Then run strict mode:

```bash
./scripts/run-dev.sh .
```

Check:

```bash
curl -i http://127.0.0.1:20128/api/health
curl -i http://127.0.0.1:20128/api/version
curl -i http://127.0.0.1:20128/api/rust/parity
```

Dashboard/authenticated endpoints may require the dashboard session cookie. Remote `/v1/*` requests require a configured 9Router API key according to the current settings.

## Provider smoke test

Use a model/account already stored in `~/.9router/db/data.sqlite`:

```bash
curl -i http://127.0.0.1:20128/v1/chat/completions \
  -H 'Authorization: Bearer YOUR_9ROUTER_API_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"model":"PROVIDER/MODEL","messages":[{"role":"user","content":"reply with ok"}]}'
```

Verify `x-9router-runtime: rust`.

Repeat with `"stream": true` and verify SSE framing and `[DONE]` where the caller format requires it.

## Hybrid differential testing

Run:

```bash
./scripts/run-hybrid.sh .
```

Exercise the existing dashboard. For each `/api/*` or LLM response inspect `x-9router-runtime`. Native Rust should increase over time; bridge responses identify exact remaining work without breaking the UI.

For high-risk transports, compare the frozen JS implementation and Rust with identical fixtures/account state and verify HTTP status, relevant headers, wire frames, translated tool calls, finish reasons, usage, DB side effects, retry/fallback choice, and stream termination.

## Release gate

Only consider the backend port 100% when all of these pass:

```bash
./scripts/release-gate.sh .
```

and live-provider tests cover every supported provider transport class on the target operating systems.
