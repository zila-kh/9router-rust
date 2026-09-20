# Codex streaming latency plan

**Status:** Implemented for the targeted path; live benchmark pending  
**Created:** 2026-09-19  
**Implemented:** 2026-09-20

## Implementation status

Steps 1-6 are implemented in `rust-backend/src/responses_stream.rs`, with the
streaming branch wired in `rust-backend/src/gateway.rs`:

- an incremental SSE decoder that buffers partial lines and never assumes a
  network chunk aligns with a frame (CRLF, multi-line `data:`, comments and
  keep-alives, a truncated final event, and an over-long frame that is dropped
  rather than buffered);
- a stateful Responses-to-Chat encoder emitting a stable id and model, one
  assistant role chunk, text and `reasoning_content` deltas, parallel tool calls
  with provider output indices, a terminal finish reason, provider usage, and
  exactly one `[DONE]`;
- the gateway takes the incremental path for `wants_stream` +
  `caller == Format::OpenAi` + `provider_format == Format::Responses` when the
  upstream answer is SSE. Same-format streaming still passes through raw, and
  non-streaming requests still use the buffered translation. A provider that
  ignores `stream: true` and answers with one JSON document stays on the
  buffered path;
- usage is recorded once from the terminal event, and a transport failure after
  the response was committed becomes a framed SSE error followed by `[DONE]`
  instead of a broken stream;
- inactivity is bounded on both streaming paths — 200 s to the first chunk and
  360 s between chunks by default (`STREAM_FIRST_CHUNK_TIMEOUT_MS` /
  `STREAM_STALL_TIMEOUT_MS`, or the `NINEROUTER_`-prefixed names) — and a stall
  or a stream that ends without a terminal event is reported in-band in the
  client's own format rather than closing silently or claiming a successful
  `finish_reason`;
- Claude and Gemini callers of a Responses provider are unchanged and still
  buffer (step 6).

Verified by unit tests: decoder chunk boundaries, CRLF, multi-line data,
keep-alives, malformed frames, truncated and over-long frames; encoder output for
text, reasoning, parallel tool calls, argument fragments reported only at item
completion, finish reasons, usage, provider errors and exactly one `[DONE]`; and
a deterministic progressive test that reads the first text delta off the
downstream stream *before* the completion event is sent upstream.

Not yet done: the step 7 live re-measurement. The numbers below are the
pre-change baseline and still stand as the "before" side; this repository has no
provider credentials, so the after-benchmark must run on a deployment.

## Problem and evidence

The native Rust gateway successfully calls Codex Responses models such as `cx/gpt-5.6-luna`, but Chat Completions streaming is not progressive when the provider transport is Responses. In `rust-backend/src/gateway.rs`, the direct streaming path passes bytes through only when the caller format matches the provider format. For a Chat Completions caller, the other path reads the complete upstream response with `response.bytes().await`, reduces/translates it, then synthesizes an SSE response. The client therefore receives its first token near the end of generation.

The 2026-09-19 live Luna sample measured a median **8.75 s first-token latency** and **43.88 end-to-end completion tokens/s** across three requests. End-to-end tokens/s includes the time spent waiting for the full upstream response; it does not isolate model generation speed. The benchmark establishes a gateway buffering issue, not a provider-only latency diagnosis.

## Goal

For a streaming Chat Completions request routed to a Responses-format provider, convert upstream Responses SSE events to Chat Completions SSE incrementally. Forward the first text delta as soon as it is available, without waiting for `response.completed` or collecting the full response body.

The first implementation targets `Format::OpenAi` callers with `Format::Responses` providers, including the tested `cx/gpt-5.6-luna` path. Preserve the existing behavior for non-streaming calls and same-format Responses pass-through.

## Implementation plan

