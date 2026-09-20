use crate::error::AppError;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    OpenAi,
    Claude,
    Gemini,
    Responses,
}
impl Format {
    pub fn from_provider(s: &str) -> Self {
        match s {
            "claude" => Self::Claude,
            "gemini" => Self::Gemini,
            "openai-responses" | "responses" => Self::Responses,
            _ => Self::OpenAi,
        }
    }
}

pub fn caller_for_path(path: &str) -> Format {
    if path.contains("/messages") {
        Format::Claude
    } else if path.starts_with("/v1beta") || path.starts_with("/api/v1beta") {
        Format::Gemini
    } else if path.ends_with("/responses") || path == "/responses" || path.starts_with("/codex") {
        Format::Responses
    } else {
        Format::OpenAi
    }
}

pub fn normalize_request(body: Value, caller: Format) -> Result<Value, AppError> {
    if !body.is_object() {
        return Err(AppError::BadRequest(
            "JSON request body must be an object".into(),
        ));
    }
    match caller {
        Format::OpenAi => Ok(body),
        Format::Claude => claude_to_openai_request(body),
        Format::Gemini => gemini_to_openai_request(body),
        Format::Responses => responses_to_openai_request(body),
    }
}
pub fn provider_request(openai: Value, provider: Format) -> Result<Value, AppError> {
    provider_request_with_cache_ttl(openai, provider, CacheTtl::default())
}
pub fn provider_request_with_cache_ttl(
    openai: Value,
    provider: Format,
    cache_ttl: CacheTtl,
) -> Result<Value, AppError> {
    match provider {
        Format::OpenAi => Ok(openai),
        Format::Claude => openai_to_claude_request(openai, cache_ttl),
        Format::Gemini => openai_to_gemini_request(openai),
        Format::Responses => openai_to_responses_request(openai),
    }
}

/// TTL for the Anthropic cache breakpoints this gateway writes. A 5-minute
/// write costs 1.25x base input and a 1-hour write costs 2x, while a read costs
/// 0.1x either way, so the cheaper 5-minute TTL wins whenever turns arrive
/// faster than the cache expires. Deployments whose session gaps exceed five
/// minutes can set `promptCacheTtl` to `"1h"` in settings to stop paying a
/// fresh write on every turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
    /// No breakpoints at all, for a Claude-compatible gateway that rejects
    /// fields it does not recognise.
    Disabled,
}
impl CacheTtl {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("1h") | Some("one-hour") | Some("one_hour") => Self::OneHour,
            Some("off") | Some("none") | Some("disabled") => Self::Disabled,
            _ => Self::FiveMinutes,
        }
    }
    fn marker(self) -> Option<Value> {
        match self {
            Self::FiveMinutes => Some(json!({"type": "ephemeral"})),
            Self::OneHour => Some(json!({"type": "ephemeral", "ttl": "1h"})),
            Self::Disabled => None,
        }
    }
}
pub fn normalize_response(body: Value, provider: Format) -> Result<Value, AppError> {
    match provider {
        Format::OpenAi => Ok(body),
        Format::Claude => claude_to_openai_response(body),
        Format::Gemini => gemini_to_openai_response(body),
        Format::Responses => responses_to_openai_response(body),
    }
}
pub fn caller_response(openai: Value, caller: Format) -> Result<Value, AppError> {
    match caller {
        Format::OpenAi => Ok(openai),
        Format::Claude => openai_to_claude_response(openai),
        Format::Gemini => openai_to_gemini_response(openai),
        Format::Responses => openai_to_responses_response(openai),
    }
}

/// Canonical usage from one provider-native usage object.
///
/// Non-streaming responses reach it through the converters below; the streaming
/// passthrough path has no response body to convert and calls it directly with
/// whatever the sniffer collected, so both weigh cache tokens the same way.
pub fn canonical_usage_from_native(native: &Value, provider: Format) -> Value {
    match provider {
        // input_tokens excludes the cache subsets.
        Format::Claude => {
            let (cached, cache_creation) = cache_tokens_from_usage(native);
            canonical_usage(
                token_count(native, "input_tokens") + cached + cache_creation,
                token_count(native, "output_tokens"),
                cached,
                cache_creation,
                0,
            )
        }
        // promptTokenCount already includes cachedContentTokenCount, and
        // thinking tokens are reported apart from candidates.
        Format::Gemini => {
            let cached = token_count(native, "cachedContentTokenCount");
            let thoughts = token_count(native, "thoughtsTokenCount");
            let total = token_count(native, "totalTokenCount");
            let mut candidates = token_count(native, "candidatesTokenCount");
            if candidates == 0 && total > 0 {
                candidates = (total - token_count(native, "promptTokenCount") - thoughts).max(0);
            }
            canonical_usage(
                token_count(native, "promptTokenCount"),
                candidates + thoughts,
                cached,
                0,
                thoughts,
            )
        }
        // input_tokens already includes input_tokens_details.cached_tokens.
        Format::Responses => {
            let cached = native
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let reasoning = native
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            canonical_usage(
                token_count(native, "input_tokens"),
                token_count(native, "output_tokens"),
                cached,
                0,
                reasoning,
            )
        }
        // prompt_tokens already includes prompt_tokens_details.cached_tokens.
        Format::OpenAi => {
            let (cached, cache_creation) = cache_tokens_from_usage(native);
            let reasoning = token_count(native, "reasoning_tokens").max(
                native
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            );
            canonical_usage(
                token_count(native, "prompt_tokens").max(token_count(native, "input_tokens")),
                token_count(native, "completion_tokens").max(token_count(native, "output_tokens")),
                cached,
                cache_creation,
                reasoning,
            )
        }
    }
}

