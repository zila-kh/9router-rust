use std::net::SocketAddr;

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    auth, auto_router,
    error::AppError,
    providers,
    state::AppState,
    streaming,
    translate::{self, Format},
};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub fn is_llm_path(path: &str) -> bool {
    path == "/responses"
        || path.starts_with("/codex")
        || path.starts_with("/v1/")
        || path == "/v1"
        || path.starts_with("/api/v1/")
        || path == "/api/v1"
        || path.starts_with("/v1beta/")
        || path == "/v1beta"
        || path.starts_with("/api/v1beta/")
        || path == "/api/v1beta"
}

pub async fn handle(
    state: AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let path = req.uri().path().to_string();
    let compact = is_responses_compact_path(&path);
    if req.method() == axum::http::Method::OPTIONS {
        return cors_preflight();
    }
    let query_key = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.into_owned())
    });
    if is_count_tokens_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let (_, body) = req.into_parts();
        let incoming = if method == axum::http::Method::POST {
            let bytes = to_bytes(body, MAX_BODY)
                .await
                .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?
        } else {
            json!({})
        };
        return crate::inference_media::handle_count_tokens(&state, &method, &incoming).await;
    }
    if is_audio_voices_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let query = req.uri().query().map(str::to_string);
        return crate::inference_media::handle_v1_audio_voices(&state, &method, query.as_deref())
            .await;
    }
    if is_models_info_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let query = req.uri().query().map(str::to_string);
        return crate::inference_media::handle_v1_models_info(&state, &method, query.as_deref())
            .await;
    }
    if is_v1beta_models_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        return crate::inference_media::handle_v1beta_models(&state, &method).await;
    }
    if is_models_path(&path) && req.method() == axum::http::Method::GET {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let headers = req.headers().clone();
        return crate::models_list::handle_models(&state, &method, &path, &headers).await;
    }
    if is_search_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let (_, body) = req.into_parts();
        let bytes = to_bytes(body, MAX_BODY)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
        return crate::search_api::handle(&state, &method, &bytes).await;
    }
    if is_web_fetch_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let (_, body) = req.into_parts();
        let bytes = to_bytes(body, MAX_BODY)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
        return crate::web_fetch_api::handle(&state, &method, &bytes).await;
    }

    authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
    let caller = if compact {
        Format::Responses
    } else {
        translate::caller_for_path(&path)
    };
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
    let mut incoming: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;

    if !incoming.is_object() {
        return Err(AppError::BadRequest(
            "JSON request body must be an object".into(),
        ));
    }
    if caller == Format::Gemini && incoming.get("model").is_none() {
        if let Some(model) = gemini_model_from_path(&path) {
            incoming["model"] = Value::String(model);
        }
    }
    let wants_stream =
        request_wants_stream(caller, &parts.headers, parts.uri.query(), &incoming, &path);
    let mut canonical = translate::normalize_request(incoming, caller)?;
    let requested = canonical
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?
        .to_string();

    // Existing combos always win. Auto-routing is intentionally a virtual-model
    // feature and never changes explicit model, alias, combo, or provider behavior.
    let existing_combo = state.db.combo_by_name(&requested)?.is_some();
    let auto_plan = if existing_combo {
        None
    } else {
        auto_router::plan_if_requested(&state, &requested, &canonical, &parts.headers)?
    };
    let targets = match auto_plan.as_ref() {
        Some(plan) => plan.targets.clone(),
        None => combo_targets(&state, &requested)?,
    };

    let mut last_error: Option<AppError> = None;
    for target in targets {
        canonical["model"] = Value::String(target.clone());
        match execute_target(
            &state,
            &parts.headers,
            caller,
            wants_stream,
            canonical.clone(),
            &target,
            compact,
        )
        .await
        {
            Ok(mut resp) => {
                if let Some(plan) = auto_plan.as_ref() {
                    auto_router::record_stream_estimate_if_passthrough(
                        &state,
                        plan,
                        &target,
                        caller,
                        wants_stream,
                    );
                    auto_router::remember_success(plan, &target);
                    auto_router::annotate_response(&mut resp, plan, &target);
                }
                return Ok(resp);
            }
            Err(e) => {
                tracing::warn!(model=%target, error=%e, "model candidate failed");
                if auto_plan.is_some() && !e.auto_route_retryable() {
                    tracing::warn!(model=%target, "auto-router stopped fallback on non-retryable error");
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| AppError::NotFound(format!("no route for model {requested}"))))
}

pub(crate) fn authorize_llm(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    query_key: Option<&str>,
) -> Result<(), AppError> {
    auth::require_llm(state, headers, peer, query_key)
}

/// `/v1/models` and its sub-routes — the kind listing (`/v1/models/image`) and
/// the single-model lookup (`/v1/models/{provider}/{model}`).
/// `/v1/models/info` is a separate route and must stay matched by
/// [`is_models_info_path`] *before* this one.
fn is_models_path(path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    path == "/v1/models" || path.starts_with("/v1/models/")
}

fn is_count_tokens_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/messages/count_tokens" | "/api/v1/messages/count_tokens"
    )
}

