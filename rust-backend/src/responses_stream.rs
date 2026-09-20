//! Incremental Responses -> Chat Completions streaming.
//!
//! When a Chat Completions caller is routed to a Responses-format provider, the
//! buffered translation path reads the *whole* upstream response before it can
//! answer, so the client sees its first token only once generation has finished
//! (measured median 8.75 s first-token latency — see
//! `docs/CODEX_STREAMING_PLAN.md`). That is the difference between a router and a
//! slow proxy: a client comparing this against a direct connection notices
//! immediately.
//!
//! This module converts upstream SSE frames to Chat Completions chunks as they
//! arrive, so the first text delta is forwarded without waiting for
//! `response.completed`. The emitted chunk shape matches the buffered path's
//! `streaming::synthesize`, so only the timing differs, never the payload.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::translate::{canonical_usage_from_native, Format};

/// Whether an upstream answer is SSE.
///
/// A provider that ignores `stream: true` and replies with one JSON document
/// must not reach the incremental decoder: it carries no events, so the client
/// would receive an empty stream instead of the completion it was sent.
/// Unknown content types are treated as SSE, since SSE was requested.
pub fn upstream_is_event_stream(content_type: Option<&str>) -> bool {
    match content_type {
        Some(value) => value.contains("text/event-stream"),
        None => true,
    }
}

/// Largest partial frame the decoder will hold before it gives up on that frame
/// and skips to the next event boundary. Upstream frames are small; this only
/// bounds the damage from a pathological stream.
const MAX_PENDING: usize = 8 * 1024 * 1024;

/// One complete server-sent event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Incremental SSE framing.
///
/// A network chunk does not align with a frame: one frame may arrive across many
/// chunks, and one chunk may carry many frames. This buffers partial lines and
/// only reports events at a blank line, which is what makes the downstream
/// encoder able to answer on the first delta.
#[derive(Default)]
pub struct SseDecoder {
    pending: String,
    /// Set after an over-long frame is dropped: skip to the next boundary.
    skipping: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; complete events are appended to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some((block_end, consume_end)) = find_boundary(&self.pending) {
            let block = self.pending[..block_end].to_string();
            self.pending.drain(..consume_end);
            if self.skipping {
                self.skipping = false;
                continue;
            }
            if let Some(event) = parse_block(&block) {
                out.push(event);
            }
        }
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
            self.skipping = true;
        }
    }

    /// Report a final event that never received its terminating blank line.
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        let block = std::mem::take(&mut self.pending);
        self.skipping = false;
        if block.is_empty() {
            return;
        }
        if let Some(event) = parse_block(&block) {
            out.push(event);
        }
    }
}

/// Offset of the next event boundary: `(end_of_block, end_of_boundary)`.
/// Blank lines end an event; `\n`, `\r\n` and a bare `\r` all terminate a line.
fn find_boundary(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut line_start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let is_lf = bytes[index] == b'\n';
        let is_cr = bytes[index] == b'\r';
        if !is_lf && !is_cr {
            index += 1;
            continue;
        }
        let crlf = is_cr && bytes.get(index + 1) == Some(&b'\n');
        if is_cr && !crlf && bytes.get(index + 1).is_none() {
            // A trailing CR may be the first half of a CRLF split across chunks;
            // wait for the next chunk instead of treating it as a line break.
            return None;
        }
        let line = &text[line_start..index];
        let consume_end = if crlf { index + 2 } else { index + 1 };
        if line.is_empty() {
            return Some((line_start, consume_end));
        }
        line_start = consume_end;
        index = consume_end;
    }
    None
}

/// Field parsing for one event block: `data:` fields join with newlines,
/// `event:` names the event, comments and unknown fields are dropped.
fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut data: Vec<&str> = Vec::new();
    for raw in block.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event.event = value.to_string(),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    event.data = data.join("\n");
    Some(event)
}

#[derive(Debug, Clone)]
struct ToolCallState {
    index: usize,
    id: String,
    name: String,
    arguments_sent: bool,
}

/// Stateful Responses-event to Chat Completions-chunk conversion.
pub struct ResponsesToChatEncoder {
    id: String,
    model: String,
    started: bool,
    finished: bool,
    tool_calls: Vec<ToolCallState>,
    tool_call_by_item: HashMap<String, usize>,
    native_usage: Value,
    error: Option<String>,
}