fn text_from_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| {
                let ty = b.get("type").and_then(Value::as_str);
                match ty {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        b.get("text").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn token_count(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Canonical usage, following `frontend/open-sse/utils/usageTracking.js`: the
/// prompt side is cache-INCLUSIVE (`cached_tokens` and
/// `cache_creation_input_tokens` are subsets of `prompt_tokens`), which is what
/// the dashboard's hit-rate column and `calculateCostFromTokens` both assume.
/// Providers whose own `input_tokens` excludes cache (Claude) fold it in at the
/// edges; providers that already include it pass it through. Cache fields are
/// only emitted when present so responses keep their previous shape on a miss.
fn canonical_usage(
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: i64,
    cache_creation_tokens: i64,
    reasoning_tokens: i64,
) -> Value {
    let mut usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    });
    if cached_tokens > 0 {
        usage["cached_tokens"] = json!(cached_tokens);
    }
    if cache_creation_tokens > 0 {
        usage["cache_creation_input_tokens"] = json!(cache_creation_tokens);
    }
    if cached_tokens > 0 || cache_creation_tokens > 0 {
        let mut details = Map::new();
        if cached_tokens > 0 {
            details.insert("cached_tokens".into(), json!(cached_tokens));
        }
        if cache_creation_tokens > 0 {
            details.insert("cache_creation_tokens".into(), json!(cache_creation_tokens));
        }
        usage["prompt_tokens_details"] = Value::Object(details);
    }
    if reasoning_tokens > 0 {
        usage["reasoning_tokens"] = json!(reasoning_tokens);
        usage["completion_tokens_details"] = json!({"reasoning_tokens": reasoning_tokens});
    }
    usage
}

fn cache_tokens_from_usage(usage: &Value) -> (i64, i64) {
    let cached = usage
        .get("cached_tokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_i64))
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cache_creation_tokens")
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    (cached, cache_creation)
}

/// The cache-inclusive token record handed to the usage ledger and to
/// `pricing::cost_from_tokens`. Kept separate from the client-facing usage so a
/// response converted back to Claude shape can subtract the cache subsets again.
pub fn stored_tokens(usage: &Value) -> Value {
    let prompt = token_count(usage, "prompt_tokens");
    let completion = token_count(usage, "completion_tokens");
    let (cached, cache_creation) = cache_tokens_from_usage(usage);
    let reasoning = token_count(usage, "reasoning_tokens").max(
        usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    canonical_usage(prompt, completion, cached, cache_creation, reasoning)
}

/// A Claude image block as a data/URL string.
fn claude_image_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) == Some("base64") {
        return Some(format!(
            "data:{};base64,{}",
            source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream"),
            source.get("data").and_then(Value::as_str).unwrap_or("")
        ));
    }
    source
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

struct ToolResultContent {
    text: String,
    images: Vec<Value>,
}

/// Split a `tool_result` body into the text the OpenAI tool role can carry and
/// the images that have to travel as user content.
fn split_tool_result_content(content: Option<&Value>) -> ToolResultContent {
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    match content {
        Some(Value::String(text)) => texts.push(text.clone()),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        }
                    }
                    Some("image") => {
                        if let Some(url) = claude_image_url(part) {
                            images.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    ToolResultContent {
        text: texts.join(
            "
",
        ),
        images,
    }
}

fn claude_to_openai_request(mut b: Value) -> Result<Value, AppError> {
    let o = b
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Claude request must be object".into()))?;
    let mut out = Map::new();
    if let Some(v) = o.remove("model") {
        out.insert("model".into(), v);
    }
    if let Some(v) = o.remove("max_tokens") {
        out.insert("max_tokens".into(), v);
    }
    if let Some(v) = o.remove("temperature") {
        out.insert("temperature".into(), v);
    }
    if let Some(v) = o.remove("top_p") {
        out.insert("top_p".into(), v);
    }
    if let Some(v) = o.remove("stop_sequences") {
        out.insert("stop".into(), v);
    }
    if let Some(v) = o.remove("stream") {
        out.insert("stream".into(), v);
    }
    let mut messages = Vec::new();
    if let Some(system) = o.remove("system") {
        messages.push(json!({"role":"system","content":text_from_content(&system)}))
    }
    for m in o
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m
            .get("content")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        // Images lifted out of tool results, emitted after the tool messages so a
        // tool call still reads as tool-result-then-evidence.
        let mut hoisted: Vec<Value> = Vec::new();
        match content {
            Value::String(s) => text_parts.push(Value::String(s)),
            Value::Array(blocks) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => text_parts
                            .push(json!({"type": "text", "text": block.get("text").cloned().unwrap_or(json!(""))})),
                        Some("image") => {
                            if let Some(url) = claude_image_url(&block) {
                                text_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                        }
                        Some("tool_use") => tool_calls.push(json!({"id":block.get("id").cloned().unwrap_or(json!("")),"type":"function","function":{"name":block.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_|"{}".into())}})),
                        Some("tool_result") => {
                            let split = split_tool_result_content(block.get("content"));
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                                "content": split.text
                            }));
                            // The OpenAI tool role is text-only, so a screenshot a tool
                            // returned would vanish. Hand it to the model in a user turn
                            // after the tool messages, tagged with the call it came from.
                            // Anthropic-compatible endpoints accept an image as user
                            // content but silently drop one inside a tool result, so this
                            // placement serves both upstream families.
                            if !split.images.is_empty() {
                                hoisted.push(json!({
                                    "type": "text",
                                    "text": format!(
                                        "[Image from tool result {}]",
                                        block.get("tool_use_id").and_then(Value::as_str).unwrap_or("")
                                    )
                                }));
                                hoisted.extend(split.images);
                            }
                        }
                        Some("thinking") => text_parts
                            .push(json!({"type": "text", "text": block.get("thinking").cloned().unwrap_or(json!(""))})),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let content = if text_parts.len() == 1 && text_parts[0].is_string() {
            text_parts.remove(0)
        } else {
            Value::Array(
                text_parts
                    .into_iter()
                    .map(|v| {
                        if v.is_string() {
                            json!({"type":"text","text":v})
                        } else {
                            v
                        }
                    })
                    .collect(),
            )
        };
        let mut msg = json!({"role":role,"content":content});
        if !tool_calls.is_empty() {
            msg["tool_calls"] = Value::Array(tool_calls)
        }
        messages.push(msg);
        messages.extend(tool_results);
        if !hoisted.is_empty() {
            messages.push(json!({"role": "user", "content": hoisted}));
        }
    }
    out.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = o.remove("tools") {
        let arr=tools.as_array().cloned().unwrap_or_default().into_iter().map(|t|json!({"type":"function","function":{"name":t.get("name").cloned().unwrap_or(json!("")),"description":t.get("description").cloned().unwrap_or(json!("")),"parameters":t.get("input_schema").cloned().unwrap_or(json!({"type":"object"}))}})).collect();
        out.insert("tools".into(), Value::Array(arr));
    }
    if let Some(tc) = o.remove("tool_choice") {
        let v = match tc.get("type").and_then(Value::as_str) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("tool") => {
                json!({"type":"function","function":{"name":tc.get("name").cloned().unwrap_or(json!(""))}})
            }
            _ => tc,
        };
        out.insert("tool_choice".into(), v);
    }
    for (k, v) in o.iter() {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn openai_to_claude_request(mut b: Value, cache_ttl: CacheTtl) -> Result<Value, AppError> {
    let object = b
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("OpenAI request must be object".into()))?;
    let mut out = Map::new();

    for key in ["model", "temperature", "top_p", "stream"] {
        if let Some(value) = object.remove(key) {
            out.insert(key.to_string(), value);
        }
    }
    if let Some(value) = object
        .remove("max_tokens")
        .or_else(|| object.remove("max_completion_tokens"))
    {
        out.insert("max_tokens".into(), value);
    } else {
        out.insert("max_tokens".into(), json!(4096));
    }

    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in object
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let message_content = message.get("content").cloned().unwrap_or(json!(""));

        if role == "system" || role == "developer" {
            let text = text_from_content(&message_content);
            if !text.is_empty() {
                system.push(json!({"type": "text", "text": text}));
            }
            continue;
        }

        if role == "tool" {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message
                        .get("tool_call_id")
                        .cloned()
                        .unwrap_or(json!("")),
                    "content": text_from_content(&message_content)
                }]
            }));
            continue;
        }

        let mut blocks = Vec::new();
        match &message_content {
            Value::String(text) => {
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            Value::Array(parts) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") | Some("input_text") => {
                            blocks.push(json!({
                                "type": "text",
                                "text": part.get("text").cloned().unwrap_or(json!(""))
                            }));
                        }
                        Some("image_url") => {
                            if let Some(url) =
                                part.pointer("/image_url/url").and_then(Value::as_str)
                            {
                                if let Some(rest) = url.strip_prefix("data:") {
                                    if let Some((meta, data)) = rest.split_once(',') {
                                        blocks.push(json!({
                                            "type": "image",
                                            "source": {
                                                "type": "base64",
                                                "media_type": meta.trim_end_matches(";base64"),
                                                "data": data
                                            }
                                        }));
                                    }
                                } else {
                                    blocks.push(json!({
                                        "type": "image",
                                        "source": {"type": "url", "url": url}
                                    }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let input = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.get("id").cloned().unwrap_or(json!("")),
                    "name": call
                        .pointer("/function/name")
                        .cloned()
                        .unwrap_or(json!("")),
                    "input": input
                }));
            }
        }

        messages.push(json!({
            "role": if role == "assistant" { "assistant" } else { "user" },
            "content": blocks
        }));
    }

    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    out.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = object.remove("tools") {
        let mapped = tools
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| tool.get("function").cloned())
            .map(|function| {
                json!({
                    "name": function.get("name").cloned().unwrap_or(json!("")),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(json!("")),
                    "input_schema": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object"}))
                })
            })
            .collect::<Vec<_>>();
        out.insert("tools".into(), Value::Array(mapped));
    }

    if let Some(choice) = object.remove("tool_choice") {
        let mapped = match choice.as_str() {
            Some("auto") => json!({"type": "auto"}),
            Some("required") => json!({"type": "any"}),
            _ if choice.get("type").and_then(Value::as_str) == Some("function") => json!({
                "type": "tool",
                "name": choice
                    .pointer("/function/name")
                    .cloned()
                    .unwrap_or(json!(""))
            }),
            _ => choice,
        };
        out.insert("tool_choice".into(), mapped);
    }

    for (key, value) in object.iter() {
        if !out.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }

    apply_claude_cache_breakpoints(&mut out, cache_ttl);

    Ok(Value::Object(out))
}

