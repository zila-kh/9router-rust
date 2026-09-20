# Build status — 2026-09-19

Local release-candidate validation:

- Rust formatting, `cargo check`, and Clippy with `-D warnings`: PASS
- Rust tests (`--locked --all-targets`): PASS (220 tests)
- RustSec audit: PASS (no known vulnerable dependencies)
- Static source audit: PASS
- Release-boundary regression suite: PASS (29 tests)
- Frontend clean install, ESLint error gate, and production build: PASS
- npm production dependency audit: PASS (0 vulnerabilities)
- Local production-stack smoke tests: PASS, including authentication, internal-port isolation, and session persistence across restart
- Live LLM acceptance: PASS for a direct Codex model and the `combo-deep` combo in both non-streaming and streaming modes; responses were served by the Rust runtime and streaming ended with `[DONE]`
- `combo-test` live acceptance: PASS through the Rust runtime using `gpt-5.6-luna`; a deterministic non-streaming probe returned HTTP 200 in 2.43 s. Three longer streaming probes reported 501, 436, and 443 completion tokens with a median 47.61 end-to-end tokens/s. The median first-token latency was 9.53 s because the current Codex Responses translation buffers upstream output before synthesizing Chat Completions SSE.
- A follow-up direct `cx/gpt-5.6-luna` measurement reported median 43.88 end-to-end completion tokens/s and median 8.75 s first-token latency across three streaming runs. Incremental Codex Responses-to-Chat SSE translation is now implemented ([CODEX_STREAMING_PLAN.md](CODEX_STREAMING_PLAN.md), `rust-backend/src/responses_stream.rs`): a Chat Completions caller of a Responses provider receives events as they arrive, a deterministic test proves the first text delta is forwarded before the upstream completion event, and usage is recorded from the terminal event. The live after-benchmark for this path has not been run here (no provider credentials), so the latency figures above are still the pre-change baseline.

Known limitations and required release checks:

- The production build currently reports 19 Turbopack dynamic-filesystem tracing warnings. The build completes, but the warnings should remain visible until their affected routes are made statically traceable.
- The storage-path suite must be run on Linux or WSL. A native Windows invocation currently fails because the shell harness passes WSL-style paths to Windows binaries.
- Strict native parity is 147/156 routes (94.2%). Compatibility mode remains required for the documented route and semantic gaps.
- Run CI on the exact committed release candidate and complete real-provider plus HTTPS reverse-proxy acceptance tests before publishing.
- Saved provider state is not uniformly healthy: `combo-ui-lite` currently exhausts its members because its OpenCode Go credential is invalid and several fallback providers have no active connection. Other saved OAuth accounts also require reauthentication. This does not affect the passing direct/`combo-deep` paths, but those combos must not be advertised as available until their connections pass a live probe.
- `combo-logic` is not currently exercising Gemini. Its first two entries use the Antigravity custom transport, whose dedicated chat executor has not been ported to Rust; the gateway now rejects those entries locally instead of incorrectly sending OpenAI JSON to the Google API root. The enabled Antigravity credential observed during this run was expired. The enabled Codex fallback was initially marked unavailable and was subsequently switched to a healthy Codex account for the `combo-test` probe. Reauthenticate Antigravity and port or explicitly delegate its executor before benchmarking `ag/gemini-3.8-flash-high` as Gemini throughput.

This file records a local validation run; it does not replace the release workflow or `./scripts/test-port.sh .` on a supported Linux/WSL environment.
