//! Native port of the chat-based (`searchViaChat`) web-search path.
//!
//! Mirrors `frontend/open-sse/handlers/search/chatSearch.js`: the upstream
//! per-provider map (`CHAT_SEARCH_CONFIG`) is replayed request-for-request and
//! answer-parser-for-answer-parser, and every failure is mapped to the same
//! `{status, error}` pair `handleChatSearch` returns. The default model and the
//! endpoint template are read from `assets/provider-catalog.json`
//! (`registry[].searchViaChat` / `media.<id>.searchViaChat`), never from Rust
//! constants, so a catalog refresh keeps the route accurate.

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::{
    providers,
    ssrf_guard::{self, FetchFailure, PublicRequest},
    state::AppState,
};

/// `REQUEST_TIMEOUT_MS` from the upstream chat-search wrapper.
const REQUEST_TIMEOUT_MS: u64 = 15_000;
/// `DEFAULT_MAX_RESULTS`.
const DEFAULT_MAX_RESULTS: u64 = 10;
/// `AG_CLIENT_NAME` (the Antigravity request envelope's client fingerprint).
const AG_CLIENT_NAME: &str = "antigravity";
/// `ANTIGRAVITY_IDE_USER_AGENT` from `open-sse/providers/shared.js`.
const ANTIGRAVITY_IDE_USER_AGENT: &str = "antigravity/ide/2.11.0 darwin/arm64";
/// `AG_CONTEXT_BEFORE` / `AG_CONTEXT_AFTER`: widths of the grounded-sentence
/// window Antigravity citations are widened to.
const AG_CONTEXT_BEFORE: i64 = 150;
const AG_CONTEXT_AFTER: i64 = 250;
/// `AG_SEARCH_GENERATION_CONFIG`. JavaScript renders the source literal `1.0` as
/// `1`, so the native body carries the integer for byte-identical payloads.
const AG_TEMPERATURE: i64 = 1;
const AG_MAX_OUTPUT_TOKENS: u64 = 8192;

/// Keys of the upstream `CHAT_SEARCH_CONFIG` map. Providers outside this set
/// answer `Unsupported chat-search provider: <id>`, exactly like upstream, even
/// when the registry entry carries a `searchViaChat` block.
const CHAT_SEARCH_PROVIDERS: [&str; 9] = [
    "antigravity",
    "gemini",
    "kimi",
    "minimax",
    "openai",
    "perplexity",
    "perplexity-agent",
    "vercel-ai-gateway",
    "xai",
];

/// `PROVIDER_MEDIA[id].searchViaChat` — the registry slice upstream reads the
/// default model and the endpoint template from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatSearchConfig {
    pub default_model: Option<String>,
    pub endpoint: Option<String>,
}

fn block_field(block: Option<&Value>, key: &str) -> Option<String> {
    block?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

/// Registry `searchViaChat` block for a supported provider. `None` means the
/// provider is not part of the upstream chat-search map.
pub fn config(provider_id: &str) -> Option<ChatSearchConfig> {
    if !CHAT_SEARCH_PROVIDERS.contains(&provider_id) {
        return None;
    }
    let canonical = providers::provider_entry(provider_id)
        .and_then(|entry| entry.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(provider_id);
    // Same two lookup locations `providers::media_config` uses (catalog `media`
    // first, the raw registry entry second).
    let block = providers::catalog()
        .get("media")
        .and_then(|media| media.get(canonical))
        .and_then(|media| media.get("searchViaChat"))
        .or_else(|| {
            providers::provider_entry(canonical).and_then(|entry| entry.get("searchViaChat"))
        });
    Some(ChatSearchConfig {
        default_model: block_field(block, "defaultModel"),
        endpoint: block_field(block, "endpoint"),
    })
}

/// Resolve the chat-search URL from the registry endpoint template, substituting
/// the resolved model for `{model}` (JavaScript `String.replace` substitutes the
/// first occurrence only).
///
/// Fallback: the `openai` and `vercel-ai-gateway` registry entries carry no
/// `searchViaChat.endpoint`, so upstream's `searchEndpoint()` yields an empty
/// string and the request cannot be built at all. The native route resolves the
/// provider's own chat transport URL for them instead (`openai` →
/// `https://api.openai.com/v1/chat/completions`, `vercel-ai-gateway` →
/// `https://ai-gateway.vercel.sh/v1/chat/completions`), because both providers
/// speak the OpenAI chat-completions shape their bodies use.
pub fn endpoint_for(
    provider_id: &str,
    config: &ChatSearchConfig,
    model: Option<&str>,
) -> Option<String> {
    if let Some(template) = config.endpoint.as_deref() {
        return Some(template.replacen("{model}", model.unwrap_or(""), 1));
    }
    providers::endpoint(provider_id, &Value::Null, "chat", model.unwrap_or(""))
        .ok()
        .map(|(url, _)| url)
}

/// Upstream `buildBody(query, model, credentials)` per provider.
pub fn request_body(
    provider_id: &str,
    query: &str,
    model: Option<&str>,
    project_id: Option<&Value>,
) -> Option<Value> {
    let mut body = Map::new();
    match provider_id {
        "gemini" => {
            return Some(json!({
                "contents": [{ "role": "user", "parts": [{ "text": query }] }],
                "tools": [{ "google_search": {} }],
            }))
        }
        "antigravity" => {
            let mut body = Map::new();
            body.insert("project".into(), project_id.cloned().unwrap_or(Value::Null));
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert("userAgent".into(), json!(AG_CLIENT_NAME));
            body.insert("requestType".into(), json!("search"));
            body.insert(
                "request".into(),
                json!({
                    "contents": [{ "role": "user", "parts": [{ "text": query }] }],
                    "tools": [{ "googleSearch": {} }],
                    "generationConfig": {
                        "temperature": AG_TEMPERATURE,
                        "maxOutputTokens": AG_MAX_OUTPUT_TOKENS,
                    },
                }),
            );
            return Some(Value::Object(body));
        }
        "openai" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert(
                "messages".into(),
                json!([{ "role": "user", "content": query }]),
            );
            // Non-search-preview models need the explicit web_search tool.
            if !model.unwrap_or("").to_ascii_lowercase().contains("search") {
                body.insert("tools".into(), json!([{ "type": "web_search" }]));
            }
        }
        "xai" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert(
                "input".into(),
                json!([{ "role": "user", "content": query }]),
            );
            body.insert("tools".into(), json!([{ "type": "web_search" }]));
        }
        "kimi" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert(
                "messages".into(),
                json!([{ "role": "user", "content": query }]),
            );
            body.insert(
                "tools".into(),
                json!([{ "type": "builtin_function", "function": { "name": "$web_search" } }]),
            );
        }
        "minimax" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert(
                "messages".into(),
                json!([{ "role": "user", "content": query }]),
            );
            body.insert("tools".into(), json!([{ "type": "web_search" }]));
        }
        "perplexity" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert(
                "messages".into(),
                json!([{ "role": "user", "content": query }]),
            );
        }
        "perplexity-agent" => {
            if let Some(model) = model {
                body.insert("model".into(), json!(model));
            }
            body.insert("input".into(), json!(query));
            body.insert("tools".into(), json!([{ "type": "web_search" }]));
        }
        _ => return None,
    }
    Some(Value::Object(body))
}