/// Anthropic prompt caching is opt-in and prefix-exact: a request without
/// `cache_control` markers pays full input price for the whole re-sent history
/// on every turn, and the client's own markers do not survive the
/// Claude -> OpenAI -> Claude round trip this gateway performs. Anchor the two
/// boundaries a follow-up turn can reuse:
///
/// - the end of the stable head, which by the Messages API's `tools` -> `system`
///   -> `messages` prefix order covers both the tool declarations and the
///   system prompt;
/// - the end of the newest turn, so the next request reads it back at 0.1x
///   instead of writing the prefix again.
///
/// Two markers stay inside the four-breakpoint budget, and a prefix shorter
/// than the model's minimum cacheable length is simply not cached (no error).
fn apply_claude_cache_breakpoints(out: &mut Map<String, Value>, cache_ttl: CacheTtl) {
    let Some(marker) = cache_ttl.marker() else {
        return;
    };
    let mut marked = 0usize;

    if let Some(system) = out.get_mut("system").and_then(Value::as_array_mut) {
        if let Some(block) = system.last_mut() {
            if set_cache_control(block, &marker) {
                marked += 1;
            }
        }
    } else if let Some(tools) = out.get_mut("tools").and_then(Value::as_array_mut) {
        if let Some(tool) = tools.last_mut() {
            if set_cache_control(tool, &marker) {
                marked += 1;
            }
        }
    }

    if marked < 4 {
        if let Some(blocks) =
            out.get_mut("messages")
                .and_then(Value::as_array_mut)
                .and_then(|messages| {
                    messages.iter_mut().rev().find_map(|message| {
                        message.get_mut("content").and_then(Value::as_array_mut)
                    })
                })
        {
            let index = blocks.iter().rposition(is_cacheable_claude_block);
            if let Some(block) = index.and_then(|index| blocks.get_mut(index)) {
                set_cache_control(block, &marker);
            }
        }
    }
}

fn set_cache_control(block: &mut Value, marker: &Value) -> bool {
    let Some(object) = block.as_object_mut() else {
        return false;
    };
    if object.contains_key("cache_control") {
        return false;
    }
    object.insert("cache_control".into(), marker.clone());
    true
}

/// `cache_control` is accepted on text, image, document, tool_use and
/// tool_result blocks; thinking blocks reject it.
fn is_cacheable_claude_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("text") | Some("image") | Some("document") | Some("tool_use") | Some("tool_result")
    )
}

/// Stable, position-derived ids for tool calls rebuilt out of a Gemini history,
/// which carries none of its own. `for_call` allocates the id a `functionCall`
/// reports; `for_response` returns the id of the call it answers, so a replayed
/// history stays internally consistent while producing identical bytes for
/// identical input.
#[derive(Default)]
struct ToolCallIds {
    issued: usize,
    pending: std::collections::HashMap<String, std::collections::VecDeque<String>>,
}
impl ToolCallIds {
    fn for_call(&mut self, name: &str, existing: Option<&str>) -> String {
        let id = match existing.map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                self.issued += 1;
                format!("call_{}_{}", self.issued, name)
            }
        };
        self.pending
            .entry(name.to_string())
            .or_default()
            .push_back(id.clone());
        id
    }
    fn for_response(&mut self, name: &str, existing: Option<&str>) -> String {
        if let Some(id) = existing.map(str::trim).filter(|id| !id.is_empty()) {
            return id.to_string();
        }
        if let Some(id) = self
            .pending
            .get_mut(name)
            .and_then(|queue| queue.pop_front())
        {
            return id;
        }
        self.issued += 1;
        format!("call_{}_{}", self.issued, name)
    }
}

