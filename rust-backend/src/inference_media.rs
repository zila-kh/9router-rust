use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};

use crate::{error::AppError, state::AppState};

pub async fn handle_media_voices(
    state: &AppState,
    method: &Method,
    subpath: &str,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    crate::voice_catalog::handle_internal(state, method, subpath, query).await
}

pub async fn handle_v1_audio_voices(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    crate::voice_catalog::handle_public(state, method, query).await
}

fn count_value_chars(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::String(s) => s.chars().count(),
        Value::Number(n) => n.to_string().chars().count(),
        Value::Bool(b) => b.to_string().chars().count(),
        Value::Array(items) => items.iter().map(count_value_chars).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.chars().count() + count_value_chars(v))
            .sum(),
    }
}

fn count_content_block_chars(block: &Value) -> usize {
    let Some(obj) = block.as_object() else {
        return count_value_chars(block);
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("text") => count_value_chars(obj.get("text").unwrap_or(&Value::Null)),
        Some("tool_use") => {
            count_value_chars(obj.get("name").unwrap_or(&Value::Null))
                + count_value_chars(obj.get("input").unwrap_or(&Value::Null))
        }
        Some("tool_result") => count_value_chars(obj.get("content").unwrap_or(&Value::Null)),
        Some("thinking") => count_value_chars(obj.get("thinking").unwrap_or(&Value::Null)),
        _ => count_value_chars(block),
    }
}

fn count_message_chars(message: &Value) -> usize {
    let Some(obj) = message.as_object() else {
        return 0;
    };
    match obj.get("content") {
        Some(Value::String(s)) => s.chars().count(),
        Some(Value::Array(blocks)) => blocks.iter().map(count_content_block_chars).sum(),
        Some(other) => count_value_chars(other),
        None => 0,
    }
}

/// Mirrors the upstream `estimateAnthropicInputTokens` heuristic: character
/// counts of `system`, `tools`, and message contents divided by four, rounded
/// up to the nearest token.
pub fn estimate_anthropic_input_tokens(body: &Value) -> usize {
    let mut total = 0usize;
    if let Some(system) = body.get("system") {
        total += count_value_chars(system);
    }
    if let Some(tools) = body.get("tools") {
        total += count_value_chars(tools);
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            total += count_message_chars(message);
        }
    }
    // ceil(total / 4)
    total.div_ceil(4)
}

pub async fn handle_count_tokens(
    _state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let input_tokens = estimate_anthropic_input_tokens(body);
    json_response(StatusCode::OK, json!({ "input_tokens": input_tokens }))
}

/// Internal `/api/v1/search` path. Delegates to the native adapter module so
/// the public `/v1/search` route and this internal path share one contract.
pub async fn handle_v1_search(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let raw = bytes::Bytes::from(serde_json::to_vec(body)?);
    crate::search_api::handle(state, method, &raw).await
}

/// Internal `/api/v1/web/fetch` path. Delegates to the native adapter module.
pub async fn handle_v1_web_fetch(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let raw = bytes::Bytes::from(serde_json::to_vec(body)?);
    crate::web_fetch_api::handle(state, method, &raw).await
}

/// Internal `/api/v1/videos/**` path. Delegates to the native adapter module so
/// the public `/v1/videos/**` family and this internal path share one contract.
pub async fn handle_v1_videos(
    state: &AppState,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    crate::videos_api::handle_parsed_body(state, method, path, headers, body).await
}

pub async fn handle_v1_models_info(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    crate::model_catalog::handle_model_info(state, method, query).await
}

pub async fn handle_v1beta_models(
    state: &AppState,
    method: &Method,
    consumer: bool,
) -> Result<Response<Body>, AppError> {
    crate::model_catalog::handle_v1beta_models(state, method, consumer).await
}

pub async fn handle_v1_api_chat(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-4o");
    let wants_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let canonical =
        match crate::translate::normalize_request(body.clone(), crate::translate::Format::OpenAi) {
            Ok(c) => c,
            Err(e) => return Err(AppError::BadRequest(e.to_string())),
        };

    let empty_headers = axum::http::HeaderMap::new();
    crate::gateway::execute_target_direct(
        state,
        &empty_headers,
        crate::translate::Format::OpenAi,
        wants_stream,
        canonical,
        model,
        false,
    )
    .await
}

pub(crate) fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::from(serde_json::to_vec(&value)?));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_tokens_from_string_messages() {
        // 8 chars -> ceil(8/4) = 2
        let body = json!({"messages": [{"role": "user", "content": "12345678"}]});
        assert_eq!(estimate_anthropic_input_tokens(&body), 2);
    }

    #[test]
    fn counts_system_and_tools() {
        let body = json!({
            "system": "abcd",
            "tools": [{"name": "get_weather"}],
            "messages": [{"role": "user", "content": "abcd"}]
        });
        // system 4 + tools 15 ("name"=4 + "get_weather"=11) + message 4 = 23 -> ceil(23/4) = 6
        assert_eq!(estimate_anthropic_input_tokens(&body), 6);
    }

    #[test]
    fn counts_anthropic_content_blocks() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "abcd"},
                    {"type": "tool_use", "name": "f", "input": {"x": "y"}}
                ]
            }]
        });
        // text 4 + (tool_use name 1 + input {"x":"y"} 2) = 7 -> ceil(7/4) = 2
        assert_eq!(estimate_anthropic_input_tokens(&body), 2);
    }

    #[test]
    fn empty_body_yields_zero() {
        assert_eq!(estimate_anthropic_input_tokens(&json!({})), 0);
    }
}