/// Upstream `buildHeaders(token)` per provider.
pub fn request_headers(provider_id: &str, token: &str) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    match provider_id {
        "gemini" => headers.push(("x-goog-api-key".to_string(), token.to_string())),
        "antigravity" => {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
            headers.push((
                "User-Agent".to_string(),
                ANTIGRAVITY_IDE_USER_AGENT.to_string(),
            ));
        }
        _ => headers.push(("Authorization".to_string(), format!("Bearer {token}"))),
    }
    headers
}

/// JavaScript truthiness for JSON values.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `a || b` for JSON operands.
fn first_truthy<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) {
        a
    } else if truthy(b) {
        b
    } else {
        None
    }
}

/// `value || ""` for a citation field.
fn or_empty(value: Option<&Value>) -> Value {
    value
        .filter(|value| truthy(Some(value)))
        .cloned()
        .unwrap_or_else(|| json!(""))
}

/// `{ url, title, snippet }` following upstream's object literals: a field is
/// present only when the caller has one (JavaScript drops `undefined` keys).
fn citation_object(url: Option<Value>, title: Option<Value>, snippet: Option<Value>) -> Value {
    let mut citation = Map::new();
    if let Some(url) = url {
        citation.insert("url".into(), url);
    }
    if let Some(title) = title {
        citation.insert("title".into(), title);
    }
    if let Some(snippet) = snippet {
        citation.insert("snippet".into(), snippet);
    }
    Value::Object(citation)
}

/// `{ url: it.url || it.link }` guarded by `if (!url) continue;`.
fn cited_item(item: &Value) -> Option<Value> {
    first_truthy(item.get("url"), item.get("link")).cloned()
}

/// A truthy-or-absent citation URL.
fn truthy_url(url: Option<&Value>) -> Option<Value> {
    url.filter(|url| truthy(Some(url))).cloned()
}

/// `parts.map(p => p?.text || "").filter(Boolean).join("")`.
fn parts_text(candidate: Option<&Value>) -> String {
    candidate
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// `tokens || 0`. Upstream passes anything non-falsy through; only numeric
/// counts are meaningful, so anything else collapses to `0`.
fn counted_tokens(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Number(number)) => match number.as_f64() {
            Some(value) if value.is_finite() => Value::Number(number.clone()),
            _ => json!(0),
        },
        Some(Value::String(text)) => json!(text),
        _ => json!(0),
    }
}

/// Upstream `Number.isInteger`.
fn js_integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    if !number.is_finite() || number.fract() != 0.0 {
        return None;
    }
    Some(number as i64)
}

/// `text.slice(start, end)` over UTF-16 code units, like JavaScript.
fn utf16_slice(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = start.min(units.len());
    let end = end.clamp(start, units.len());
    String::from_utf16_lossy(&units[start..end])
}

/// `out.replace(/^\S+/, "")` — drop the leading run of non-whitespace.
fn strip_leading_word(text: &str) -> String {
    match text.find(char::is_whitespace) {
        Some(index) => text[index..].to_string(),
        None => String::new(),
    }
}

/// `out.replace(/\S+$/, "")` — drop the trailing run of non-whitespace.
fn strip_trailing_word(text: &str) -> String {
    let mut cut = 0usize;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            cut = index + character.len_utf8();
        }
    }
    text[..cut].to_string()
}

/// Upstream `expandSegment(text, segment)`: widen a grounded segment to its
/// surrounding context window, dropping the partial words the window cut off.
fn expand_segment(text: &str, segment: &Value) -> String {
    let start_index = segment.get("startIndex").and_then(js_integer);
    let end_index = segment.get("endIndex").and_then(js_integer);
    let (Some(start_index), Some(end_index)) = (start_index, end_index) else {
        return String::new();
    };
    if text.is_empty() {
        return String::new();
    }
    let length = text.encode_utf16().count() as i64;
    let start = start_index.saturating_sub(AG_CONTEXT_BEFORE).max(0);
    let end = end_index.saturating_add(AG_CONTEXT_AFTER).min(length);
    let mut out = utf16_slice(text, start as usize, end.max(start) as usize)
        .trim()
        .to_string();
    if start > 0 {
        out = format!("...{}", strip_leading_word(&out));
    }
    if end < length {
        out = format!("{}...", strip_trailing_word(&out));
    }
    out.trim().to_string()
}

/// Order-preserving `Set` (upstream dedupes grounding snippets with `new Set`).
#[derive(Default)]
struct PieceSet {
    seen: HashSet<String>,
    order: Vec<String>,
}

impl PieceSet {
    fn add(&mut self, piece: &str) {
        if self.seen.insert(piece.to_string()) {
            self.order.push(piece.to_string());
        }
    }

    /// Upstream `joinPieces(set, sep)`: drop empties, join, trim.
    fn join(&self, separator: &str) -> String {
        self.order
            .iter()
            .filter(|piece| !piece.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(separator)
            .trim()
            .to_string()
    }
}

/// Extracted answer, citations and token count for one upstream payload.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatAnswer {
    pub text: String,
    pub citations: Vec<Value>,
    pub tokens: Value,
}

impl Default for ChatAnswer {
    fn default() -> Self {
        Self {
            text: String::new(),
            citations: Vec::new(),
            tokens: json!(0),
        }
    }
}