fn gemini_to_openai_request(b: Value) -> Result<Value, AppError> {
    let model = b.get("model").cloned().unwrap_or(json!(""));
    let stream = b.get("stream").cloned().unwrap_or(json!(true));
    let mut messages = Vec::new();
    // Gemini's history carries no tool-call ids, so they have to be invented. A
    // counter and a per-name queue keep them stable across requests: a fresh
    // uuid here rewrote every earlier turn on every call, which made the prefix
    // worthless to a provider's implicit cache, and left a replayed history
    // unable to correlate a functionResponse with the call it answers.
    let mut call_ids = ToolCallIds::default();
    if let Some(sys) = b.get("systemInstruction") {
        messages
            .push(json!({"role":"system","content":parts_text(sys.get("parts").unwrap_or(sys))}))
    }
    for c in b
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = if c.get("role").and_then(Value::as_str) == Some("model") {
            "assistant"
        } else {
            "user"
        };
        let mut content = Vec::new();
        for p in c
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(t) = p.get("text") {
                content.push(json!({"type":"text","text":t}))
            } else if let Some(d) = p.get("inlineData") {
                content.push(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",d.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream"),d.get("data").and_then(Value::as_str).unwrap_or(""))}}))
            } else if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
                let id = call_ids.for_call(name, fc.get("id").and_then(Value::as_str));
                content.push(json!({"type":"text","text":""}));
                messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":fc.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(fc.get("args").unwrap_or(&json!({}))).unwrap()}}]}))
            } else if let Some(fr) = p.get("functionResponse") {
                let name = fr.get("name").and_then(Value::as_str).unwrap_or("");
                let id = call_ids.for_response(name, fr.get("id").and_then(Value::as_str));
                messages.push(json!({"role":"tool","tool_call_id":id,"content":serde_json::to_string(fr.get("response").unwrap_or(&json!({}))).unwrap()}))
            }
        }
        if !content.is_empty() {
            messages.push(json!({"role":role,"content":content}))
        }
    }
    let mut out = json!({"model":model,"messages":messages,"stream":stream});
    if let Some(gc) = b.get("generationConfig") {
        if let Some(v) = gc.get("temperature") {
            out["temperature"] = v.clone()
        }
        if let Some(v) = gc.get("topP") {
            out["top_p"] = v.clone()
        }
        if let Some(v) = gc.get("maxOutputTokens") {
            out["max_tokens"] = v.clone()
        }
        if let Some(v) = gc.get("stopSequences") {
            out["stop"] = v.clone()
        }
    }
    if let Some(tools) = b.get("tools").and_then(Value::as_array) {
        let mut funcs = Vec::new();
        for t in tools {
            for f in t
                .get("functionDeclarations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                funcs.push(json!({"type":"function","function":{"name":f.get("name").cloned().unwrap_or(json!("")),"description":f.get("description").cloned().unwrap_or(json!("")),"parameters":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))}}))
            }
        }
        out["tools"] = Value::Array(funcs)
    }
    Ok(out)
}
fn parts_text(v: &Value) -> String {
    if let Some(a) = v.as_array() {
        a.iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    } else {
        v.get("text").and_then(Value::as_str).unwrap_or("").into()
    }
}

fn openai_to_gemini_request(b: Value) -> Result<Value, AppError> {
    let mut contents = Vec::new();
    let mut system_parts = Vec::new();

    for message in b
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = message.get("content").cloned().unwrap_or(json!(""));

        if role == "system" || role == "developer" {
            let text = text_from_content(&content);
            if !text.is_empty() {
                system_parts.push(json!({"text": text}));
            }
            continue;
        }

        let mut parts = Vec::new();
        match &content {
            Value::String(text) => {
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            Value::Array(items) => {
                for item in items {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(json!({"text": text}));
                        continue;
                    }
                    if let Some(url) = item.pointer("/image_url/url").and_then(Value::as_str) {
                        if let Some(rest) = url.strip_prefix("data:") {
                            if let Some((meta, data)) = rest.split_once(',') {
                                parts.push(json!({
                                    "inlineData": {
                                        "mimeType": meta.trim_end_matches(";base64"),
                                        "data": data
                                    }
                                }));
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(json!({}));
                parts.push(json!({
                    "functionCall": {
                        "name": call
                            .pointer("/function/name")
                            .cloned()
                            .unwrap_or(json!("")),
                        "args": args
                    }
                }));
            }
        }

        if role == "tool" {
            let function_name = message
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            parts.push(json!({
                "functionResponse": {
                    "name": function_name,
                    "response": {"content": text_from_content(&content)}
                }
            }));
        }

        if parts.is_empty() {
            parts.push(json!({"text": ""}));
        }
        contents.push(json!({
            "role": if role == "assistant" { "model" } else { "user" },
            "parts": parts
        }));
    }

    let mut out = json!({"contents": contents});
    if !system_parts.is_empty() {
        out["systemInstruction"] = json!({"parts": system_parts});
    }

    let mut generation_config = Map::new();
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_tokens", "maxOutputTokens"),
        ("stop", "stopSequences"),
    ] {
        if let Some(value) = b.get(source) {
            generation_config.insert(target.to_string(), value.clone());
        }
    }
    if !generation_config.is_empty() {
        out["generationConfig"] = Value::Object(generation_config);
    }

    if let Some(tools) = b.get("tools").and_then(Value::as_array) {
        let declarations = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                json!({
                    "name": function.get("name").cloned().unwrap_or(json!("")),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(json!("")),
                    "parameters": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object"}))
                })
            })
            .collect::<Vec<_>>();
        if !declarations.is_empty() {
            out["tools"] = json!([{"functionDeclarations": declarations}]);
        }
    }

    Ok(out)
}
fn responses_to_openai_request(b: Value) -> Result<Value, AppError> {
    let mut messages = Vec::new();
    if let Some(inst) = b.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role":"system","content":inst}))
    }
    match b.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role":"user","content":s})),
        Some(Value::Array(items)) => {
            for i in items {
                let ty = i.get("type").and_then(Value::as_str).unwrap_or("message");
                if ty == "message" {
                    messages.push(json!({"role":i.get("role").cloned().unwrap_or(json!("user")),"content":i.get("content").cloned().unwrap_or(json!(""))}))
                } else if ty == "function_call_output" {
                    messages.push(json!({"role":"tool","tool_call_id":i.get("call_id").cloned().unwrap_or(json!("")),"content":i.get("output").cloned().unwrap_or(json!(""))}))
                }
            }
        }
        _ => {}
    }
    let mut out = json!({"model":b.get("model").cloned().unwrap_or(json!("")),"messages":messages,"stream":b.get("stream").cloned().unwrap_or(json!(false))});
    if let Some(v) = b.get("temperature") {
        out["temperature"] = v.clone()
    }
    if let Some(v) = b.get("max_output_tokens") {
        out["max_tokens"] = v.clone()
    }
    if let Some(v) = b.get("tools") {
        out["tools"] = v.clone()
    }
    if let Some(v) = b.get("tool_choice") {
        out["tool_choice"] = v.clone()
    }
    Ok(out)
}