impl ResponsesToChatEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            finished: false,
            tool_calls: Vec::new(),
            tool_call_by_item: HashMap::new(),
            native_usage: json!({}),
            error: None,
        }
    }

    /// Provider-native usage as reported by the terminal event.
    pub fn native_usage(&self) -> Value {
        self.native_usage.clone()
    }

    /// Canonical (cache-aware) usage for the usage ledger.
    pub fn canonical_usage(&self) -> Value {
        canonical_usage_from_native(&self.native_usage, Format::Responses)
    }

    /// Whether a usage event was seen at all.
    pub fn observed(&self) -> bool {
        self.native_usage
            .as_object()
            .is_some_and(|usage| !usage.is_empty())
    }

    /// Build one chunk. The frame is serialized once, after `usage` is attached,
    /// so a caller never has to re-encode a frame it already produced.
    fn chunk_value(&self, delta: Value, finish_reason: Value, usage: Option<Value>) -> Value {
        let mut value = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        });
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        value
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> Bytes {
        frame(&self.chunk_value(delta, finish_reason, None))
    }

    /// Emit the assistant role once, before any content, as OpenAI clients expect.
    fn ensure_started(&mut self, out: &mut Vec<Bytes>) {
        if self.started || self.finished {
            return;
        }
        self.started = true;
        out.push(self.chunk(json!({"role": "assistant"}), Value::Null));
    }

    pub fn handle(&mut self, event: &SseEvent, out: &mut Vec<Bytes>) {
        if self.finished || self.error.is_some() {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.event.as_str());
        match kind {
            "response.created" | "response.in_progress" => {
                self.capture_identity(&value);
                self.ensure_started(out);
            }
            "response.output_text.delta" => {
                let Some(text) = value.get("delta").and_then(Value::as_str) else {
                    return;
                };
                if text.is_empty() {
                    return;
                }
                self.ensure_started(out);
                out.push(self.chunk(json!({"content": text}), Value::Null));
            }
            // Vendors differ on whether reasoning arrives as a summary or as raw
            // text; both map to the `reasoning_content` field Chat clients read.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(text) = value.get("delta").and_then(Value::as_str) else {
                    return;
                };
                if text.is_empty() {
                    return;
                }
                self.ensure_started(out);
                out.push(self.chunk(json!({"reasoning_content": text}), Value::Null));
            }
            "response.output_item.added" => {
                let item = value.get("item").cloned().unwrap_or(json!({}));
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.open_tool_call(&value, &item, out);
                } else {
                    self.ensure_started(out);
                }
            }
            "response.function_call_arguments.delta" => {
                let Some(fragment) = value.get("delta").and_then(Value::as_str) else {
                    return;
                };
                if fragment.is_empty() {
                    return;
                }
                let item_id = value.get("item_id").and_then(Value::as_str);
                let Some(index) = self.index_for(item_id) else {
                    return;
                };
                self.tool_calls[index].arguments_sent = true;
                let call_index = self.tool_calls[index].index;
                self.ensure_started(out);
                out.push(self.chunk(
                    json!({"tool_calls": [{"index": call_index, "function": {"arguments": fragment}}]}),
                    Value::Null,
                ));
            }
            // Some providers only assemble arguments at the end of the item.
            "response.function_call_arguments.done" | "response.output_item.done" => {
                self.finish_tool_call(&value, out);
            }
            "response.completed" | "response.done" => {
                self.capture_usage(&value);
                self.finish_with("stop", out);
            }
            "response.incomplete" => {
                self.capture_usage(&value);
                self.finish_with("length", out);
            }
            "response.failed" | "error" => {
                self.error = Some(error_message(&value));
                out.push(frame(&json!({"error": {
                    "message": self.error.clone().unwrap_or_default(),
                    "type": "upstream_error",
                }})));
                self.close();
            }
            _ => {}
        }
    }

    fn capture_identity(&mut self, value: &Value) {
        if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
            self.id = id.replace("resp_", "chatcmpl-");
        }
        if let Some(model) = value.pointer("/response/model").and_then(Value::as_str) {
            if !model.is_empty() {
                self.model = model.to_string();
            }
        }
    }

    fn capture_usage(&mut self, value: &Value) {
        if let Some(usage) = value.pointer("/response/usage").and_then(Value::as_object) {
            if !usage.is_empty() {
                self.native_usage = Value::Object(usage.clone());
            }
        }
        self.capture_identity(value);
    }

    fn open_tool_call(&mut self, value: &Value, item: &Value, out: &mut Vec<Bytes>) {
        let call_index = self.tool_calls.len();
        let item_id = item
            .get("id")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let initial_arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
            .unwrap_or(call_index);
        self.tool_calls.push(ToolCallState {
            index,
            id: call_id,
            name,
            arguments_sent: !initial_arguments.is_empty(),
        });
        if let Some(item_id) = Some(item_id).filter(|id| !id.is_empty()) {
            self.tool_call_by_item.insert(item_id, call_index);
        }
        if let Some(output_item_id) = value.pointer("/item/id").and_then(Value::as_str) {
            self.tool_call_by_item
                .insert(output_item_id.to_string(), call_index);
        }
        self.ensure_started(out);
        let call = &self.tool_calls[call_index];
        let mut delta = json!({
            "tool_calls": [{
                "index": call.index,
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": ""},
            }]
        });
        if !initial_arguments.is_empty() {
            delta["tool_calls"][0]["function"]["arguments"] = json!(initial_arguments);
        }
        out.push(self.chunk(delta, Value::Null));
    }

    fn index_for(&self, item_id: Option<&str>) -> Option<usize> {
        if let Some(item_id) = item_id {
            if let Some(index) = self.tool_call_by_item.get(item_id) {
                return Some(*index);
            }
        }
        // No id to match on: only safe to assume if a single call is open.
        (self.tool_calls.len() == 1).then_some(0)
    }

    fn finish_tool_call(&mut self, value: &Value, out: &mut Vec<Bytes>) {
        let item = value.get("item").cloned().unwrap_or(json!({}));
        if value.get("item").is_some()
            && item.get("type").and_then(Value::as_str) != Some("function_call")
        {
            return;
        }
        let item_id = value
            .get("item_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str);
        let Some(index) = self.index_for(item_id) else {
            return;
        };
        if self.tool_calls[index].arguments_sent {
            return;
        }
        let arguments = value
            .get("arguments")
            .or_else(|| item.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if arguments.is_empty() {
            return;
        }
        self.tool_calls[index].arguments_sent = true;
        let call_index = self.tool_calls[index].index;
        self.ensure_started(out);
        out.push(self.chunk(
            json!({"tool_calls": [{"index": call_index, "function": {"arguments": arguments}}]}),
            Value::Null,
        ));
    }

    fn finish_with(&mut self, reason: &str, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        let reason = if reason == "stop" && !self.tool_calls.is_empty() {
            "tool_calls"
        } else {
            reason
        };
        // Usage rides the finish chunk, exactly as the buffered path's
        // `streaming::synthesize` puts it, so a client cannot tell the two apart.
        let usage = if self.observed() {
            self.canonical_usage()
        } else {
            json!({})
        };
        out.push(frame(&self.chunk_value(
            json!({}),
            json!(reason),
            Some(usage),
        )));
        self.close();
    }

    /// End the stream: a terminal finish chunk unless one was already sent,
    /// then exactly one `[DONE]`.
    pub fn close(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
    }

    /// Close out a stream that ended without a terminal event.
    ///
    /// The Responses protocol always ends with `response.completed`,
    /// `response.incomplete` or `response.failed`, so reaching here means the
    /// stream was cut short. Reporting a successful `finish_reason` would tell
    /// the client a truncated answer was complete, which is worse than an error
    /// it can act on.
    pub fn finish(&mut self, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.fail(
            "upstream stream ended before a terminal event (truncated or stalled)",
            out,
        );
    }

    /// Report a failure that happened after the client was committed to a 200.
    pub fn fail(&mut self, message: &str, out: &mut Vec<Bytes>) {
        if self.finished {
            return;
        }
        self.error = Some(message.to_string());
        out.push(frame(&json!({"error": {
            "message": message,
            "type": "upstream_error",
        }})));
        self.close();
    }
}

fn error_message(value: &Value) -> String {
    value
        .pointer("/response/error/message")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.pointer("/response/status_details/error/message"))
        .and_then(Value::as_str)
        .unwrap_or("upstream stream failed")
        .to_string()
}

