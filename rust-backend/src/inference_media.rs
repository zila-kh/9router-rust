use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::{error::AppError, state::AppState};

pub async fn handle_media_voices(
    _state: &AppState,
    method: &Method,
    subpath: &str,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let catalog = crate::providers::catalog();
    let media_catalog = catalog.get("media").and_then(Value::as_object);
    let tts = media_catalog
        .and_then(|m| m.get("tts"))
        .and_then(Value::as_object);

    let provider = subpath.trim_matches('/').split('/').next().unwrap_or("");
    if !provider.is_empty() && provider != "voices" {
        let voices = tts
            .and_then(|t| t.get(provider))
            .and_then(|p| p.get("voices"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        return json_response(StatusCode::OK, json!({ "voices": voices }));
    }

    let all_voices = tts
        .map(|t| {
            let mut list = Vec::new();
            for (p_name, p_val) in t {
                if let Some(v_arr) = p_val.get("voices").and_then(Value::as_array) {
                    for v in v_arr {
                        let mut item = v.clone();
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert("provider".into(), json!(p_name));
                        }
                        list.push(item);
                    }
                }
            }
            Value::Array(list)
        })
        .unwrap_or_else(|| json!([]));

    json_response(StatusCode::OK, json!({ "voices": all_voices }))
}

pub async fn handle_v1_audio_voices(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    handle_media_voices(state, method, "voices").await
}

pub async fn handle_count_tokens(
    _state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let mut chars = 0;
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for m in messages {
            if let Some(content) = m.get("content").and_then(Value::as_str) {
                chars += content.chars().count();
            }
        }
    }
    if let Some(input) = body.get("input").and_then(Value::as_str) {
        chars += input.chars().count();
    }
    // Simple heuristic 4 chars/token approximation
    let tokens = (chars / 4).max(1);
    json_response(StatusCode::OK, json!({ "input_tokens": tokens }))
}

pub async fn handle_v1_search(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    if query.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, json!({"error": "query parameter required"}));
    }

    let searxng_url = std::env::var("SEARXNG_URL").unwrap_or_else(|_| "http://127.0.0.1:8080/search".into());
    let search_req = state.http.get(&searxng_url).query(&[("q", query), ("format", "json")]);
    
    match search_req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({ "results": [] }));
            json_response(StatusCode::OK, data)
        }
        _ => {
            // Fallback to DuckDuckGo instant API
            let ddg_url = "https://api.duckduckgo.com/";
            match state.http.get(ddg_url).query(&[("q", query), ("format", "json"), ("no_html", "1")]).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let data: Value = resp.json().await.unwrap_or(json!({}));
                    let mut results = Vec::new();
                    if let Some(topics) = data.get("RelatedTopics").and_then(Value::as_array) {
                        for t in topics {
                            if let (Some(text), Some(url)) = (t.get("Text").and_then(Value::as_str), t.get("FirstURL").and_then(Value::as_str)) {
                                results.push(json!({ "title": text, "url": url, "content": text }));
                            }
                        }
                    }
                    json_response(StatusCode::OK, json!({ "query": query, "results": results }))
                }
                _ => json_response(StatusCode::OK, json!({ "query": query, "results": [] })),
            }
        }
    }
}

pub async fn handle_v1_web_fetch(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let url = body.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, json!({"error": "url parameter required"}));
    }

    match state.http.get(url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            json_response(
                StatusCode::OK,
                json!({
                    "url": url,
                    "status": status,
                    "content": text
                }),
            )
        }
        Err(e) => json_response(
            StatusCode::BAD_GATEWAY,
            json!({
                "url": url,
                "error": e.to_string()
            }),
        ),
    }
}

pub async fn handle_v1_videos(
    _state: &AppState,
    method: &Method,
    path: &str,
    _body: &Value,
) -> Result<Response<Body>, AppError> {
    let sub = path.strip_prefix("/api/v1/videos").unwrap_or("");
    match method.as_str() {
        "POST" => {
            let task_id = uuid::Uuid::new_v4().to_string();
            json_response(
                StatusCode::OK,
                json!({
                    "id": task_id,
                    "status": "processing",
                    "action": sub.trim_matches('/')
                }),
            )
        }
        "GET" => {
            let id = sub.trim_matches('/');
            json_response(
                StatusCode::OK,
                json!({
                    "id": id,
                    "status": "completed",
                    "video_url": ""
                }),
            )
        }
        _ => json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"})),
    }
}

pub async fn handle_v1_models_info(
    _state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let params: HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let model = params.get("model").map(String::as_str).unwrap_or("");
    let catalog = crate::providers::catalog();
    let models_map = catalog.get("models").and_then(Value::as_object);

    if let Some(m_obj) = models_map.and_then(|m| m.get(model)) {
        return json_response(StatusCode::OK, json!({ "model": m_obj }));
    }

    json_response(
        StatusCode::OK,
        json!({
            "model": {
                "id": model,
                "name": model,
                "max_tokens": 4096,
                "context_window": 128000
            }
        }),
    )
}

pub async fn handle_v1_responses_compact(
    _state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    json_response(StatusCode::OK, json!({ "compact": body }))
}

pub async fn handle_v1beta_models(
    _state: &AppState,
    method: &Method,
    _path: &str,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let catalog = crate::providers::catalog();
    let models = catalog.get("models").cloned().unwrap_or_else(|| json!({}));
    let list: Vec<Value> = models
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, _)| {
                    json!({
                        "name": format!("models/{k}"),
                        "displayName": k,
                        "supportedGenerationMethods": ["generateContent"]
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json_response(StatusCode::OK, json!({ "models": list }))
}

pub async fn handle_v1_api_chat(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("gpt-4o");
    let wants_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let canonical = match crate::translate::normalize_request(body.clone(), crate::translate::Format::OpenAi) {
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
    ).await
}

fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
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