1. **Capture protocol fixtures.** Add sanitized Responses SSE fixtures covering response creation, text deltas, reasoning deltas where present, tool-call item creation and argument deltas, completion with usage, provider error events, and truncated streams. Keep credentials, prompts, and account identifiers out of fixtures.
2. **Add an incremental SSE decoder.** In `rust-backend/src/streaming.rs` or a focused sibling module, parse arbitrary byte chunks into complete SSE events. Handle CRLF, multiple events in one chunk, one event split over many chunks, comments/keep-alives, multi-line `data:` fields, `[DONE]` where applicable, and a final unterminated event. Do not assume a network chunk aligns with an SSE frame.
3. **Add a stateful Responses-to-Chat stream encoder.** Keep a stable Chat Completions id and model; emit the assistant role once; map text and supported reasoning deltas as they arrive; assemble tool-call indices, ids, names, and argument fragments correctly; emit a final finish reason and provider usage; and terminate with exactly one `data: [DONE]` frame. Preserve current public usage-chunk semantics unless the parity fixtures demonstrate that they must change.
4. **Connect the streaming path in `gateway.rs`.** When `wants_stream`, `caller == Format::OpenAi`, and `provider_format == Format::Responses`, feed `response.bytes_stream()` through the decoder and encoder and return `Body::from_stream`. Keep the existing raw passthrough for matching formats and the current buffered translation for non-streaming requests. Apply backpressure naturally by only polling upstream as the client consumes the downstream body.
5. **Preserve errors and usage accounting.** Continue to handle non-2xx HTTP status before returning the downstream stream so existing combo fallback can select another candidate. Convert an in-band Responses error or body transport error after headers into a correctly framed Chat Completions SSE error and close the stream; retrying another account after bytes have been committed is unsafe. Record provider-reported usage from the terminal event once, without buffering text. Dropping the downstream body should stop polling and release the upstream stream.
6. **Extend other caller formats separately.** Claude and Gemini require their own event-by-event encoders; do not route those callers through the OpenAI chunk encoder. Responses callers using a Responses provider should continue to receive the current native pass-through path.
7. **Document measured behavior.** After implementation and acceptance, update `docs/PARITY.md` to narrow the cross-format streaming gap to any remaining formats, and update `docs/BUILD_STATUS.md` with before/after latency, throughput, and the tested route/model.

## Verification and acceptance

- Unit-test decoder chunk boundaries, CRLF, multi-line data, keep-alives, final partial frames, malformed frames, and truncated upstream streams.
- Unit-test emitted Chat Completions chunks for text, reasoning, parallel tool calls, usage, finish reasons, provider errors, and exactly one `[DONE]`.
- Use a local mock Responses server that delays `response.completed` after sending an early text delta. Assert the client receives that delta before the mock sends completion; this proves progressive behavior deterministically.
- Verify non-streaming requests still return the same normalized completion and usage; same-format `/v1/responses` streaming remains byte-stream pass-through; combo fallback still occurs for pre-stream HTTP errors; and the response is marked `x-9router-runtime: rust`.
- Repeat the live direct `cx/gpt-5.6-luna` benchmark with the same prompt and account for at least five runs before and after. Record median/p95 client time-to-first-text, end-to-end duration, provider-reported input/output/reasoning usage where available, visible output tokens/s, and end-to-end output tokens/s. Separate the two throughput figures; do not label end-to-end throughput as raw model generation speed.
- Acceptance requires the deterministic mock to show the first downstream text delta before the upstream completion event, no whole-body buffering on the targeted path, all relevant tests to pass, and a lower live median time-to-first-text without breaking non-streaming or same-format streaming behavior.

## Risks and limits

- Upstream event ordering varies with text, reasoning, and tool use. The encoder must tolerate interleaving and must not emit incomplete JSON arguments as a completed tool call.
- Once downstream headers or SSE data are sent, an upstream failure cannot be converted into a new HTTP status or safely retried through combo fallback. Send a stream error frame and terminate instead.
- Streaming should reduce time-to-first-token. It does not guarantee a higher provider generation rate or lower total generation duration.
- This plan does not implement the separate Antigravity executor needed for `ag/gemini-3.8-flash-high`.