fn extract_gemini(data: &Value) -> ChatAnswer {
    let candidate = data.pointer("/candidates/0");
    let citations = candidate
        .and_then(|candidate| candidate.pointer("/groundingMetadata/groundingChunks"))
        .and_then(Value::as_array)
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|chunk| {
                    let web = chunk.get("web")?;
                    let url = truthy_url(first_truthy(web.get("uri"), web.get("url")))?;
                    Some(citation_object(
                        Some(url),
                        Some(or_empty(web.get("title"))),
                        None,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    ChatAnswer {
        text: parts_text(candidate),
        citations,
        tokens: counted_tokens(data.pointer("/usageMetadata/totalTokenCount")),
    }
}

fn extract_antigravity(data: &Value) -> ChatAnswer {
    // Antigravity wraps the Gemini payload in `{ response: {...} }`.
    let response = data
        .get("response")
        .filter(|response| truthy(Some(response)))
        .unwrap_or(data);
    let candidate = response.pointer("/candidates/0");
    let text = parts_text(candidate);
    let grounding = candidate.and_then(|candidate| candidate.get("groundingMetadata"));
    let chunks = grounding
        .and_then(|grounding| grounding.get("groundingChunks"))
        .and_then(Value::as_array);
    let supports = grounding
        .and_then(|grounding| grounding.get("groundingSupports"))
        .and_then(Value::as_array);

    // Upstream repeats the same source across chunks — key by URL so it stays
    // one citation, in first-seen order.
    struct Source {
        url: Value,
        title: Value,
        snippets: PieceSet,
        contexts: PieceSet,
    }
    let mut sources: Vec<Source> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut by_chunk_index: Vec<Option<usize>> = Vec::new();
    for chunk in chunks.into_iter().flatten() {
        let web = chunk.get("web");
        let url = web.and_then(|web| first_truthy(web.get("uri"), web.get("url")));
        let Some(url) = url.filter(|url| truthy(Some(url))) else {
            by_chunk_index.push(None);
            continue;
        };
        let key = match url {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let position = *positions.entry(key).or_insert_with(|| {
            sources.push(Source {
                url: url.clone(),
                title: or_empty(web.and_then(|web| web.get("title"))),
                snippets: PieceSet::default(),
                contexts: PieceSet::default(),
            });
            sources.len() - 1
        });
        by_chunk_index.push(Some(position));
    }

    // Each support ties a sentence of the answer back to the chunks that grounded it.
    for support in supports.into_iter().flatten() {
        let segment = support.get("segment");
        let grounded = segment
            .and_then(|segment| segment.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let expanded = match segment
            .map(|segment| expand_segment(&text, segment))
            .filter(|expanded| !expanded.is_empty())
        {
            Some(expanded) => expanded,
            None => grounded.to_string(),
        };
        for index in support
            .get("groundingChunkIndices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(source) = js_integer(index)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| by_chunk_index.get(index))
                .copied()
                .flatten()
                .and_then(|position| sources.get_mut(position))
            else {
                continue;
            };
            if !grounded.is_empty() {
                source.snippets.add(grounded);
            }
            if !expanded.is_empty() {
                source.contexts.add(&expanded);
            }
        }
    }

    let citations = sources
        .into_iter()
        .map(|source| {
            let snippets = source.snippets.join(" | ");
            let snippet = if snippets.is_empty() {
                source.title.clone()
            } else {
                json!(snippets)
            };
            let contexts = source.contexts.join("\n\n");
            let content = if contexts.is_empty() {
                snippet.clone()
            } else {
                json!(contexts)
            };
            json!({
                "url": source.url,
                "title": source.title,
                "snippet": snippet,
                "content": content,
            })
        })
        .collect();

    ChatAnswer {
        text,
        citations,
        tokens: counted_tokens(response.pointer("/usageMetadata/totalTokenCount")),
    }
}

fn extract_openai(data: &Value) -> ChatAnswer {
    let message = data.pointer("/choices/0/message");
    let annotations = message
        .and_then(|message| message.get("annotations"))
        .and_then(Value::as_array);
    let from_annotations: Vec<Value> = annotations
        .into_iter()
        .flatten()
        .filter_map(|annotation| annotation.get("url_citation"))
        .map(|url_citation| {
            citation_object(
                url_citation.get("url").cloned(),
                Some(or_empty(url_citation.get("title"))),
                None,
            )
        })
        .collect();
    let from_top: Vec<Value> = data
        .get("citations")
        .and_then(Value::as_array)
        .map(|citations| citations.iter().filter_map(normalize_citation).collect())
        .unwrap_or_default();
    ChatAnswer {
        text: message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        citations: if from_annotations.is_empty() {
            from_top
        } else {
            from_annotations
        },
        tokens: counted_tokens(data.pointer("/usage/total_tokens")),
    }
}

/// Upstream `normalizeCitation`: raw URL strings and `{url}` objects both pass.
fn normalize_citation(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => Some(json!({ "url": text })),
        Value::Object(map) => map
            .get("url")
            .filter(|url| truthy(Some(url)))
            .map(|_| value.clone()),
        _ => None,
    }
}

/// Upstream Responses-API walk: collect text and citations from `output[]`.
fn responses_output(data: &Value, with_item_results: bool) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut citations = Vec::new();
    for item in data
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                text.push_str(part_text);
            }
            for annotation in part
                .get("annotations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let nested = if truthy(annotation.get("url")) {
                    annotation
                } else {
                    match annotation.get("url_citation") {
                        Some(url_citation) => url_citation,
                        None => continue,
                    }
                };
                if let Some(entry) = normalize_citation(nested) {
                    citations.push(entry);
                }
            }
        }
        if with_item_results {
            for result in item
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(url) = cited_item(result) else {
                    continue;
                };
                citations.push(citation_object(
                    Some(url),
                    Some(or_empty(result.get("title"))),
                    Some(or_empty(result.get("snippet"))),
                ));
            }
        }
    }
    (text, citations)
}

fn extract_responses(provider_id: &str, data: &Value) -> ChatAnswer {
    let (text, mut citations) = responses_output(data, provider_id == "perplexity-agent");
    if citations.is_empty() {
        // Some response variants only carry the top-level citations array.
        citations = data
            .get("citations")
            .and_then(Value::as_array)
            .map(|citations| citations.iter().filter_map(normalize_citation).collect())
            .unwrap_or_default();
    }
    ChatAnswer {
        text,
        citations,
        tokens: counted_tokens(data.pointer("/usage/total_tokens")),
    }
}