/// `/v1/search` (public) and `/api/v1/search` (internal Next-compat path).
fn is_search_path(path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    let path = path.strip_prefix("/v1").unwrap_or(path);
    path == "/search" || path == "/v1/search"
}

/// `/v1/web/fetch` (public) and `/api/v1/web/fetch` (internal path).
fn is_web_fetch_path(path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    let path = path.strip_prefix("/v1").unwrap_or(path);
    path == "/web/fetch" || path == "/v1/web/fetch"
}

fn is_audio_voices_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/audio/voices"
            | "/api/v1/audio/voices"
            | "/v1/v1/audio/voices"
            | "/api/v1/v1/audio/voices"
    )
}

fn is_models_info_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/models/info" | "/api/v1/models/info" | "/v1/v1/models/info" | "/api/v1/v1/models/info"
    )
}

fn is_v1beta_models_path(path: &str) -> bool {
    matches!(path, "/v1beta/models" | "/api/v1beta/models")
}

fn is_responses_compact_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/responses/compact"
            | "/api/v1/responses/compact"
            | "/v1/v1/responses/compact"
            | "/api/v1/v1/responses/compact"
    )
}

/// Upstream `codex.js` sends `/v1/responses/compact` requests to `{base}/compact`;
/// every other provider ignores the flag and keeps the normal responses URL.
fn compact_upstream_url(url: String, provider: &str, compact: bool) -> String {
    if compact && provider == "codex" {
        format!("{url}/compact")
    } else {
        url
    }
}

fn gemini_model_from_path(path: &str) -> Option<String> {
    let marker = "/models/";
    let start = path.find(marker)? + marker.len();
    let rest = &path[start..];
    let model = rest.split(':').next()?.trim_matches('/');
    (!model.is_empty()).then(|| model.to_string())
}

fn request_wants_stream(
    caller: Format,
    headers: &HeaderMap,
    query: Option<&str>,
    body: &Value,
    path: &str,
) -> bool {
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return true;
    }
    if caller == Format::Gemini
        && (path.contains(":streamGenerateContent") || query.unwrap_or("").contains("alt=sse"))
    {
        return true;
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
}

fn combo_targets(state: &AppState, requested: &str) -> Result<Vec<String>, AppError> {
    let Some(combo) = state.db.combo_by_name(requested)? else {
        return Ok(vec![requested.to_string()]);
    };
    let mut out = Vec::new();
    for item in combo
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if let Some(s) = item.as_str() {
            out.push(s.to_string());
        } else if let Some(s) = item.get("model").and_then(Value::as_str) {
            if item.get("enabled").and_then(Value::as_bool) != Some(false) {
                out.push(s.to_string());
            }
        }
    }
    if out.is_empty() {
        return Err(AppError::BadRequest(format!(
            "combo {requested} has no enabled models"
        )));
    }
    Ok(out)
}

pub async fn execute_target_direct(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    caller: Format,
    wants_stream: bool,
    canonical: Value,
    target: &str,
    compact: bool,
) -> Result<Response<Body>, AppError> {
    execute_target(
        state,
        headers,
        caller,
        wants_stream,
        canonical,
        target,
        compact,
    )
    .await
}

