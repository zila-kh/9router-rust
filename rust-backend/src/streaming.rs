use crate::{error::AppError, translate::Format};
use bytes::Bytes;
use chrono::Utc;
use serde_json::{json, Value};

pub fn looks_streaming(content_type: Option<&str>, bytes: &[u8]) -> bool {
    content_type
        .map(|s| {
            s.contains("text/event-stream")
                || s.contains("amazon.eventstream")
                || s.contains("grpc-web")
        })
        .unwrap_or(false)
        || bytes.starts_with(b"data:")
        || bytes.windows(6).any(|w| w == b"data: ")
}

fn sse_jsons(bytes: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        for line in block.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    out.push(v)
                }
            }
        }
    }
    out
}

/// Longest partial SSE line kept while sniffing; a single frame larger than
/// this (an inline image, say) is dropped rather than buffered.
const SNIFFER_MAX_PENDING: usize = 256 * 1024;

/// Reads the cache-aware usage out of an SSE stream as it passes through.
///
/// A same-format stream is forwarded verbatim, so nothing downstream ever sees
/// the usage event that carries the cache counters. Feeding the bytes through
/// here as they pass keeps streaming requests in the ledger — which is the only
/// way a Claude Code session (always streaming) can show its cache hit rate.
pub struct UsageSniffer {
    format: Format,
    pending: String,
    native: Value,
}
impl UsageSniffer {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            pending: String::new(),
            native: json!({}),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some(index) = self.pending.find('\n') {
            let line = self.pending[..index].trim().to_string();
            self.pending.drain(..=index);
            if let Some(data) = line.strip_prefix("data:") {
                self.absorb(data.trim());
            }
        }
        if self.pending.len() > SNIFFER_MAX_PENDING {
            self.pending.clear();
        }
    }

    /// Provider-native usage collected so far.
    pub fn native_usage(&self) -> Value {
        self.native.clone()
    }

    /// Whether any usage event was seen at all — a stream the client abandoned
    /// before the usage frame yields false.
    pub fn observed(&self) -> bool {
        self.native
            .as_object()
            .is_some_and(|usage| !usage.is_empty())
    }

    fn absorb(&mut self, data: &str) {
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match self.format {
            Format::Claude => match event.get("type").and_then(Value::as_str) {
                Some("message_start") => self.merge(event.pointer("/message/usage")),
                Some("message_delta") => self.merge(event.get("usage")),
                _ => {}
            },
            Format::Gemini => self.merge(event.get("usageMetadata")),
            Format::Responses => {
                if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                    self.merge(event.pointer("/response/usage"));
                }
            }
            Format::OpenAi => self.merge(event.get("usage")),
        }
    }

    /// Keep the newest positive counter per field: Claude splits the prompt side
    /// (message_start) from the output side (message_delta) across events.
    fn merge(&mut self, incoming: Option<&Value>) {
        let Some(object) = incoming.and_then(Value::as_object) else {
            return;
        };
        let native = self
            .native
            .as_object_mut()
            .expect("sniffer usage is an object");
        for (key, value) in object {
            let positive = value.as_i64().is_some_and(|count| count > 0);
            if positive || !native.contains_key(key) {
                native.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Terminal frames for a stream that aborted or stalled *after* HTTP 200 was
/// already committed to the client, so the status code can no longer change.
///
/// Never end a stream silently: an OpenAI-compatible client raises on a `data:`
/// payload carrying an `error` key (it checks that before `[DONE]`), an
/// Anthropic client needs an `event: error` frame, and a Responses client needs a
/// `response.failed` event. Shapes mirror upstream's
/// `buildStreamErrorBytes`/`buildAbortedResponsesTerminalBytes`.
pub fn abort_terminal_frames(format: Format, message: &str) -> Bytes {
    let payload = match format {
        Format::Claude => format!(
            "event: error
data: {}

",
            json!({
                "type": "error",
                "error": {
                    "message": message,
                    "type": "server_error",
                    "code": "internal_server_error",
                },
            })
        ),
        Format::Gemini => format!(
            "data: {}

",
            json!({"error": {"code": 504, "message": message, "status": "GATEWAY_TIMEOUT"}})
        ),
        Format::Responses => format!(
            "event: response.failed
data: {}

data: [DONE]

",
            json!({
                "type": "response.failed",
                "response": {
                    "id": format!("resp_{}", Utc::now().timestamp_millis()),
                    "status": "failed",
                    "error": {
                        "type": "stream_error",
                        "code": "stream_disconnected",
                        "message": message,
                    },
                },
            })
        ),
        Format::OpenAi => format!(
            "data: {}

data: [DONE]

",
            json!({
                "error": {
                    "message": message,
                    "type": "server_error",
                    "code": "internal_server_error",
                },
            })
        ),
    };
    Bytes::from(payload)
}

pub fn reduce_stream(bytes: &[u8], format: Format, model: &str) -> Result<Value, AppError> {
    match format {
        Format::OpenAi => reduce_openai(bytes, model),
        Format::Claude => reduce_claude(bytes, model),
        Format::Gemini => reduce_gemini(bytes),
        Format::Responses => reduce_responses(bytes, model),
    }
}

fn reduce_openai(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = "stop".to_string();
    let mut prompt = 0;
    let mut completion = 0;
    // Cache counters ride in the usage payload; dropping them here would hide
    // every hit from the ledger and from cost accounting.
    let mut cached = 0;
    let mut cache_creation = 0;
    let mut cached_reasoning = 0;
    let mut calls: Vec<Value> = Vec::new();
    let mut id = None;
    for e in events {
        if id.is_none() {
            id = e.get("id").cloned()
        }
        if let Some(u) = e.get("usage") {
            prompt = u
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(prompt);
            completion = u
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(completion);
            cached = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(cached);
            cache_creation = u
                .pointer("/prompt_tokens_details/cache_creation_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(cache_creation);
            cached_reasoning = u
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(cached_reasoning);
        }
        if let Some(c) = e
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                finish = f.into()
            }
            let d = c
                .get("delta")
                .or_else(|| c.get("message"))
                .cloned()
                .unwrap_or(json!({}));
            if let Some(t) = d.get("content").and_then(Value::as_str) {
                content.push_str(t)
            }
            if let Some(t) = d.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(t)
            }
            if let Some(tc) = d.get("tool_calls").and_then(Value::as_array) {
                for item in tc {
                    let idx = item
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(calls.len() as u64) as usize;
                    while calls.len() <= idx {
                        calls.push(json!({"id":"","type":"function","function":{"name":"","arguments":""}}))
                    }
                    if let Some(s) = item.get("id").and_then(Value::as_str) {
                        calls[idx]["id"] = json!(s)
                    }
                    if let Some(s) = item.pointer("/function/name").and_then(Value::as_str) {
                        calls[idx]["function"]["name"] = json!(s)
                    }
                    if let Some(s) = item.pointer("/function/arguments").and_then(Value::as_str) {
                        let old = calls[idx]["function"]["arguments"].as_str().unwrap_or("");
                        calls[idx]["function"]["arguments"] = json!(format!("{old}{s}"))
                    }
                }
            }
        }
    }
    let mut msg = json!({"role":"assistant","content":content});
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning)
    }
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls)
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":model,"choices":[{"index":0,"message":msg,"finish_reason":finish}],"usage":stream_usage(prompt, completion, cached, cache_creation, cached_reasoning)}),
    )
}