fn openai_to_responses_request(b: Value) -> Result<Value, AppError> {
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for m in b
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "system" || role == "developer" {
            instructions.push(text_from_content(m.get("content").unwrap_or(&json!(""))));
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":m.get("tool_call_id").cloned().unwrap_or(json!("")),"output":text_from_content(m.get("content").unwrap_or(&json!("")))}));
            continue;
        }
        input.push(json!({"type":"message","role":role,"content":m.get("content").cloned().unwrap_or(json!(""))}));
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                input.push(json!({"type":"function_call","call_id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"arguments":c.pointer("/function/arguments").cloned().unwrap_or(json!("{}"))}))
            }
        }
    }
    let mut out = json!({"model":b.get("model").cloned().unwrap_or(json!("")),"input":input,"stream":b.get("stream").cloned().unwrap_or(json!(false))});
    if !instructions.is_empty() {
        out["instructions"] = json!(instructions.join("\n\n"))
    }
    if let Some(v) = b.get("tools") {
        out["tools"] = v.clone()
    }
    if let Some(v) = b.get("tool_choice") {
        out["tool_choice"] = v.clone()
    }
    if let Some(v) = b.get("temperature") {
        out["temperature"] = v.clone()
    }
    if let Some(v) = b.get("max_tokens") {
        out["max_output_tokens"] = v.clone()
    }
    Ok(out)
}

fn claude_to_openai_response(b: Value) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for c in b
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match c.get("type").and_then(Value::as_str){Some("text")=>text.push_str(c.get("text").and_then(Value::as_str).unwrap_or("")),Some("tool_use")=>calls.push(json!({"id":c.get("id").cloned().unwrap_or(json!("")),"type":"function","function":{"name":c.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(c.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_|"{}".into())}})),_=>{}}
    }
    let stop = match b.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    };
    let usage = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":b.get("model").cloned().unwrap_or(json!("")),"choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":stop}],"usage":canonical_usage_from_native(&usage, Format::Claude)}),
    )
}
fn gemini_to_openai_response(b: Value) -> Result<Value, AppError> {
    let c = b
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let mut text = String::new();
    let mut calls = Vec::new();
    for p in c
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            text.push_str(t)
        }
        if let Some(fc) = p.get("functionCall") {
            calls.push(json!({"id":format!("call_{}",uuid::Uuid::new_v4().simple()),"type":"function","function":{"name":fc.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(fc.get("args").unwrap_or(&json!({}))).unwrap()}}))
        }
    }
    let fr = match c.get("finishReason").and_then(Value::as_str) {
        Some("MAX_TOKENS") => "length",
        _ if !calls.is_empty() => "tool_calls",
        _ => "stop",
    };
    let u = b.get("usageMetadata").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()),"object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":fr}],"usage":canonical_usage_from_native(&u, Format::Gemini)}),
    )
}
fn responses_to_openai_response(b: Value) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for i in b
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match i.get("type").and_then(Value::as_str){Some("message")=>for c in i.get("content").and_then(Value::as_array).cloned().unwrap_or_default(){if let Some(t)=c.get("text").and_then(Value::as_str){text.push_str(t)}},Some("function_call")=>calls.push(json!({"id":i.get("call_id").or_else(||i.get("id")).cloned().unwrap_or(json!("")),"type":"function","function":{"name":i.get("name").cloned().unwrap_or(json!("")),"arguments":i.get("arguments").cloned().unwrap_or(json!("{}"))}})),_=>{}}
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":b.get("model").cloned().unwrap_or(json!("")),"choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":canonical_usage_from_native(&u, Format::Responses)}),
    )
}