async fn execute_target(
    state: &AppState,
    client_headers: &HeaderMap,
    caller: Format,
    wants_stream: bool,
    canonical: Value,
    target: &str,
    compact: bool,
) -> Result<Response<Body>, AppError> {
    let resolved = providers::resolve_model(state, target)?;
    let connections = state
        .db
        .provider_connections(Some(&resolved.provider), Some(true))?;
    if connections.is_empty() {
        return Err(AppError::NotFound(format!(
            "no active connection for provider {}",
            resolved.provider
        )));
    }

    let mut last: Option<AppError> = None;
    for connection in connections {
        match execute_connection(
            state,
            client_headers,
            caller,
            wants_stream,
            canonical.clone(),
            &resolved.provider,
            &resolved.model,
            connection,
            compact,
        )
        .await
        {
            Ok(r) => return Ok(r),
            Err(e) => {
                tracing::warn!(provider=%resolved.provider, model=%resolved.model, error=%e, "provider account failed");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        AppError::Upstream(format!("all accounts failed for {}", resolved.provider))
    }))
}

async fn execute_connection(
    state: &AppState,
    client_headers: &HeaderMap,
    caller: Format,
    wants_stream: bool,
    canonical: Value,
    provider: &str,
    model: &str,
    connection: Value,
    compact: bool,
) -> Result<Response<Body>, AppError> {
    let transport = providers::transport(provider);
    let transport_format = transport
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai");

    match transport_format {
        "kiro" => {
            return crate::special::kiro::execute(
                state,
                caller,
                wants_stream,
                canonical,
                model,
                &connection,
                &transport,
            )
            .await
        }
        "commandcode" => {
            return crate::special::commandcode::execute(
                state,
                caller,
                wants_stream,
                canonical,
                model,
                &connection,
                &transport,
            )
            .await
        }
        "cursor" | "windsurf" => {
            return Err(AppError::Upstream(format!(
                "{transport_format} transport requires the dedicated binary executor"
            )));
        }
        _ => {}
    }

    let provider_format = Format::from_provider(transport_format);
    let mut upstream_body = translate::provider_request(canonical.clone(), provider_format)?;
    upstream_body["model"] = Value::String(model.to_string());

    let force_stream = transport
        .get("forceStream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let upstream_stream = wants_stream || force_stream;
    upstream_body["stream"] = Value::Bool(upstream_stream);

    let (url, _) = providers::endpoint(provider, &connection, "chat", model)?;
    let url = build_format_url(&url, provider_format, model, upstream_stream);
    let url = compact_upstream_url(url, provider, compact);
    let req = build_upstream_request(
        state,
        provider,
        &transport,
        &connection,
        client_headers,
        &url,
        &upstream_body,
    )?;
    let started = std::time::Instant::now();
    let response = req
        .send()
        .await
        .map_err(|e| AppError::Upstream(format!("{provider} request failed: {e}")))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let _ = state.db.usage_record(
            Some(provider),
            Some(model),
            connection.get("id").and_then(Value::as_str),
            &url,
            0,
            0,
            &format!("http_{}", status.as_u16()),
            &json!({"error":text,"durationMs":started.elapsed().as_millis()}),
        );
        return Err(AppError::UpstreamHttp {
            status: status.as_u16(),
            message: format!(
                "{provider} returned HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 2048)
            ),
        });
    }

    if wants_stream && caller == provider_format && upstream_stream {
        let headers = response.headers().clone();
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let mut out = Response::new(Body::from_stream(stream));
        *out.status_mut() = StatusCode::OK;
        copy_response_headers(&headers, out.headers_mut());
        return Ok(out);
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::Upstream(format!("failed reading {provider} response: {e}")))?;
    let native: Value =
        if upstream_stream || streaming::looks_streaming(content_type.as_deref(), &bytes) {
            streaming::reduce_stream(&bytes, provider_format, model)?
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|e| AppError::Upstream(format!("invalid {provider} JSON response: {e}")))?
        };
    let canonical_response = translate::normalize_response(native, provider_format)?;
    let usage = canonical_response
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let _ = state.db.usage_record(
        Some(provider),
        Some(model),
        connection.get("id").and_then(Value::as_str),
        &url,
        usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        "ok",
        &json!({"durationMs":started.elapsed().as_millis()}),
    );

    if wants_stream {
        let bytes = streaming::synthesize(&canonical_response, caller)?;
        return bytes_response(StatusCode::OK, "text/event-stream", bytes);
    }
    let final_body = translate::caller_response(canonical_response, caller)?;
    json_response(StatusCode::OK, final_body)
}