/// OpenAI-shaped usage with the cache counters re-attached, so the canonical
/// conversion downstream can fold them like any other provider's.
fn stream_usage(
    prompt: i64,
    completion: i64,
    cached: i64,
    cache_creation: i64,
    reasoning: i64,
) -> Value {
    let mut usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    });
    if cached > 0 || cache_creation > 0 {
        let mut details = serde_json::Map::new();
        if cached > 0 {
            details.insert("cached_tokens".into(), json!(cached));
        }
        if cache_creation > 0 {
            details.insert("cache_creation_tokens".into(), json!(cache_creation));
        }
        usage["prompt_tokens_details"] = Value::Object(details);
    }
    if reasoning > 0 {
        usage["completion_tokens_details"] = json!({"reasoning_tokens": reasoning});
    }
    usage
}

fn reduce_claude(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut id = None;
    let mut content: Vec<Value> = Vec::new();
    let mut stop = "end_turn".to_string();
    let mut input = 0;
    let mut output = 0;
    // Claude reports cache tokens separately from input_tokens, on message_start
    // (prompt side) and again on message_delta; both are carried through so the
    // canonical conversion can fold them into the prompt count.
    let mut cache_read = 0;
    let mut cache_creation = 0;
    let mut active: Option<usize> = None;
    for e in events {
        match e.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let m = e.get("message").cloned().unwrap_or(json!({}));
                id = m.get("id").cloned();
                input = m
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(input);
                cache_read = m
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(cache_read);
                cache_creation = m
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(cache_creation);
            }
            Some("content_block_start") => {
                let idx = e
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(content.len() as u64) as usize;
                while content.len() <= idx {
                    content.push(json!({}))
                }
                content[idx] = e
                    .get("content_block")
                    .cloned()
                    .unwrap_or(json!({"type":"text","text":""}));
                active = Some(idx)
            }
            Some("content_block_delta") => {
                let idx = e
                    .get("index")
                    .and_then(Value::as_u64)
                    .or(active.map(|i| i as u64))
                    .unwrap_or(0) as usize;
                while content.len() <= idx {
                    content.push(json!({"type":"text","text":""}))
                }
                let d = e.get("delta").cloned().unwrap_or(json!({}));
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let s = d.get("text").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        content[idx]["type"] = json!("text");
                        content[idx]["text"] = json!(format!("{old}{s}"))
                    }
                    Some("input_json_delta") => {
                        let s = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("_partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        content[idx]["_partial_json"] = json!(format!("{old}{s}"))
                    }
                    Some("thinking_delta") => {
                        let s = d.get("thinking").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        content[idx]["type"] = json!("thinking");
                        content[idx]["thinking"] = json!(format!("{old}{s}"))
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(i) = e.get("index").and_then(Value::as_u64).map(|x| x as usize) {
                    if let Some(s) = content
                        .get(i)
                        .and_then(|v| v.get("_partial_json"))
                        .and_then(Value::as_str)
                    {
                        content[i]["input"] = serde_json::from_str(s).unwrap_or(json!({}));
                        if let Some(o) = content[i].as_object_mut() {
                            o.remove("_partial_json");
                        }
                    }
                }
            }
            Some("message_delta") => {
                stop = e
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or(&stop)
                    .to_string();
                output = e
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(output);
                cache_read = e
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(cache_read);
                cache_creation = e
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(cache_creation);
            }
            _ => {}
        }
    }
    let mut usage = json!({"input_tokens": input, "output_tokens": output});
    if cache_read > 0 {
        usage["cache_read_input_tokens"] = json!(cache_read);
    }
    if cache_creation > 0 {
        usage["cache_creation_input_tokens"] = json!(cache_creation);
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("msg_{}",uuid::Uuid::new_v4().simple()))),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":usage}),
    )
}