fn frame(value: &Value) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(value).unwrap_or_default()
    ))
}

const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// Decoder plus encoder, driven by upstream bytes.
#[derive(Default)]
pub struct ResponsesToChatStream {
    decoder: SseDecoder,
    encoder: Option<ResponsesToChatEncoder>,
    events: Vec<SseEvent>,
    done_sent: bool,
}

impl ResponsesToChatStream {
    pub fn new(model: &str) -> Self {
        Self {
            decoder: SseDecoder::new(),
            encoder: Some(ResponsesToChatEncoder::new(model)),
            events: Vec::new(),
            done_sent: false,
        }
    }

    /// Feed upstream bytes; returns the client frames to send right now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.events.clear();
        self.decoder.push(chunk, &mut self.events);
        let events = std::mem::take(&mut self.events);
        if let Some(encoder) = self.encoder.as_mut() {
            for event in &events {
                if event.data == "[DONE]" {
                    encoder.close();
                    break;
                }
                encoder.handle(event, &mut out);
            }
        }
        self.events = events;
        self.events.clear();
        self.push_done_if_closed(&mut out);
        out
    }

    /// Flush the tail of the stream: an unterminated final event, then `[DONE]`.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        let mut events = Vec::new();
        self.decoder.finish(&mut events);
        if let Some(encoder) = self.encoder.as_mut() {
            for event in &events {
                if event.data == "[DONE]" {
                    encoder.close();
                    break;
                }
                encoder.handle(event, &mut out);
            }
            encoder.finish(&mut out);
        }
        self.push_done_if_closed(&mut out);
        out
    }

    /// A transport failure after the response was committed to the client.
    pub fn fail(&mut self, message: &str) -> Vec<Bytes> {
        let mut out = Vec::new();
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.fail(message, &mut out);
        }
        self.push_done_if_closed(&mut out);
        out
    }

    fn push_done_if_closed(&mut self, out: &mut Vec<Bytes>) {
        let closed = self.encoder.as_ref().is_none_or(|encoder| encoder.finished);
        if closed && !self.done_sent {
            self.done_sent = true;
            out.push(Bytes::from_static(DONE_FRAME));
        }
    }

    pub fn observed(&self) -> bool {
        self.encoder
            .as_ref()
            .is_some_and(ResponsesToChatEncoder::observed)
    }

    pub fn canonical_usage(&self) -> Value {
        self.encoder
            .as_ref()
            .map(ResponsesToChatEncoder::canonical_usage)
            .unwrap_or_else(|| json!({}))
    }

    pub fn native_usage(&self) -> Value {
        self.encoder
            .as_ref()
            .map(ResponsesToChatEncoder::native_usage)
            .unwrap_or_else(|| json!({}))
    }
}