pub(crate) fn build_format_url(base: &str, format: Format, model: &str, stream: bool) -> String {
    if format != Format::Gemini {
        return base.to_string();
    }
    let clean = base.trim_end_matches('/');
    if clean.contains(":generateContent") || clean.contains(":streamGenerateContent") {
        return clean.replace("{model}", model);
    }
    let clean = clean.strip_suffix("/models").unwrap_or(clean);
    if stream {
        format!("{clean}/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("{clean}/models/{model}:generateContent")
    }
}

fn build_upstream_request(
    state: &AppState,
    provider: &str,
    transport: &Value,
    connection: &Value,
    client_headers: &HeaderMap,
    url: &str,
    body: &Value,
) -> Result<reqwest::RequestBuilder, AppError> {
    let mut rb = state.http.post(url).json(body);
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    if let Some(map) = transport.get("headers").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(v) = v.as_str() {
                if let (Ok(n), Ok(v)) = (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                    reqwest::header::HeaderValue::from_str(v),
                ) {
                    h.insert(n, v);
                }
            }
        }
    }
    if let Some((name, value)) = providers::auth_header(connection, transport) {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| AppError::Internal(e.into()))?;
        let value = reqwest::header::HeaderValue::from_str(&value)
            .map_err(|e| AppError::Internal(e.into()))?;
        h.insert(name, value);
    }
    if transport.get("format").and_then(Value::as_str) == Some("claude")
        && !h.contains_key("anthropic-version")
    {
        h.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
    }
    for name in ["x-request-id", "user-agent"] {
        if let Some(v) = client_headers.get(name) {
            if let Ok(v) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
                if let Ok(n) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                    h.entry(n).or_insert(v);
                }
            }
        }
    }
    tracing::debug!(provider, url, "sending upstream request");
    rb = rb.headers(h);
    Ok(rb)
}

fn copy_response_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (k, v) in src {
        if matches!(
            k.as_str().to_ascii_lowercase().as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            dst.append(name, value);
        }
    }
}

fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
    let bytes = serde_json::to_vec(&value)?;
    bytes_response(status, "application/json", bytes)
}

fn bytes_response(
    status: StatusCode,
    content_type: &str,
    bytes: impl Into<Bytes>,
) -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::from(bytes.into()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).map_err(|e| AppError::Internal(e.into()))?,
    );
    Ok(r)
}

fn cors_preflight() -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    r.headers_mut()
        .insert("access-control-allow-origin", HeaderValue::from_static("*"));
    r.headers_mut().insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    r.headers_mut().insert(
        "access-control-allow-headers",
        HeaderValue::from_static("*"),
    );
    Ok(r)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    #[test]
    fn compact_suffix_only_applies_to_codex_responses_urls() {
        let url = "https://chatgpt.com/backend-api/codex/responses".to_string();
        assert_eq!(
            compact_upstream_url(url.clone(), "codex", true),
            "https://chatgpt.com/backend-api/codex/responses/compact"
        );
        assert_eq!(compact_upstream_url(url.clone(), "codex", false), url);
        assert_eq!(
            compact_upstream_url("https://api.openai.com/v1/responses".into(), "openai", true),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn compact_paths_are_recognized() {
        assert!(is_responses_compact_path("/v1/responses/compact"));
        assert!(is_responses_compact_path("/api/v1/responses/compact"));
        assert!(!is_responses_compact_path("/v1/responses"));
    }

    #[test]
    fn model_paths_cover_the_list_kind_and_lookup_routes() {
        for path in [
            "/v1/models",
            "/api/v1/models",
            "/v1/models/image",
            "/v1/models/image-to-text",
            "/v1/models/openai/gpt-5.2",
            "/api/v1/models/gemini/gemini-3.8-flash",
        ] {
            assert!(is_models_path(path), "expected models route: {path}");
        }
        // `/v1/models/info` is dispatched by its own matcher first.
        assert!(is_models_path("/v1/models/info"));
        assert!(is_models_info_path("/v1/models/info"));
        // the `/v1/v1/**` alias stays delegated to the upstream route handlers
        assert!(!is_models_path("/v1/v1/models"));
        assert!(!is_models_path("/v1/models-extra"));
        assert!(!is_models_path("/v1beta/models"));
    }
}