fn reduce_gemini(bytes: &[u8]) -> Result<Value, AppError> {
    let mut events = sse_jsons(bytes);
    if events.is_empty() {
        let text = String::from_utf8_lossy(bytes);
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&text) {
            events = a
        }
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut finish = "STOP".to_string();
    let mut usage = json!({});
    for e in events {
        if let Some(c) = e
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(f) = c.get("finishReason").and_then(Value::as_str) {
                finish = f.into()
            }
            for p in c
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    text.push_str(t)
                }
                if p.get("functionCall").is_some() {
                    calls.push(p)
                }
            }
        }
        if e.get("usageMetadata").is_some() {
            usage = e.get("usageMetadata").cloned().unwrap()
        }
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"text":text}))
    }
    parts.extend(calls);
    Ok(
        json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":finish,"index":0}],"usageMetadata":usage}),
    )
}

fn reduce_responses(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut id = None;
    let mut text = String::new();
    let mut output = Vec::new();
    let mut usage = json!({});
    let mut status = "completed".to_string();
    for e in events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            id = e
                .pointer("/response/id")
                .cloned()
                .or_else(|| e.get("response_id").cloned())
        }
        match ty {
            "response.output_text.delta" => {
                text.push_str(e.get("delta").and_then(Value::as_str).unwrap_or(""))
            }
            "response.output_item.done" => {
                if let Some(item) = e.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        output.push(item.clone())
                    }
                }
            }
            "response.completed" => {
                if let Some(r) = e.get("response") {
                    status = r
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .into();
                    usage = r.get("usage").cloned().unwrap_or(json!({}));
                    if let Some(a) = r.get("output").and_then(Value::as_array) {
                        for item in a {
                            if item.get("type").and_then(Value::as_str) == Some("function_call")
                                && !output.iter().any(|x| x == item)
                            {
                                output.push(item.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        output.insert(0,json!({"type":"message","id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}))
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("resp_{}",uuid::Uuid::new_v4().simple()))),"object":"response","status":status,"model":model,"output":output,"usage":usage}),
    )
}

pub fn synthesize(openai: &Value, caller: Format) -> Result<Vec<u8>, AppError> {
    match caller {
        Format::OpenAi => openai_sse(openai),
        Format::Claude => claude_sse(openai),
        Format::Gemini => gemini_sse(openai),
        Format::Responses => responses_sse(openai),
    }
}
fn line(v: &Value) -> Vec<u8> {
    format!("data: {}\n\n", serde_json::to_string(v).unwrap()).into_bytes()
}
fn openai_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let id = b
        .get("id")
        .cloned()
        .unwrap_or(json!(format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())));
    let model = b.get("model").cloned().unwrap_or(json!(""));
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut out = Vec::new();
    out.extend(line(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": null
        }]
    })));

    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            out.extend(line(&json!({
                "id": id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"content": text},
                    "finish_reason": null
                }]
            })));
        }
    }

    if let Some(tool_calls) = message.get("tool_calls") {
        out.extend(line(&json!({
            "id": id,
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": tool_calls},
                "finish_reason": null
            }]
        })));
    }

    out.extend(line(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": choice
                .get("finish_reason")
                .cloned()
                .unwrap_or(json!("stop"))
        }],
        "usage": b.get("usage").cloned().unwrap_or(json!({}))
    })));
    out.extend_from_slice(b"data: [DONE]\n\n");
    Ok(out)
}
fn claude_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let c = crate::translate::caller_response(b.clone(), Format::Claude)?;
    let mut out = Vec::new();
    out.extend_from_slice(format!("event: message_start\ndata: {}\n\n",serde_json::to_string(&json!({"type":"message_start","message":{"id":c.get("id"),"type":"message","role":"assistant","model":c.get("model"),"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":c.pointer("/usage/input_tokens").cloned().unwrap_or(json!(0)),"output_tokens":0}}})).unwrap()).as_bytes());
    for (i, block) in c
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        out.extend_from_slice(format!("event: content_block_start\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_start","index":i,"content_block":match block.get("type").and_then(Value::as_str){Some("text")=>json!({"type":"text","text":""}),Some("tool_use")=>json!({"type":"tool_use","id":block.get("id"),"name":block.get("name"),"input":{}}),_=>block.clone()}})).unwrap()).as_bytes());
        match block.get("type").and_then(Value::as_str){Some("text")=>out.extend_from_slice(format!("event: content_block_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":block.get("text")}})).unwrap()).as_bytes()),Some("tool_use")=>out.extend_from_slice(format!("event: content_block_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap()}})).unwrap()).as_bytes()),_=>{}}
        out.extend_from_slice(
            format!(
                "event: content_block_stop\ndata: {}\n\n",
                serde_json::to_string(&json!({"type":"content_block_stop","index":i})).unwrap()
            )
            .as_bytes(),
        )
    }
    out.extend_from_slice(format!("event: message_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"message_delta","delta":{"stop_reason":c.get("stop_reason"),"stop_sequence":null},"usage":{"output_tokens":c.pointer("/usage/output_tokens").cloned().unwrap_or(json!(0))}})).unwrap()).as_bytes());
    out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Ok(out)
}
fn gemini_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let v = crate::translate::caller_response(b.clone(), Format::Gemini)?;
    Ok(line(&v))
}
fn responses_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let r = crate::translate::caller_response(b.clone(), Format::Responses)?;
    let id = r.get("id").cloned().unwrap_or(json!(""));
    let mut out = Vec::new();
    out.extend(line(&json!({"type":"response.created","response":{"id":id,"object":"response","status":"in_progress","model":r.get("model"),"output":[]}})));
    for (oi, item) in r
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        out.extend(line(
            &json!({"type":"response.output_item.added","output_index":oi,"item":item}),
        ));
        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (ci, c) in item
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                if let Some(t) = c.get("text").and_then(Value::as_str) {
                    out.extend(line(&json!({"type":"response.output_text.delta","output_index":oi,"content_index":ci,"delta":t})));
                    out.extend(line(&json!({"type":"response.output_text.done","output_index":oi,"content_index":ci,"text":t})))
                }
            }
        }
        out.extend(line(
            &json!({"type":"response.output_item.done","output_index":oi,"item":item}),
        ))
    }
    out.extend(line(&json!({"type":"response.completed","response":r})));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stream_cache_tokens_survive_reduction() {
        let stream = [
            "event: message_start",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4.5\",\"usage\":{\"input_tokens\":900,\"output_tokens\":0,\"cache_read_input_tokens\":21000,\"cache_creation_input_tokens\":3000}}}",
            "",
            "event: content_block_delta",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
            "",
            "event: message_delta",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":12}}",
            "",
            "event: message_stop",
            "data: {\"type\":\"message_stop\"}",
            "",
        ]
        .join("\n");

        let reduced =
            reduce_stream(stream.as_bytes(), Format::Claude, "claude-sonnet-4.5").unwrap();
        assert_eq!(reduced["usage"]["input_tokens"], json!(900));
        assert_eq!(reduced["usage"]["cache_read_input_tokens"], json!(21000));
        assert_eq!(reduced["usage"]["cache_creation_input_tokens"], json!(3000));

        // The canonical conversion is where the prompt total becomes the
        // cache-inclusive count the dashboard and the pricing model both read.
        let canonical = crate::translate::normalize_response(reduced, Format::Claude).unwrap();
        let tokens = crate::translate::stored_tokens(&canonical["usage"]);
        assert_eq!(tokens["prompt_tokens"], json!(24900));
        assert_eq!(tokens["cached_tokens"], json!(21000));
        assert_eq!(tokens["completion_tokens"], json!(12));
    }

    #[test]
    fn openai_stream_cache_details_survive_reduction() {
        let stream = [
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}",
            "",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5000,\"completion_tokens\":20,\"total_tokens\":5020,\"prompt_tokens_details\":{\"cached_tokens\":4096},\"completion_tokens_details\":{\"reasoning_tokens\":8}}}",
            "",
            "data: [DONE]",
            "",
        ]
        .join("\n");

        let reduced = reduce_stream(stream.as_bytes(), Format::OpenAi, "gpt-5.1").unwrap();
        assert_eq!(
            reduced["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(4096)
        );
        assert_eq!(
            reduced["usage"]["completion_tokens_details"]["reasoning_tokens"],
            json!(8)
        );

        let canonical = crate::translate::normalize_response(reduced, Format::OpenAi).unwrap();
        let tokens = crate::translate::stored_tokens(&canonical["usage"]);
        assert_eq!(tokens["cached_tokens"], json!(4096));
        assert_eq!(tokens["reasoning_tokens"], json!(8));
    }
}