fn openai_to_claude_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        content.push(json!({"type":"text","text":t}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let input = c
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(json!({}));
        content.push(json!({"type":"tool_use","id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"input":input}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    // Claude clients expect input_tokens to exclude the cache subsets, which the
    // canonical prompt count folds in (see `openai-to-claude.js`).
    let (cached, cache_creation) = cache_tokens_from_usage(&u);
    let mut usage = json!({
        "input_tokens": (token_count(&u, "prompt_tokens") - cached - cache_creation).max(0),
        "output_tokens": token_count(&u, "completion_tokens"),
    });
    if cached > 0 {
        usage["cache_read_input_tokens"] = json!(cached);
    }
    if cache_creation > 0 {
        usage["cache_creation_input_tokens"] = json!(cache_creation);
    }
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("msg_{}",uuid::Uuid::new_v4().simple()))),"type":"message","role":"assistant","model":b.get("model").cloned().unwrap_or(json!("")),"content":content,"stop_reason":match choice.get("finish_reason").and_then(Value::as_str){Some("tool_calls")=>"tool_use",Some("length")=>"max_tokens",_=>"end_turn"},"stop_sequence":null,"usage":usage}),
    )
}
fn openai_to_gemini_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut parts = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        parts.push(json!({"text":t}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let args = c
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(json!({}));
        parts.push(json!({"functionCall":{"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"args":args}}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    let (cached, _) = cache_tokens_from_usage(&u);
    let reasoning = token_count(&u, "reasoning_tokens").max(
        u.pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    let mut usage = json!({
        "promptTokenCount": token_count(&u, "prompt_tokens"),
        "candidatesTokenCount": token_count(&u, "completion_tokens") - reasoning,
        "totalTokenCount": token_count(&u, "total_tokens"),
    });
    if reasoning > 0 {
        usage["thoughtsTokenCount"] = json!(reasoning);
    }
    if cached > 0 {
        usage["cachedContentTokenCount"] = json!(cached);
    }
    Ok(
        json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":match choice.get("finish_reason").and_then(Value::as_str){Some("length")=>"MAX_TOKENS",_=>"STOP"},"index":0}],"usageMetadata":usage}),
    )
}
fn openai_to_responses_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut output = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        output.push(json!({"type":"message","id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"status":"completed","role":"assistant","content":[{"type":"output_text","text":t,"annotations":[]}]}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        output.push(json!({"type":"function_call","id":format!("fc_{}",uuid::Uuid::new_v4().simple()),"call_id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"arguments":c.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),"status":"completed"}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    // Responses input_tokens is cache-inclusive, so cached tokens ride along as
    // input_tokens_details rather than being subtracted.
    let (cached, _) = cache_tokens_from_usage(&u);
    let reasoning = token_count(&u, "reasoning_tokens").max(
        u.pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    );
    let mut usage = json!({
        "input_tokens": token_count(&u, "prompt_tokens"),
        "output_tokens": token_count(&u, "completion_tokens"),
        "total_tokens": token_count(&u, "total_tokens"),
    });
    if cached > 0 {
        usage["input_tokens_details"] = json!({"cached_tokens": cached});
    }
    if reasoning > 0 {
        usage["output_tokens_details"] = json!({"reasoning_tokens": reasoning});
    }
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("resp_{}",uuid::Uuid::new_v4().simple()))),"object":"response","status":"completed","model":b.get("model").cloned().unwrap_or(json!("")),"output":output,"usage":usage}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_request(messages: Value) -> Value {
        json!({"model":"claude-sonnet-4.5","max_tokens":1024,"messages":messages})
    }

    fn claude_breakpoints(body: &Value) -> Vec<Value> {
        let mut found = Vec::new();
        if let Some(system) = body.get("system").and_then(Value::as_array) {
            found.extend(
                system
                    .iter()
                    .filter_map(|b| b.get("cache_control").cloned()),
            );
        }
        if let Some(tools) = body.get("tools").and_then(Value::as_array) {
            found.extend(tools.iter().filter_map(|t| t.get("cache_control").cloned()));
        }
        if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            for message in messages {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    found.extend(
                        blocks
                            .iter()
                            .filter_map(|b| b.get("cache_control").cloned()),
                    );
                }
            }
        }
        found
    }

    #[test]
    fn claude_route_anchors_the_head_and_the_tail() {
        let body = provider_request(
            openai_request(json!([
                {"role":"system","content":"stable system prompt"},
                {"role":"user","content":"first turn"},
                {"role":"assistant","content":"first answer"},
                {"role":"user","content":"second turn"},
            ])),
            Format::Claude,
        )
        .unwrap();

        let markers = claude_breakpoints(&body);
        assert_eq!(markers.len(), 2, "head + tail: {body}");
        assert_eq!(markers[0]["type"], json!("ephemeral"));
        // The head anchor sits on the last system block, which the Messages API's
        // tools -> system -> messages prefix order makes cover the tools too.
        let system = body.get("system").and_then(Value::as_array).unwrap();
        assert!(system.last().unwrap().get("cache_control").is_some());
        // The tail anchor sits on the newest turn, which the next request reuses.
        let messages = body.get("messages").and_then(Value::as_array).unwrap();
        assert_eq!(
            messages.last().unwrap()["content"][0]["text"],
            "second turn"
        );
        assert!(messages
            .last()
            .unwrap()
            .pointer("/content/0/cache_control")
            .is_some());
    }

    #[test]
    fn tools_are_anchored_when_no_system_prompt_exists() {
        let mut request = openai_request(json!([{"role":"user","content":"hi"}]));
        request["tools"] = json!([
            {"type":"function","function":{"name":"read","description":"read a file","parameters":{"type":"object"}}},
        ]);
        let body = provider_request(request, Format::Claude).unwrap();
        let tools = body.get("tools").and_then(Value::as_array).unwrap();
        assert!(tools.last().unwrap().get("cache_control").is_some());
        assert_eq!(claude_breakpoints(&body).len(), 2);
    }

    #[test]
    fn one_hour_ttl_is_opt_in() {
        let request = openai_request(json!([{"role":"user","content":"hi"}]));
        let body = provider_request_with_cache_ttl(
            request,
            Format::Claude,
            CacheTtl::from_setting(Some("1h")),
        )
        .unwrap();
        let marker = body.pointer("/messages/0/content/0/cache_control").unwrap();
        assert_eq!(marker["ttl"], json!("1h"));
        assert_eq!(
            CacheTtl::from_setting(Some("5m")),
            CacheTtl::FiveMinutes,
            "unknown values fall back to the cheaper 5m write"
        );
    }

    #[test]
    fn cache_breakpoints_can_be_switched_off() {
        let request = openai_request(json!([{"role":"user","content":"hi"}]));
        let body = provider_request_with_cache_ttl(
            request,
            Format::Claude,
            CacheTtl::from_setting(Some("off")),
        )
        .unwrap();
        assert!(claude_breakpoints(&body).is_empty(), "{body}");
    }

    #[test]
    fn thinking_blocks_never_take_a_breakpoint() {
        assert!(!is_cacheable_claude_block(
            &json!({"type":"thinking","thinking":"..."})
        ));
        assert!(is_cacheable_claude_block(
            &json!({"type":"text","text":"hello"})
        ));
        assert!(!is_cacheable_claude_block(
            &json!({"type":"redacted_thinking"})
        ));
    }

    #[test]
    fn openai_requests_are_untouched_by_cache_markers() {
        let request = openai_request(json!([{"role":"system","content":"s"}]));
        let body = provider_request(request.clone(), Format::OpenAi).unwrap();
        assert_eq!(body, request);
    }

    #[test]
    fn gemini_history_tool_ids_are_stable_and_correlated() {
        let request = json!({
            "contents": [
                {"role":"user","parts":[{"text":"read the file"}]},
                {"role":"model","parts":[{"functionCall":{"name":"read","args":{"path":"a.txt"}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"read","response":{"content":"hello"}}}]},
            ],
        });
        let first = normalize_request(request.clone(), Format::Gemini).unwrap();
        let second = normalize_request(request, Format::Gemini).unwrap();
        assert_eq!(first, second, "identical history must rebuild identically");

        let messages = first.get("messages").and_then(Value::as_array).unwrap();
        let call_id = messages
            .iter()
            .find_map(|m| m.pointer("/tool_calls/0/id"))
            .and_then(Value::as_str)
            .unwrap();
        let result_id = messages
            .iter()
            .find_map(|m| {
                if m.get("role").and_then(Value::as_str) == Some("tool") {
                    m.get("tool_call_id")
                } else {
                    None
                }
            })
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(
            call_id, result_id,
            "a replayed history must stay correlated"
        );
        assert!(call_id.starts_with("call_"), "{call_id}");
    }

    #[test]
    fn explicit_gemini_call_ids_are_preserved() {
        let request = json!({
            "contents": [
                {"role":"model","parts":[{"functionCall":{"id":"call_client_1","name":"read","args":{}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"read","response":{}}}]},
            ],
        });
        let body = normalize_request(request, Format::Gemini).unwrap();
        let messages = body.get("messages").and_then(Value::as_array).unwrap();
        let call_id = messages
            .iter()
            .find_map(|m| m.pointer("/tool_calls/0/id"))
            .and_then(Value::as_str)
            .unwrap();
        let result_id = messages
            .iter()
            .find_map(|m| {
                if m.get("role").and_then(Value::as_str) == Some("tool") {
                    m.get("tool_call_id")
                } else {
                    None
                }
            })
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(call_id, "call_client_1");
        assert_eq!(result_id, "call_client_1");
    }

    #[test]
    fn claude_prompt_tokens_are_cache_inclusive() {
        let upstream = json!({
            "id":"msg_1","type":"message","content":[{"type":"text","text":"ok"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1000,"output_tokens":50,
                     "cache_read_input_tokens":20000,"cache_creation_input_tokens":4000},
        });
        let canonical = normalize_response(upstream, Format::Claude).unwrap();
        let usage = canonical.get("usage").unwrap();
        assert_eq!(usage["prompt_tokens"], json!(25000));
        assert_eq!(usage["cached_tokens"], json!(20000));
        assert_eq!(
            usage["prompt_tokens_details"]["cache_creation_tokens"],
            json!(4000)
        );

        // Back to Claude shape: input_tokens excludes the cache subsets again.
        let claude = caller_response(canonical, Format::Claude).unwrap();
        let usage = claude.get("usage").unwrap();
        assert_eq!(usage["input_tokens"], json!(1000));
        assert_eq!(usage["cache_read_input_tokens"], json!(20000));
        assert_eq!(usage["cache_creation_input_tokens"], json!(4000));
    }

    #[test]
    fn stored_tokens_are_what_the_ledger_and_pricing_read() {
        let claude = claude_to_openai_response(json!({
            "content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":10,"output_tokens":5,
                     "cache_read_input_tokens":900,"cache_creation_input_tokens":90},
        }))
        .unwrap();
        let tokens = stored_tokens(claude.get("usage").unwrap());
        assert_eq!(tokens["prompt_tokens"], json!(1000));
        assert_eq!(tokens["cached_tokens"], json!(900));
        assert_eq!(tokens["cache_creation_input_tokens"], json!(90));
        assert_eq!(tokens["total_tokens"], json!(1005));
    }

    #[test]
    fn responses_cache_tokens_survive_both_directions() {
        let upstream = json!({
            "id":"resp_1","model":"gpt-5.1","output":[],
            "usage":{"input_tokens":1000,"output_tokens":20,"total_tokens":1020,
                     "input_tokens_details":{"cached_tokens":800},
                     "output_tokens_details":{"reasoning_tokens":10}},
        });
        let canonical = normalize_response(upstream, Format::Responses).unwrap();
        assert_eq!(canonical["usage"]["cached_tokens"], json!(800));
        assert_eq!(canonical["usage"]["prompt_tokens"], json!(1000));

        let responses = caller_response(canonical, Format::Responses).unwrap();
        assert_eq!(
            responses["usage"]["input_tokens_details"]["cached_tokens"],
            json!(800)
        );
        assert_eq!(responses["usage"]["output_tokens"], json!(20));
    }

    #[test]
    fn gemini_thoughts_and_cache_are_counted() {
        let upstream = json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":2000,"candidatesTokenCount":100,
                             "thoughtsTokenCount":40,"totalTokenCount":2140,
                             "cachedContentTokenCount":1500},
        });
        let canonical = normalize_response(upstream, Format::Gemini).unwrap();
        let usage = canonical.get("usage").unwrap();
        assert_eq!(usage["prompt_tokens"], json!(2000));
        assert_eq!(usage["completion_tokens"], json!(140));
        assert_eq!(usage["reasoning_tokens"], json!(40));
        assert_eq!(usage["cached_tokens"], json!(1500));
    }
}