/// Wrap an upstream byte stream so a Chat Completions client receives chunks as
/// the Responses events arrive. `on_complete` runs once after the stream ends,
/// which is where the caller records usage.
///
/// Dropping the returned stream stops polling upstream, so a client that
/// disconnects stops the provider work rather than buffering into the void.
/// Inactivity limits for a stream, mirroring upstream's `runtimeConfig.js`:
/// a slow prefill must not hold the client forever, and once tokens are flowing
/// a silent upstream should be reported rather than waited on. Generous by
/// default so slow reasoning models are never cut off mid-thought.
#[derive(Debug, Clone, Copy)]
pub struct StreamGuards {
    pub first_chunk: Duration,
    pub stall: Duration,
}
impl Default for StreamGuards {
    fn default() -> Self {
        Self {
            first_chunk: Duration::from_secs(200),
            stall: Duration::from_secs(360),
        }
    }
}

pub fn incremental_chat_stream<S, E, F>(
    upstream: S,
    model: String,
    guards: StreamGuards,
    on_complete: F,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display,
    F: FnOnce(&ResponsesToChatStream) + Send + 'static,
{
    async_stream::stream! {
        let mut pipeline = ResponsesToChatStream::new(&model);
        let mut upstream = Box::pin(upstream);
        let mut started = false;
        loop {
            let limit = if started { guards.stall } else { guards.first_chunk };
            let next = match tokio::time::timeout(limit, upstream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    // Report the stall in-band: headers are long gone, so the
                    // only honest signal left is a terminal error frame.
                    let seconds = limit.as_secs();
                    let stage = if started { "stalled mid-stream" } else { "sent no first chunk" };
                    for outgoing in pipeline.fail(&format!("upstream {stage} after {seconds}s")) {
                        yield Ok(outgoing);
                    }
                    on_complete(&pipeline);
                    return;
                }
            };
            let Some(chunk) = next else { break };
            match chunk {
                Ok(bytes) => {
                    started = true;
                    for outgoing in pipeline.push(&bytes) {
                        yield Ok(outgoing);
                    }
                }
                Err(error) => {
                    for outgoing in pipeline.fail(&format!("{error}")) {
                        yield Ok(outgoing);
                    }
                    on_complete(&pipeline);
                    return;
                }
            }
        }
        for outgoing in pipeline.finish() {
            yield Ok(outgoing);
        }
        on_complete(&pipeline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_text(frames: &[Bytes]) -> Vec<String> {
        frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).to_string())
            .collect()
    }

    fn decoded(frames: &[Bytes]) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|frame| {
                let text = String::from_utf8_lossy(frame);
                let data = text.strip_prefix("data: ")?.trim();
                if data == "[DONE]" {
                    return Some(json!("[DONE]"));
                }
                serde_json::from_str::<Value>(data).ok()
            })
            .collect()
    }

    fn sse(events: &[&str]) -> String {
        events.join("")
    }

    #[test]
    fn decoder_handles_arbitrary_chunk_boundaries() {
        let payload = sse(&[
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-luna\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        ]);
        for width in [1usize, 3, 7, 13, 4096] {
            let mut decoder = SseDecoder::new();
            let mut events = Vec::new();
            for chunk in payload.as_bytes().chunks(width) {
                decoder.push(chunk, &mut events);
            }
            decoder.finish(&mut events);
            assert_eq!(events.len(), 3, "width {width}: {events:?}");
            assert_eq!(events[0].event, "response.created");
            assert!(events[2].data.contains("\"lo\""));
        }
    }

    #[test]
    fn decoder_handles_crlf_multiline_data_and_keepalives() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        decoder.push(
            b": keep-alive\r\n\r\nevent: message\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n",
            &mut events,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "{\"a\":\n1}");
        assert_eq!(
            serde_json::from_str::<Value>(&events[0].data).unwrap()["a"],
            json!(1)
        );

        // A comment-only frame carries no data and must not surface.
        let mut events = Vec::new();
        decoder.push(b": ping\n\n", &mut events);
        assert!(events.is_empty());
    }

    #[test]
    fn decoder_reports_a_truncated_final_event() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        decoder.push(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"tail\"}",
            &mut events,
        );
        assert!(events.is_empty(), "no boundary yet");
        decoder.finish(&mut events);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn encoder_emits_openai_chunks_progressively() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let first = stream.push(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_abc\",\"model\":\"gpt-5.6-luna\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        );
        let values = decoded(&first);
        assert_eq!(values[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(values[0]["id"], json!("chatcmpl-abc"));
        assert_eq!(values[0]["model"], json!("gpt-5.6-luna"));
        assert_eq!(values[1]["choices"][0]["delta"]["content"], json!("Hel"));
        assert_eq!(values[1]["choices"][0]["finish_reason"], Value::Null);
        assert_eq!(values[1]["object"], json!("chat.completion.chunk"));

        let second = stream.push(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        );
        assert_eq!(
            decoded(&second)[0]["choices"][0]["delta"]["content"],
            json!("lo")
        );

        let last = stream.push(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_abc\",\"usage\":{\"input_tokens\":1200,\"output_tokens\":12,\"total_tokens\":1212,\"input_tokens_details\":{\"cached_tokens\":1024}}}}\n\n",
        );
        let values = decoded(&last);
        assert_eq!(values[0]["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(values[0]["usage"]["prompt_tokens"], json!(1200));
        assert_eq!(values[1], json!("[DONE]"));
        assert!(stream.observed());
        assert_eq!(stream.canonical_usage()["cached_tokens"], json!(1024));

        // Exactly one [DONE], even if the stream is finished again.
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn encoder_streams_reasoning_and_parallel_tool_calls() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"item_1\",\"type\":\"reasoning\",\"summary\":[]}}\n\nevent: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"item_1\",\"delta\":\"thinking\"}\n\n",
        );
        let values = decoded(&out);
        assert_eq!(
            values[1]["choices"][0]["delta"]["reasoning_content"],
            json!("thinking")
        );

        let out = stream.push(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"item_2\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\nevent: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_2\",\"delta\":\"{\\\"path\\\":\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"id\":\"item_3\",\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"write_file\",\"arguments\":\"\"}}\n\nevent: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_3\",\"delta\":\"{\\\"path\\\":\"}\n\n",
        );
        let values = decoded(&out);
        let calls: Vec<&Value> = values
            .iter()
            .filter_map(|value| value.pointer("/choices/0/delta/tool_calls"))
            .collect();
        assert_eq!(calls.len(), 4, "two opens + two argument fragments");
        assert_eq!(calls[0][0]["id"], json!("call_a"));
        assert_eq!(calls[0][0]["function"]["name"], json!("read_file"));
        assert_eq!(calls[0][0]["index"], json!(1));
        assert_eq!(calls[1][0]["function"]["arguments"], json!("{\"path\":"));
        assert_eq!(
            calls[2][0]["index"],
            json!(2),
            "second call keeps the provider index"
        );
        assert_eq!(calls[3][0]["function"]["arguments"], json!("{\"path\":"));

        // A completed stream with open tool calls finishes as tool_calls.
        let out = stream.push(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_x\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
        );
        assert_eq!(
            decoded(&out)[0]["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
    }

    #[test]
    fn encoder_appends_arguments_only_reported_at_item_done() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"item_1\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"item_1\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}\n\n",
        );
        let values = decoded(&out);
        let arguments: Vec<&Value> = values
            .iter()
            .filter_map(|value| value.pointer("/choices/0/delta/tool_calls/0/function/arguments"))
            .collect();
        assert_eq!(arguments, vec![&json!(""), &json!("{\"path\":\"a.txt\"}")]);
    }

    #[test]
    fn encoder_maps_incomplete_and_failed_streams() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":900}}}\n\n",
        );
        let values = decoded(&out);
        // `finish_reason` is present but null on every non-terminal chunk, so
        // look for the first chunk that actually names a reason.
        let finish = values
            .iter()
            .filter_map(|value| {
                value
                    .pointer("/choices/0/finish_reason")
                    .and_then(Value::as_str)
            })
            .next()
            .unwrap_or_default();
        assert_eq!(finish, "length", "frames: {:?}", frames_text(&out));
        assert_eq!(values.last(), Some(&json!("[DONE]")));

        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"rate limited\"}}}\n\n",
        );
        let values = decoded(&out);
        assert_eq!(values[0]["error"]["message"], json!("rate limited"));
        assert_eq!(values[1], json!("[DONE]"), "the stream still terminates");
    }

    #[test]
    fn an_unterminated_stream_reports_the_truncation_instead_of_a_finish() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        );
        assert_eq!(
            decoded(&out)[1]["choices"][0]["delta"]["content"],
            json!("partial")
        );

        // The Responses protocol always ends with a terminal event, so reaching
        // the end without one means the answer was cut short. Claiming a
        // successful finish_reason would tell the client a partial answer was
        // complete.
        let values = decoded(&stream.finish());
        assert_eq!(values[0]["error"]["type"], json!("upstream_error"));
        assert!(
            values[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("terminal event"),
            "{values:?}"
        );
        assert_eq!(values[1], json!("[DONE]"));
        assert!(stream.finish().is_empty(), "no second terminal frame");
    }

    #[tokio::test]
    async fn a_stalled_upstream_is_reported_in_band() {
        use tokio::sync::mpsc;

        let (_tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let upstream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let stream = incremental_chat_stream(
            upstream,
            "gpt-5.6-luna".to_string(),
            StreamGuards {
                first_chunk: Duration::from_millis(50),
                stall: Duration::from_millis(50),
            },
            |_| {},
        );
        let mut stream = Box::pin(stream);

        // Nothing ever arrives: the client must get a terminal error frame rather
        // than a stream that hangs until the transport gives up.
        let first = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("the guard fires without a chunk")
            .expect("stream is open")
            .unwrap();
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("\"error\""), "{text}");
        assert!(text.contains("no first chunk"), "{text}");

        let mut saw_done = false;
        while let Some(frame) = stream.next().await {
            if String::from_utf8_lossy(&frame.unwrap()).contains("[DONE]") {
                saw_done = true;
            }
        }
        assert!(saw_done);
    }

    #[test]
    fn transport_failure_becomes_a_framed_error_then_done() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        stream.push(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n");
        let out = stream.fail("connection reset");
        let values = decoded(&out);
        assert_eq!(values[0]["error"]["message"], json!("connection reset"));
        assert_eq!(values[1], json!("[DONE]"));
        assert!(stream.finish().is_empty(), "no second terminal frame");
    }

    #[test]
    fn malformed_events_are_skipped_without_breaking_the_stream() {
        let mut stream = ResponsesToChatStream::new("gpt-5.6-luna");
        let out = stream.push(
            b"data: not json\n\ndata: {\"type\":\"unknown.event\",\"x\":1}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        );
        let values = decoded(&out);
        assert_eq!(values[1]["choices"][0]["delta"]["content"], json!("ok"));
    }

    #[tokio::test]
    async fn first_token_reaches_the_client_before_upstream_completes() {
        // The deterministic proof required by the plan: the mock sends an early
        // text delta and only afterwards the completion event. If the gateway
        // buffered, the client would see nothing until the completion arrived.
        use tokio::sync::mpsc;

        let (upstream_tx, upstream_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let upstream = futures_util::stream::unfold(upstream_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let stream = incremental_chat_stream(
            upstream,
            "gpt-5.6-luna".to_string(),
            StreamGuards::default(),
            |_| {},
        );
        let mut stream = Box::pin(stream);

        upstream_tx
            .send(Ok(Bytes::from_static(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-luna\"}}\n\n",
            )))
            .await
            .unwrap();
        upstream_tx
            .send(Ok(Bytes::from_static(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
            )))
            .await
            .unwrap();

        // Read until the text delta arrives — while the completion event still
        // has not been sent upstream.
        let mut saw_text = false;
        for _ in 0..4 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("a frame arrives without the completion event")
                .expect("stream is open")
                .unwrap();
            if String::from_utf8_lossy(&frame).contains("\"first\"") {
                saw_text = true;
                break;
            }
        }
        assert!(
            saw_text,
            "the first text delta must be forwarded progressively"
        );

        upstream_tx
            .send(Ok(Bytes::from_static(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
            )))
            .await
            .unwrap();
        drop(upstream_tx);

        let mut saw_done = false;
        while let Some(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("stream terminates")
        {
            if String::from_utf8_lossy(&frame.unwrap()).contains("[DONE]") {
                saw_done = true;
            }
        }
        assert!(saw_done, "the stream ends with [DONE]");
    }

    #[tokio::test]
    async fn completion_callback_sees_the_recorded_usage() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::mpsc;

        let seen: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({})));
        let captured = seen.clone();
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let upstream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let stream = incremental_chat_stream(
            upstream,
            "gpt-5.6-luna".to_string(),
            StreamGuards::default(),
            move |pipeline| {
                *captured.lock().unwrap() = pipeline.canonical_usage();
            },
        );
        let mut stream = Box::pin(stream);
        tx.send(Ok(Bytes::from_static(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":500,\"output_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":448}}}}\n\n",
        )))
        .await
        .unwrap();
        drop(tx);
        while stream.next().await.is_some() {}
        let usage = seen.lock().unwrap().clone();
        assert_eq!(usage["prompt_tokens"], json!(500));
        assert_eq!(usage["cached_tokens"], json!(448));
        assert_eq!(usage["completion_tokens"], json!(20));
    }

    #[test]
    fn frames_are_single_line_json_events() {
        let mut stream = ResponsesToChatStream::new("m");
        let out = stream.push(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"line1\\nline2\"}\n\n",
        );
        let text = frames_text(&out);
        assert!(
            text.iter().all(|frame| frame.ends_with("\n\n")),
            "every frame is blank-line terminated"
        );
        assert_eq!(
            text[1].trim().lines().count(),
            1,
            "embedded newlines are escaped inside the JSON payload"
        );
    }
}

#[cfg(test)]
mod decoder_limit_tests {
    use super::*;

    #[test]
    fn an_oversized_frame_is_dropped_instead_of_buffered() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        let mut huge = String::from("data: ");
        huge.push_str(&"x".repeat(MAX_PENDING + 16));
        decoder.push(huge.as_bytes(), &mut events);
        assert!(events.is_empty());
        // The next well-formed event still arrives: only that one frame was lost.
        decoder.push(
            b"\ndata: {\"type\":\"response.completed\"}\n\n",
            &mut events,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"type\":\"response.completed\"}");
    }

    #[test]
    fn event_stream_detection_keeps_json_responses_off_the_incremental_path() {
        assert!(upstream_is_event_stream(Some(
            "text/event-stream; charset=utf-8"
        )));
        assert!(!upstream_is_event_stream(Some("application/json")));
        assert!(upstream_is_event_stream(None));
    }
}
