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
    auth,
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
    if req.method() == axum::http::Method::OPTIONS {
        return cors_preflight();
    }
    let query_key = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.into_owned())
    });
    if is_models_path(&path) && req.method() == axum::http::Method::GET {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        return json_response(StatusCode::OK, providers::all_models_openai());
    }

    authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
    let caller = translate::caller_for_path(&path);
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
    let mut incoming: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON body: {e}")))?;

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

    let targets = combo_targets(&state, &requested)?;
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
        )
        .await
        {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                tracing::warn!(model=%target, error=%e, "model candidate failed");
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
    let settings = state.db.settings()?;
    let local = auth::is_loopback_ip(peer.ip());
    let require = !local
        || settings
            .get("requireApiKey")
            .and_then(Value::as_bool)
            .unwrap_or(true);
    if !require {
        return Ok(());
    }
    let key = auth::extract_api_key(headers, query_key);
    match key {
        Some(k) if state.db.validate_api_key(&k)? => Ok(()),
        _ => Err(AppError::Unauthorized),
    }
}

fn is_models_path(path: &str) -> bool {
    matches!(path, "/v1/models" | "/api/v1/models")
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

async fn execute_target(
    state: &AppState,
    client_headers: &HeaderMap,
    caller: Format,
    wants_stream: bool,
    canonical: Value,
    target: &str,
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
        return Err(AppError::Upstream(format!(
            "{provider} returned HTTP {}: {}",
            status.as_u16(),
            truncate(&text, 2048)
        )));
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