#[cfg(test)]
mod sniffer_tests {
    use super::*;

    #[test]
    fn claude_usage_is_collected_across_split_frames() {
        let mut sniffer = UsageSniffer::new(Format::Claude);
        // Frames arrive in fragments, as they do over a socket.
        let payload = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":800,",
            "\"cache_read_input_tokens\":19000,\"cache_creation_input_tokens\":2000,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
        );
        for fragment in payload.as_bytes().chunks(37) {
            sniffer.feed(fragment);
        }
        let native = sniffer.native_usage();
        assert_eq!(native["input_tokens"], json!(800));
        assert_eq!(native["cache_read_input_tokens"], json!(19000));
        assert_eq!(native["cache_creation_input_tokens"], json!(2000));
        assert_eq!(
            native["output_tokens"],
            json!(7),
            "message_delta updates output only"
        );

        let canonical = crate::translate::canonical_usage_from_native(&native, Format::Claude);
        assert_eq!(canonical["prompt_tokens"], json!(21800));
        assert_eq!(canonical["cached_tokens"], json!(19000));
    }

    #[test]
    fn openai_and_gemini_usage_events_are_picked_up() {
        let mut openai = UsageSniffer::new(Format::OpenAi);
        openai.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
        openai.feed(b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\n");
        openai.feed(b"data: [DONE]\n\n");
        assert_eq!(
            openai.native_usage()["prompt_tokens_details"]["cached_tokens"],
            json!(64)
        );

        let mut gemini = UsageSniffer::new(Format::Gemini);
        gemini.feed(b"data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":50,\"cachedContentTokenCount\":40}}\n\n");
        let canonical =
            crate::translate::canonical_usage_from_native(&gemini.native_usage(), Format::Gemini);
        assert_eq!(canonical["prompt_tokens"], json!(50));
        assert_eq!(canonical["cached_tokens"], json!(40));
    }

    #[test]
    fn junk_and_oversized_frames_do_not_poison_the_sniffer() {
        let mut sniffer = UsageSniffer::new(Format::OpenAi);
        sniffer.feed(b"data: not json at all\n\n");
        sniffer.feed(&vec![b'x'; SNIFFER_MAX_PENDING + 1024]);
        sniffer.feed(b"\ndata: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n");
        assert_eq!(sniffer.native_usage()["prompt_tokens"], json!(5));
    }
}