#[cfg(test)]
mod round_trip_tests {
    use crate::translate::{normalize_request, provider_request, CacheTtl, Format};
    use serde_json::{json, Value};

    /// A Claude Code-shaped request: block system prompt with its own
    /// breakpoints, a tool, and a replayed history holding a tool_use/tool_result
    /// pair.
    fn claude_code_request() -> Value {
        json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 8192,
            "system": [
                {"type": "text", "text": "You are a coding agent.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Repository guidance: run the tests.", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ],
            "tools": [{
                "name": "read_file",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "read a.txt"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "I should read the file."},
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "a.txt"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "hello world"},
                ]},
                {"role": "user", "content": [{"type": "text", "text": "now summarize it"}]},
            ],
        })
    }

    #[test]
    fn claude_client_to_claude_provider_keeps_caching_and_correlation() {
        let canonical = normalize_request(claude_code_request(), Format::Claude).unwrap();
        let upstream = provider_request(canonical, Format::Claude).unwrap();

        // The round trip rebuilds the body, so the client's own breakpoints are
        // gone; the gateway's two anchors must take their place.
        let mut markers = Vec::new();
        if let Some(system) = upstream.get("system").and_then(Value::as_array) {
            for block in system {
                if let Some(marker) = block.get("cache_control") {
                    markers.push(marker.clone());
                }
            }
        }
        if let Some(tools) = upstream.get("tools").and_then(Value::as_array) {
            for tool in tools {
                if let Some(marker) = tool.get("cache_control") {
                    markers.push(marker.clone());
                }
            }
        }
        if let Some(messages) = upstream.get("messages").and_then(Value::as_array) {
            for message in messages {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if let Some(marker) = block.get("cache_control") {
                            markers.push(marker.clone());
                        }
                    }
                }
            }
        }
        assert_eq!(markers.len(), 2, "head + tail, inside the 4-marker budget");
        assert!(markers.iter().all(|m| m["type"] == json!("ephemeral")));
        assert!(
            markers.iter().all(|m| m.get("ttl").is_none()),
            "5m by default"
        );
        assert_eq!(markers[0]["ttl"], Value::Null);

        // Head anchor: the system block (tools + system share one prefix). The
        // rebuild merges the client's system blocks into one, so the anchor
        // covers exactly the bytes this gateway will resend next turn.
        let system = upstream.get("system").and_then(Value::as_array).unwrap();
        assert_eq!(system.len(), 1);
        assert!(system[0].get("cache_control").is_some());
        assert!(system[0]["text"]
            .as_str()
            .unwrap()
            .contains("Repository guidance"));

        // Tail anchor: the newest block, i.e. the final user text.
        let messages = upstream.get("messages").and_then(Value::as_array).unwrap();
        let last_blocks = messages.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(last_blocks[0]["text"], "now summarize it");
        assert!(last_blocks[0].get("cache_control").is_some());

        // The tool_use/tool_result pair still correlates, and the history is a
        // byte-stable rebuild: same input, same bytes.
        let serialized = serde_json::to_string(&upstream).unwrap();
        assert!(serialized.contains("toolu_01ABC"));
        let repeat = provider_request(
            normalize_request(claude_code_request(), Format::Claude).unwrap(),
            Format::Claude,
        )
        .unwrap();
        assert_eq!(upstream, repeat);
    }

    #[test]
    fn a_tiny_request_still_gets_anchors_that_anthropic_ignores() {
        // Below the minimum cacheable prefix Anthropic just does not cache, so
        // marking a small request costs nothing and needs no length heuristic.
        let canonical = normalize_request(
            json!({"model":"claude-sonnet-4.5","max_tokens":16,
                   "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}),
            Format::Claude,
        )
        .unwrap();
        let upstream = provider_request_with_ttl(canonical);
        let messages = upstream.get("messages").and_then(Value::as_array).unwrap();
        assert!(messages
            .last()
            .unwrap()
            .pointer("/content/0/cache_control")
            .is_some());
    }

    fn provider_request_with_ttl(canonical: Value) -> Value {
        provider_request(canonical, Format::Claude).unwrap()
    }

    #[test]
    fn one_hour_setting_reaches_the_wire() {
        let canonical = normalize_request(claude_code_request(), Format::Claude).unwrap();
        let upstream = crate::translate::provider_request_with_cache_ttl(
            canonical,
            Format::Claude,
            CacheTtl::OneHour,
        )
        .unwrap();
        let system = upstream.get("system").and_then(Value::as_array).unwrap();
        assert_eq!(system[0]["cache_control"]["ttl"], json!("1h"));
    }
}

#[cfg(test)]
mod tool_result_image_tests {
    use crate::translate::{normalize_request, Format};
    use serde_json::{json, Value};

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    /// Claude Code taking a screenshot: the image comes back inside a tool_result.
    fn screenshot_turn(tool_result_content: Value) -> Value {
        json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 4096,
            "tools": [{"name":"mcp__browser__computer","description":"browser","input_schema":{"type":"object","properties":{}}}],
            "messages": [
                {"role":"user","content":"take a screenshot"},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"mcp__browser__computer","input":{"action":"screenshot"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":tool_result_content}]},
            ],
        })
    }

    fn messages(body: &Value) -> Vec<Value> {
        body.get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn a_tool_result_image_reaches_the_upstream_as_user_content() {
        let body = normalize_request(
            screenshot_turn(json!([
                {"type":"text","text":"Successfully captured screenshot (1x1, png)"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
            ])),
            Format::Claude,
        )
        .unwrap();
        let messages = messages(&body);

        let tool = messages
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .expect("the tool result is still reported to the model");
        assert_eq!(tool["tool_call_id"], json!("toolu_1"));
        assert_eq!(
            tool["content"],
            json!("Successfully captured screenshot (1x1, png)")
        );
        assert!(
            !tool["content"].as_str().unwrap().contains(PNG),
            "base64 never leaks into the tool message"
        );

        // The image follows as user content, tagged with the call it came from.
        let follow = messages
            .iter()
            .position(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .map(|index| &messages[index + 1])
            .expect("a message follows the tool result");
        assert_eq!(follow["role"], json!("user"));
        let parts = follow["content"].as_array().unwrap();
        let image = parts
            .iter()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
            .expect("the screenshot is forwarded");
        assert_eq!(
            image["image_url"]["url"],
            json!(format!("data:image/png;base64,{PNG}"))
        );
        let tag = parts
            .iter()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .expect("the image is labelled");
        assert!(
            tag["text"].as_str().unwrap().contains("toolu_1"),
            "the model can tell which call produced the image"
        );
    }

    #[test]
    fn an_image_only_tool_result_does_not_dump_base64() {
        let body = normalize_request(
            screenshot_turn(json!([
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
            ])),
            Format::Claude,
        )
        .unwrap();
        let messages = messages(&body);
        let tool = messages
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .unwrap();
        assert_eq!(tool["content"], json!(""));
        assert!(messages.iter().any(|message| message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("image_url")))));
    }

    #[test]
    fn a_text_only_tool_result_is_unchanged() {
        let body =
            normalize_request(screenshot_turn(json!("plain result")), Format::Claude).unwrap();
        let messages = messages(&body);
        let tool = messages
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .unwrap();
        assert_eq!(tool["content"], json!("plain result"));
        assert_eq!(
            messages.last(),
            Some(tool),
            "no follow-up message is invented when there is no image"
        );
    }

    #[test]
    fn the_image_survives_a_claude_to_claude_round_trip() {
        // A Claude client routed to a Claude-format upstream still goes through
        // the canonical form, so the hoist has to survive the way back.
        let canonical = normalize_request(
            screenshot_turn(json!([
                {"type":"text","text":"captured"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},
            ])),
            Format::Claude,
        )
        .unwrap();
        let upstream = crate::translate::provider_request(canonical, Format::Claude).unwrap();
        let serialized = serde_json::to_string(&upstream).unwrap();
        assert!(
            serialized.contains(PNG),
            "the screenshot reaches the upstream"
        );
        let messages = messages(&upstream);
        assert!(
            messages.iter().any(|message| message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("image")))),
            "it arrives as user image content, which every Claude-compatible endpoint accepts"
        );
    }
}