/// Parsed `tool_calls[].function.arguments` payloads (upstream `JSON.parse`).
fn tool_call_payloads(message: Option<&Value>) -> Vec<Value> {
    message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.pointer("/function/arguments"))
                .filter(|arguments| truthy(Some(arguments)))
                .filter_map(|arguments| match arguments {
                    Value::String(text) => serde_json::from_str::<Value>(text).ok(),
                    other => Some(other.clone()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Upstream `parsed?.k1 || parsed?.k2 || []` + `Array.isArray` guard.
fn payload_items<'a>(payload: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    keys.iter()
        .find_map(|key| payload.get(*key).filter(|value| truthy(Some(value))))
        .and_then(Value::as_array)
}

fn extract_kimi(data: &Value) -> ChatAnswer {
    let message = data.pointer("/choices/0/message");
    let mut citations = Vec::new();
    for payload in tool_call_payloads(message) {
        let Some(items) = payload_items(&payload, &["search_results", "results", "references"])
        else {
            continue;
        };
        for item in items {
            let Some(url) = cited_item(item) else {
                continue;
            };
            citations.push(citation_object(
                Some(url),
                Some(or_empty(item.get("title"))),
                Some(or_empty(first_truthy(
                    item.get("snippet"),
                    item.get("summary"),
                ))),
            ));
        }
    }
    ChatAnswer {
        text: message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        citations,
        tokens: counted_tokens(data.pointer("/usage/total_tokens")),
    }
}

fn extract_minimax(data: &Value) -> ChatAnswer {
    let message = data.pointer("/choices/0/message");
    let mut citations: Vec<Value> = data
        .get("web_search_results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|item| {
                    let url = cited_item(item)?;
                    Some(citation_object(
                        Some(url),
                        Some(or_empty(item.get("title"))),
                        Some(or_empty(first_truthy(
                            item.get("snippet"),
                            item.get("summary"),
                        ))),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if citations.is_empty() {
        for payload in tool_call_payloads(message) {
            let Some(items) = payload_items(&payload, &["results", "search_results"]) else {
                continue;
            };
            for item in items {
                let Some(url) = cited_item(item) else {
                    continue;
                };
                citations.push(citation_object(
                    Some(url),
                    Some(or_empty(item.get("title"))),
                    Some(or_empty(item.get("snippet"))),
                ));
            }
        }
    }
    ChatAnswer {
        text: message
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        citations,
        tokens: counted_tokens(data.pointer("/usage/total_tokens")),
    }
}

fn extract_perplexity(data: &Value) -> ChatAnswer {
    ChatAnswer {
        text: data
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        citations: data
            .get("citations")
            .and_then(Value::as_array)
            .map(|citations| citations.iter().filter_map(normalize_citation).collect())
            .unwrap_or_default(),
        tokens: counted_tokens(data.pointer("/usage/total_tokens")),
    }
}

/// Upstream `extractAnswer` dispatch.
pub fn extract_answer(provider_id: &str, data: &Value) -> ChatAnswer {
    match provider_id {
        "gemini" => extract_gemini(data),
        "antigravity" => extract_antigravity(data),
        "openai" => extract_openai(data),
        "xai" => extract_responses("xai", data),
        "kimi" => extract_kimi(data),
        "minimax" => extract_minimax(data),
        "perplexity" => extract_perplexity(data),
        "perplexity-agent" => extract_responses("perplexity-agent", data),
        _ => ChatAnswer::default(),
    }
}

/// Credentials a chat-search attempt runs with. `provider_specific_data` in the
/// upstream enriched credential object supplies `projectId` on the top level.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatCredentials {
    pub token: Option<String>,
    pub project_id: Option<Value>,
}

/// Upstream `requireCredentials`: a provider may need more than a token.
pub fn credentials_error(provider_id: &str, credentials: &ChatCredentials) -> Option<String> {
    match provider_id {
        "antigravity" => (!truthy(credentials.project_id.as_ref()))
            .then(|| "Antigravity account has no projectId — reconnect the account".to_string()),
        _ => None,
    }
}

/// `Number.isFinite(maxResults) && maxResults > 0 ? Math.floor(maxResults) : 10`.
pub fn limit(max_results: Option<&Value>) -> u64 {
    let Some(Value::Number(number)) = max_results else {
        return DEFAULT_MAX_RESULTS;
    };
    let Some(value) = number.as_f64() else {
        return DEFAULT_MAX_RESULTS;
    };
    if !value.is_finite() || value <= 0.0 {
        return DEFAULT_MAX_RESULTS;
    }
    value.floor().min(u64::MAX as f64) as u64
}

/// A chat-search attempt that passed validation.
#[derive(Debug, PartialEq)]
pub struct Prepared {
    pub url: String,
    pub body: Value,
    pub headers: Vec<(String, String)>,
    pub limit: u64,
    pub model: Option<String>,
}

/// Result of a chat-search attempt, mirroring `handleChatSearch`'s
/// `{success, status?, error?, data?}` tuple.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Success(Value),
    Failure { status: u16, error: String },
}

impl Outcome {
    pub fn failure(status: u16, error: impl Into<String>) -> Self {
        Outcome::Failure {
            status,
            error: error.into(),
        }
    }
}

/// Validation + request construction. Mirrors the ordered guard block of
/// `handleChatSearch` (provider map, query, token, `requireCredentials`) before
/// any network call happens.
pub fn prepare(
    provider_id: &str,
    query: &str,
    max_results: Option<&Value>,
    credentials: &ChatCredentials,
) -> Result<Prepared, Outcome> {
    let Some(config) = config(provider_id) else {
        return Err(Outcome::failure(
            400,
            format!("Unsupported chat-search provider: {provider_id}"),
        ));
    };
    if query.is_empty() {
        return Err(Outcome::failure(400, "Missing query"));
    }
    let Some(token) = credentials
        .token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return Err(Outcome::failure(
            401,
            "Missing credentials (apiKey or accessToken)",
        ));
    };
    if let Some(error) = credentials_error(provider_id, credentials) {
        return Err(Outcome::failure(401, error));
    }
    let model = config.default_model.clone();
    let Some(url) = endpoint_for(provider_id, &config, model.as_deref()) else {
        return Err(Outcome::failure(
            502,
            format!("Network error: {provider_id} has no chat-search endpoint"),
        ));
    };
    let Some(body) = request_body(
        provider_id,
        query,
        model.as_deref(),
        credentials.project_id.as_ref(),
    ) else {
        return Err(Outcome::failure(
            400,
            format!("Unsupported chat-search provider: {provider_id}"),
        ));
    };
    Ok(Prepared {
        url,
        body,
        headers: request_headers(provider_id, token),
        limit: limit(max_results),
        model,
    })
}

/// Upstream transport failures: an aborted `fetch` is a `504`, everything else a
/// `502` carrying the Node error message.
fn fetch_failure_outcome(failure: &FetchFailure) -> Outcome {
    match failure {
        FetchFailure::Timeout => Outcome::failure(504, "Upstream timeout"),
        other => Outcome::failure(502, format!("Network error: {}", other.message())),
    }
}

/// `data?.error?.message || data?.error || data?.message || "Upstream HTTP N"`,
/// stringified when the selected value is not a string.
pub fn upstream_error_message(data: &Value, status: u16) -> String {
    let candidates = [
        data.pointer("/error/message"),
        data.get("error"),
        data.get("message"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if !truthy(Some(candidate)) {
            continue;
        }
        return match candidate {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
    }
    format!("Upstream HTTP {status}")
}

/// Upstream `toResult(c, index, provider, retrievedAt)`.
fn to_result(citation: &Value, index: usize, provider_id: &str, retrieved_at: &str) -> Value {
    let rank = index + 1;
    json!({
        "title": or_empty(citation.get("title")),
        "url": citation.get("url").cloned().unwrap_or(Value::Null),
        "snippet": or_empty(citation.get("snippet")),
        "position": rank,
        "score": Value::Null,
        "published_at": Value::Null,
        "favicon_url": Value::Null,
        "content": citation
            .get("content")
            .filter(|content| truthy(Some(content)))
            .cloned()
            .unwrap_or(Value::Null),
        "metadata": {},
        "citation": {
            "provider": provider_id,
            "retrieved_at": retrieved_at,
            "rank": rank,
        },
        "provider_raw": Value::Null,
    })
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Upstream chat-search success payload.
fn payload(
    provider_id: &str,
    query: &str,
    model: Option<&str>,
    answer: &ChatAnswer,
    limit: u64,
    retrieved_at: &str,
    response_time_ms: i64,
    upstream_latency_ms: i64,
) -> Value {
    let results: Vec<Value> = answer
        .citations
        .iter()
        .take(limit as usize)
        .enumerate()
        .map(|(index, citation)| to_result(citation, index, provider_id, retrieved_at))
        .collect();
    let mut answer_payload = Map::new();
    answer_payload.insert("source".into(), json!(provider_id));
    answer_payload.insert("text".into(), json!(answer.text));
    if let Some(model) = model {
        answer_payload.insert("model".into(), json!(model));
    }
    json!({
        "provider": provider_id,
        "query": query,
        "results": results,
        "answer": Value::Object(answer_payload),
        "usage": {
            "queries_used": 1,
            "search_cost_usd": 0,
            "llm_tokens": answer.tokens,
        },
        "metrics": {
            "response_time_ms": response_time_ms,
            "upstream_latency_ms": upstream_latency_ms,
            "total_results_available": Value::Null,
        },
        "errors": [],
    })
}

/// Upstream `handleChatSearch` response handling order: parse the JSON payload
/// first (`502 Invalid upstream response (status N)` when that fails, even for a
/// successful status), then surface a non-2xx status with the extracted message.
pub fn parse_upstream_response(status: u16, text: &str) -> Result<Value, Outcome> {
    let data: Value = match serde_json::from_str(text) {
        Ok(data) => data,
        Err(_) => {
            return Err(Outcome::failure(
                502,
                format!("Invalid upstream response (status {status})"),
            ))
        }
    };
    if !(200..300).contains(&status) {
        return Err(Outcome::failure(
            status,
            upstream_error_message(&data, status),
        ));
    }
    Ok(data)
}

/// Upstream `handleChatSearch`: one chat-completion round trip turned into the
/// unified `/v1/search` envelope.
pub async fn handle(
    state: &AppState,
    provider_id: &str,
    query: &str,
    max_results: Option<&Value>,
    credentials: &ChatCredentials,
) -> Outcome {
    let started = Instant::now();
    let prepared = match prepare(provider_id, query, max_results, credentials) {
        Ok(prepared) => prepared,
        Err(outcome) => return outcome,
    };

    let mut headers = HeaderMap::new();
    for (name, value) in &prepared.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    let body = match serde_json::to_string(&prepared.body) {
        Ok(body) => body,
        Err(_) => {
            return Outcome::failure(
                502,
                format!("Network error: {provider_id} request body is not serialisable"),
            )
        }
    };
    let request = PublicRequest {
        method: Method::POST,
        headers,
        body: Some(body),
        timeout_ms: Some(REQUEST_TIMEOUT_MS),
    };

    let upstream_started = Instant::now();
    let response = match ssrf_guard::fetch_public(&state.proxy_http, &prepared.url, &request).await
    {
        Ok(response) => response,
        Err(failure) => return fetch_failure_outcome(&failure),
    };
    let upstream_latency_ms = upstream_started.elapsed().as_millis() as i64;

    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let data = match parse_upstream_response(status, &text) {
        Ok(data) => data,
        Err(outcome) => return outcome,
    };

    let answer = extract_answer(provider_id, &data);
    let response_time_ms = started.elapsed().as_millis() as i64;
    Outcome::Success(payload(
        provider_id,
        query,
        prepared.model.as_deref(),
        &answer,
        prepared.limit,
        &now_iso(),
        response_time_ms,
        upstream_latency_ms,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn credentials() -> ChatCredentials {
        ChatCredentials {
            token: Some("token-123".to_string()),
            project_id: Some(json!("project-1")),
        }
    }

    fn prepared(provider_id: &str, query: &str) -> Prepared {
        prepare(provider_id, query, None, &credentials()).expect("prepared")
    }

    /// The supported set must stay exactly the registry's `searchViaChat` set,
    /// and no provider may carry both a dedicated `searchConfig` and a
    /// `searchViaChat` block (that combination would need upstream's
    /// dedicated → chat failover inside `handleSearchCore`).
    #[test]
    fn chat_search_set_matches_the_registry() {
        let registry: Vec<String> = providers::catalog()
            .get("registry")
            .and_then(Value::as_array)
            .expect("registry")
            .iter()
            .filter(|entry| entry.get("searchViaChat").is_some())
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        assert_eq!(registry, CHAT_SEARCH_PROVIDERS.to_vec());
        for entry in providers::catalog()
            .get("registry")
            .and_then(Value::as_array)
            .expect("registry")
        {
            assert!(
                !(entry.get("searchConfig").is_some() && entry.get("searchViaChat").is_some()),
                "provider {} has both searchConfig and searchViaChat",
                entry.get("id").and_then(Value::as_str).unwrap_or_default()
            );
        }
        assert!(config("serper").is_none());
    }

    #[test]
    fn config_reads_endpoint_and_model_from_the_catalog() {
        let gemini = config("gemini").expect("gemini config");
        assert_eq!(gemini.default_model.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(
            gemini.endpoint.as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent")
        );
        let antigravity = config("antigravity").expect("antigravity config");
        assert_eq!(
            antigravity.default_model.as_deref(),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            antigravity.endpoint.as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent")
        );
        // Both endpoint-less entries fall back to the provider chat transport.
        for (provider, model, url) in [
            (
                "openai",
                "gpt-4o-mini",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "vercel-ai-gateway",
                "openai/gpt-4o-mini",
                "https://ai-gateway.vercel.sh/v1/chat/completions",
            ),
        ] {
            let config = config(provider).expect("config");
            assert_eq!(config.default_model.as_deref(), Some(model));
            assert_eq!(config.endpoint, None);
            assert_eq!(
                endpoint_for(provider, &config, Some(model)).as_deref(),
                Some(url)
            );
        }
        // The IDE fingerprint constant must match the catalog transport header.
        let agent = providers::transport("antigravity")
            .pointer("/headers/User-Agent")
            .and_then(Value::as_str)
            .map(str::to_string);
        assert_eq!(agent.as_deref(), Some(ANTIGRAVITY_IDE_USER_AGENT));
    }

    #[test]
    fn gemini_request_matches_upstream_shape() {
        let built = prepared("gemini", "rust lang");
        assert_eq!(
            built.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            built.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("x-goog-api-key".to_string(), "token-123".to_string()),
            ]
        );
        assert_eq!(
            built.body,
            json!({
                "contents": [{ "role": "user", "parts": [{ "text": "rust lang" }] }],
                "tools": [{ "google_search": {} }],
            })
        );
    }

    #[test]
    fn antigravity_request_carries_project_and_search_tool() {
        let built = prepared("antigravity", "rust lang");
        assert_eq!(
            built.url,
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
        assert_eq!(
            built.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer token-123".to_string()),
                (
                    "User-Agent".to_string(),
                    "antigravity/ide/2.11.0 darwin/arm64".to_string()
                ),
            ]
        );
        assert_eq!(
            built.body,
            json!({
                "project": "project-1",
                "model": "gemini-2.5-flash",
                "userAgent": "antigravity",
                "requestType": "search",
                "request": {
                    "contents": [{ "role": "user", "parts": [{ "text": "rust lang" }] }],
                    "tools": [{ "googleSearch": {} }],
                    "generationConfig": { "temperature": 1, "maxOutputTokens": 8192 },
                },
            })
        );
        // The upstream envelope stringifies the JS literal `1.0` as `1`.
        assert_eq!(
            serde_json::to_string(&built.body).expect("serialisable"),
            "{\"model\":\"gemini-2.5-flash\",\"project\":\"project-1\",\"request\":{\"contents\":[{\"parts\":[{\"text\":\"rust lang\"}],\"role\":\"user\"}],\"generationConfig\":{\"maxOutputTokens\":8192,\"temperature\":1},\"tools\":[{\"googleSearch\":{}}]},\"requestType\":\"search\",\"userAgent\":\"antigravity\"}"
        );
    }

    #[test]
    fn openai_request_omits_tools_for_search_models() {
        let built =
            prepare("openai", "rust lang", Some(&json!(4)), &credentials()).expect("prepared");
        assert_eq!(
            built.body,
            json!({
                "model": "gpt-4o-mini",
                "messages": [{ "role": "user", "content": "rust lang" }],
                "tools": [{ "type": "web_search" }],
            })
        );
        assert_eq!(
            request_body("openai", "rust lang", Some("gpt-4o-search-preview"), None).expect("body"),
            json!({
                "model": "gpt-4o-search-preview",
                "messages": [{ "role": "user", "content": "rust lang" }],
            })
        );
    }

    #[test]
    fn responses_providers_use_their_upstream_shapes() {
        assert_eq!(
            request_body("xai", "rust lang", Some("grok-4.20-reasoning"), None).expect("body"),
            json!({
                "model": "grok-4.20-reasoning",
                "input": [{ "role": "user", "content": "rust lang" }],
                "tools": [{ "type": "web_search" }],
            })
        );
        assert_eq!(
            request_body(
                "perplexity-agent",
                "rust lang",
                Some("perplexity/sonar"),
                None
            )
            .expect("body"),
            json!({
                "model": "perplexity/sonar",
                "input": "rust lang",
                "tools": [{ "type": "web_search" }],
            })
        );
        assert_eq!(
            request_body("kimi", "rust lang", Some("kimi-k3"), None).expect("body"),
            json!({
                "model": "kimi-k3",
                "messages": [{ "role": "user", "content": "rust lang" }],
                "tools": [{ "type": "builtin_function", "function": { "name": "$web_search" } }],
            })
        );
        assert_eq!(
            request_body("minimax", "rust lang", Some("MiniMax-M2.7"), None).expect("body"),
            json!({
                "model": "MiniMax-M2.7",
                "messages": [{ "role": "user", "content": "rust lang" }],
                "tools": [{ "type": "web_search" }],
            })
        );
        assert_eq!(
            request_body("perplexity", "rust lang", Some("sonar"), None).expect("body"),
            json!({
                "model": "sonar",
                "messages": [{ "role": "user", "content": "rust lang" }],
            })
        );
        // Endpoint templates keep `{model}` substitution for the chat transports.
        assert_eq!(
            prepared("gemini", "q").headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("x-goog-api-key".to_string(), "token-123".to_string()),
            ]
        );
        for provider in ["xai", "kimi", "minimax", "perplexity", "perplexity-agent"] {
            assert_eq!(
                request_headers(provider, "token-123"),
                vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Authorization".to_string(), "Bearer token-123".to_string()),
                ],
                "{provider}"
            );
        }
        assert!(endpoint_for(
            "xai",
            &config("xai").expect("config"),
            Some("grok-4.20-reasoning")
        )
        .is_some_and(|url| url == "https://api.x.ai/v1/responses"));
    }

    #[test]
    fn validation_errors_match_upstream_status_and_message() {
        assert_eq!(
            prepare("serper", "q", None, &credentials()).unwrap_err(),
            Outcome::failure(400, "Unsupported chat-search provider: serper")
        );
        assert_eq!(
            prepare("gemini", "", None, &credentials()).unwrap_err(),
            Outcome::failure(400, "Missing query")
        );
        let anonymous = ChatCredentials::default();
        assert_eq!(
            prepare("gemini", "q", None, &anonymous).unwrap_err(),
            Outcome::failure(401, "Missing credentials (apiKey or accessToken)")
        );
        let antigravity = ChatCredentials {
            token: Some("token-123".to_string()),
            project_id: None,
        };
        assert_eq!(
            prepare("antigravity", "q", None, &antigravity).unwrap_err(),
            Outcome::failure(
                401,
                "Antigravity account has no projectId — reconnect the account"
            )
        );
    }

    #[test]
    fn fetch_failures_map_to_upstream_status_codes() {
        assert_eq!(
            fetch_failure_outcome(&FetchFailure::Timeout),
            Outcome::failure(504, "Upstream timeout")
        );
        assert_eq!(
            fetch_failure_outcome(&FetchFailure::Network("connection refused".into())),
            Outcome::failure(502, "Network error: connection refused")
        );
    }

    #[test]
    fn response_parsing_maps_status_and_bodies_like_upstream() {
        // The body is parsed before the status is inspected.
        assert_eq!(
            parse_upstream_response(500, "<html>bad gateway</html>"),
            Err(Outcome::failure(
                502,
                "Invalid upstream response (status 500)"
            ))
        );
        assert_eq!(
            parse_upstream_response(200, "not json"),
            Err(Outcome::failure(
                502,
                "Invalid upstream response (status 200)"
            ))
        );
        assert_eq!(
            parse_upstream_response(401, "{\"error\":{\"message\":\"bad key\"}}"),
            Err(Outcome::failure(401, "bad key"))
        );
        assert_eq!(
            parse_upstream_response(429, "{\"message\":\"slow down\"}"),
            Err(Outcome::failure(429, "slow down"))
        );
        assert_eq!(
            parse_upstream_response(200, "{\"choices\":[]}"),
            Ok(json!({"choices": []}))
        );
    }

    #[test]
    fn upstream_error_message_prefers_error_message() {
        assert_eq!(
            upstream_error_message(&json!({"error": {"message": "bad key"}}), 401),
            "bad key"
        );
        assert_eq!(
            upstream_error_message(&json!({"error": "rate limited"}), 429),
            "rate limited"
        );
        assert_eq!(
            upstream_error_message(&json!({"message": "boom"}), 500),
            "boom"
        );
        assert_eq!(
            upstream_error_message(&json!({"error": {"code": "x"}}), 400),
            "{\"code\":\"x\"}"
        );
        assert_eq!(upstream_error_message(&json!({}), 503), "Upstream HTTP 503");
        // Falsy values fall through the `||` chain; the falsy *message* inside a
        // truthy `error` object still wins because `data.error` itself is truthy.
        assert_eq!(
            upstream_error_message(&json!({"error": {"message": ""}, "message": "kept"}), 400),
            "{\"message\":\"\"}"
        );
        assert_eq!(
            upstream_error_message(&json!({"error": "", "message": "kept"}), 400),
            "kept"
        );
    }

    #[test]
    fn limit_matches_upstream_defaults() {
        assert_eq!(limit(None), 10);
        assert_eq!(limit(Some(&json!(3))), 3);
        assert_eq!(limit(Some(&json!(2.9))), 2);
        assert_eq!(limit(Some(&json!(0))), 10);
        assert_eq!(limit(Some(&json!(-4))), 10);
        assert_eq!(limit(Some(&json!("5"))), 10);
        assert_eq!(limit(Some(&json!(true))), 10);
        assert_eq!(limit(Some(&Value::Null)), 10);
        assert_eq!(limit(Some(&json!(1e20))), u64::MAX);
    }

    #[test]
    fn translate_helpers_match_javascript_string_semantics() {
        let text = "First sentence here. Second sentence with a very long window tail that keeps going for a while.";
        // The 150/250 window already covers this whole short answer.
        assert_eq!(
            expand_segment(text, &json!({"startIndex": 0, "endIndex": 19})),
            text
        );
        assert_eq!(
            expand_segment(text, &json!({"startIndex": 0.5, "endIndex": 3})),
            ""
        );
        assert_eq!(
            expand_segment("", &json!({"startIndex": 0, "endIndex": 0})),
            ""
        );
        // UTF-16 windows, like `String.prototype.slice`.
        assert_eq!(
            expand_segment("héllo wörld", &json!({"startIndex": 0, "endIndex": 5})),
            "héllo wörld"
        );
        // A window that really is clipped gets the partial-word ellipses.
        let long_text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega ".repeat(6);
        let window = expand_segment(&long_text, &json!({"startIndex": 200, "endIndex": 220}));
        assert!(window.starts_with("..."), "{window}");
        assert!(window.ends_with("..."), "{window}");
        assert!(window.len() > 300, "{}", window.len());
        assert_eq!(js_integer(&json!(2.0)), Some(2));
        assert_eq!(js_integer(&json!(2.5)), None);
        assert_eq!(js_integer(&json!("2")), None);
    }

    #[test]
    fn citation_windows_handle_extreme_provider_offsets() {
        assert_eq!(
            expand_segment(
                "answer text",
                &json!({"startIndex": i64::MIN, "endIndex": i64::MAX})
            ),
            "answer text"
        );
        assert_eq!(
            expand_segment("answer text", &json!({"startIndex": 0, "endIndex": 1e100})),
            "answer text"
        );
    }

    #[test]
    fn gemini_fixture_extracts_text_citations_and_tokens() {
        let data = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "Rust is a language. " }, { "text": "See rust-lang.org." }] },
                "groundingMetadata": {
                    "groundingChunks": [
                        { "web": { "uri": "https://rust-lang.org", "title": "Rust" } },
                        { "web": { "title": "no url" } },
                        { "web": { "url": "https://doc.rust-lang.org", "title": "Docs" } }
                    ]
                }
            }],
            "usageMetadata": { "totalTokenCount": 42 }
        });
        let answer = extract_answer("gemini", &data);
        assert_eq!(answer.text, "Rust is a language. See rust-lang.org.");
        assert_eq!(
            answer.citations,
            vec![
                json!({"url": "https://rust-lang.org", "title": "Rust"}),
                json!({"url": "https://doc.rust-lang.org", "title": "Docs"}),
            ]
        );
        assert_eq!(answer.tokens, json!(42));
    }

    #[test]
    fn antigravity_fixture_builds_deduped_snippets_and_contexts() {
        let text = "Rust was first released in 2015 and is maintained by the Rust team.";
        let data = json!({
            "response": {
                "candidates": [{
                    "content": { "parts": [{ "text": text }] },
                    "groundingMetadata": {
                        "groundingChunks": [
                            { "web": { "uri": "https://rust-lang.org", "title": "Rust" } },
                            { "web": { "uri": "https://rust-lang.org", "title": "Rust" } },
                            { "web": { "uri": "https://blog.rust-lang.org", "title": "Blog" } }
                        ],
                        "groundingSupports": [
                            { "segment": { "startIndex": 0, "endIndex": 30, "text": "Rust was first released in 2015" },
                              "groundingChunkIndices": [0, 1] },
                            { "segment": { "startIndex": 39, "endIndex": 63, "text": "maintained by the Rust team" },
                              "groundingChunkIndices": [2, 0.5] }
                        ]
                    }
                }],
                "usageMetadata": { "totalTokenCount": 7 }
            }
        });
        let answer = extract_answer("antigravity", &data);
        assert_eq!(answer.text, text);
        assert_eq!(answer.tokens, json!(7));
        assert_eq!(answer.citations.len(), 2);
        let first = &answer.citations[0];
        assert_eq!(first["url"], "https://rust-lang.org");
        assert_eq!(first["title"], "Rust");
        // Chunks 0 and 1 share this URL but only one support grounds it, so the
        // snippet set holds a single sentence for it.
        assert_eq!(first["snippet"], "Rust was first released in 2015");
        // The 150/250 context window already covers the whole short answer.
        assert_eq!(
            first["content"],
            "Rust was first released in 2015 and is maintained by the Rust team."
        );
        assert_eq!(answer.citations[1]["url"], "https://blog.rust-lang.org");
        assert_eq!(
            answer.citations[1]["snippet"],
            "maintained by the Rust team"
        );
    }

    #[test]
    fn inline_urls_in_the_answer_are_not_turned_into_results() {
        // Upstream only maps structured citations; inline links stay in the answer text.
        let data = json!({
            "choices": [{ "message": { "content": "See https://example.com/a and https://example.com/b." } }],
            "usage": { "total_tokens": 12 }
        });
        let answer = extract_answer("perplexity", &data);
        assert_eq!(
            answer.text,
            "See https://example.com/a and https://example.com/b."
        );
        assert!(answer.citations.is_empty());
        let payload = payload("perplexity", "q", Some("sonar"), &answer, 10, "now", 1, 1);
        assert_eq!(payload["results"], json!([]));
        assert_eq!(payload["answer"]["model"], "sonar");
        assert_eq!(payload["usage"]["llm_tokens"], 12);
    }

    #[test]
    fn openai_fixture_prefers_annotations_over_top_level_citations() {
        let annotated = json!({
            "choices": [{ "message": {
                "content": "Answer",
                "annotations": [
                    { "type": "url_citation", "url_citation": { "url": "https://a.example", "title": "A" } },
                    { "type": "other" }
                ]
            }}],
            "citations": ["https://ignored.example"],
            "usage": { "total_tokens": 5 }
        });
        assert_eq!(
            extract_answer("openai", &annotated).citations,
            vec![json!({"url": "https://a.example", "title": "A"})]
        );
        let top_level = json!({
            "choices": [{ "message": { "content": "Answer" } }],
            "citations": ["https://a.example", { "url": "https://b.example", "title": "B" }, { "title": "no url" }]
        });
        assert_eq!(
            extract_answer("openai", &top_level).citations,
            vec![
                json!({"url": "https://a.example"}),
                json!({"url": "https://b.example", "title": "B"}),
            ]
        );
    }

    #[test]
    fn xai_fixture_reads_output_annotations() {
        let data = json!({
            "output": [
                { "content": [
                    { "type": "output_text", "text": "Part one. " },
                    { "type": "output_text", "text": "Part two.", "annotations": [
                        { "type": "url_citation", "url": "https://x.example", "title": "X" }
                    ] }
                ] }
            ],
            "usage": { "total_tokens": 9 }
        });
        let answer = extract_answer("xai", &data);
        assert_eq!(answer.text, "Part one. Part two.");
        assert_eq!(
            answer.citations,
            vec![json!({"type": "url_citation", "url": "https://x.example", "title": "X"})]
        );
        assert_eq!(answer.tokens, json!(9));
    }

    #[test]
    fn kimi_fixture_parses_tool_call_search_results() {
        let data = json!({
            "choices": [{ "message": {
                "content": "Answer",
                "tool_calls": [
                    { "function": { "name": "$web_search", "arguments": "{\"search_results\":[{\"url\":\"https://a.example\",\"title\":\"A\",\"summary\":\"sum\"}]}" } },
                    { "function": { "name": "$web_search", "arguments": "not json" } },
                    { "function": { "name": "$web_search" } }
                ]
            }}],
            "usage": { "total_tokens": 3 }
        });
        let answer = extract_answer("kimi", &data);
        assert_eq!(answer.text, "Answer");
        assert_eq!(
            answer.citations,
            vec![json!({"url": "https://a.example", "title": "A", "snippet": "sum"})]
        );
    }

    #[test]
    fn minimax_fixture_prefers_web_search_results_over_tool_calls() {
        let direct = json!({
            "choices": [{ "message": { "content": "Answer" } }],
            "web_search_results": [{ "link": "https://a.example", "title": "A", "summary": "sum" }],
            "usage": { "total_tokens": 4 }
        });
        assert_eq!(
            extract_answer("minimax", &direct).citations,
            vec![json!({"url": "https://a.example", "title": "A", "snippet": "sum"})]
        );
        let tool_only = json!({
            "choices": [{ "message": {
                "content": null,
                "tool_calls": [{ "function": { "arguments": { "results": [{ "url": "https://b.example", "snippet": "s" }] } } }]
            }}]
        });
        let answer = extract_answer("minimax", &tool_only);
        assert_eq!(answer.text, "");
        assert_eq!(
            answer.citations,
            vec![json!({"url": "https://b.example", "title": "", "snippet": "s"})]
        );
    }

    #[test]
    fn perplexity_agent_fixture_merges_content_and_item_results() {
        let data = json!({
            "output": [
                { "content": [
                    { "type": "output_text", "text": "Hello. ", "annotations": [
                        { "url": "https://a.example", "title": "A" }
                    ] }
                ] },
                { "content": [{ "type": "output_text", "text": "World." }],
                  "results": [{ "url": "https://b.example", "link": "https://b.example", "title": "B", "snippet": "sb" }] }
            ],
            "usage": { "total_tokens": 11 }
        });
        let answer = extract_answer("perplexity-agent", &data);
        assert_eq!(answer.text, "Hello. World.");
        assert_eq!(
            answer.citations,
            vec![
                json!({"url": "https://a.example", "title": "A"}),
                json!({"url": "https://b.example", "title": "B", "snippet": "sb"}),
            ]
        );
        assert_eq!(answer.tokens, json!(11));
    }

    #[test]
    fn payload_matches_upstream_envelope() {
        let answer = ChatAnswer {
            text: "Answer text".to_string(),
            citations: vec![
                json!({"url": "https://a.example", "title": "A", "snippet": "sa"}),
                json!({"url": "https://b.example", "title": "B"}),
            ],
            tokens: json!(17),
        };
        let payload = payload(
            "gemini",
            "rust lang",
            Some("gemini-2.5-flash"),
            &answer,
            1,
            "2026-09-16T00:00:00.000Z",
            12,
            7,
        );
        assert_eq!(
            payload,
            json!({
                "provider": "gemini",
                "query": "rust lang",
                "results": [{
                    "title": "A",
                    "url": "https://a.example",
                    "snippet": "sa",
                    "position": 1,
                    "score": null,
                    "published_at": null,
                    "favicon_url": null,
                    "content": null,
                    "metadata": {},
                    "citation": {
                        "provider": "gemini",
                        "retrieved_at": "2026-09-16T00:00:00.000Z",
                        "rank": 1,
                    },
                    "provider_raw": null,
                }],
                "answer": { "source": "gemini", "text": "Answer text", "model": "gemini-2.5-flash" },
                "usage": { "queries_used": 1, "search_cost_usd": 0, "llm_tokens": 17 },
                "metrics": {
                    "response_time_ms": 12,
                    "upstream_latency_ms": 7,
                    "total_results_available": null,
                },
                "errors": [],
            })
        );
    }

    #[test]
    fn antigravity_citations_keep_their_grounding_content() {
        let data = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "Sentence one. Sentence two." }] },
                "groundingMetadata": {
                    "groundingChunks": [{ "web": { "uri": "https://a.example", "title": "A" } }],
                    "groundingSupports": [
                        { "segment": { "startIndex": 0, "endIndex": 12, "text": "Sentence one" },
                          "groundingChunkIndices": [0] }
                    ]
                }
            }]
        });
        let answer = extract_answer("antigravity", &data);
        assert_eq!(
            answer.citations,
            vec![json!({
                "url": "https://a.example",
                "title": "A",
                "snippet": "Sentence one",
                "content": "Sentence one. Sentence two.",
            })]
        );
    }
}