#[cfg(test)]
mod observed_tests {
    use super::*;

    #[test]
    fn observed_only_after_a_usage_frame_arrives() {
        let mut sniffer = UsageSniffer::new(Format::Claude);
        // Text deltas alone must not count as observed usage.
        sniffer.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n");
        assert!(!sniffer.observed(), "an abandoned stream has no usage yet");
        sniffer.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":900}}}\n\n");
        assert!(sniffer.observed());
    }
}

#[cfg(test)]
mod abort_frame_tests {
    use super::*;

    fn text(format: Format, message: &str) -> String {
        String::from_utf8_lossy(&abort_terminal_frames(format, message)).to_string()
    }

    #[test]
    fn every_client_format_gets_a_terminal_frame_it_can_parse() {
        // An OpenAI-compatible client raises on a data payload carrying `error`,
        // and only then stops at [DONE].
        let openai = text(Format::OpenAi, "upstream stalled");
        assert!(openai.contains("\"error\""), "{openai}");
        assert!(openai.contains("upstream stalled"), "{openai}");
        assert!(openai.ends_with("data: [DONE]\n\n"), "{openai}");

        // Anthropic clients need a named event, not a bare data frame.
        let claude = text(Format::Claude, "upstream stalled");
        assert!(claude.starts_with("event: error\ndata: "), "{claude}");
        assert!(claude.contains("\"type\":\"error\""), "{claude}");

        // A Responses client is waiting for a terminal event of its own protocol.
        let responses = text(Format::Responses, "stream closed");
        assert!(responses.contains("event: response.failed"), "{responses}");
        assert!(responses.contains("\"status\":\"failed\""), "{responses}");
        assert!(responses.ends_with("data: [DONE]\n\n"), "{responses}");

        // Gemini callers parse the Google error envelope.
        let gemini = text(Format::Gemini, "upstream stalled");
        assert!(gemini.contains("GATEWAY_TIMEOUT"), "{gemini}");
    }
}
