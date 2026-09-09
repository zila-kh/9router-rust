use std::{convert::Infallible, net::SocketAddr};

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::{error::AppError, gateway, providers, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub fn is_media_path(path: &str) -> bool {
    let p = strip_api_prefix(path);
    p == "/v1/embeddings"
        || p == "/v1/audio/speech"
        || p == "/v1/audio/transcriptions"
        || p == "/v1/audio/translations"
        || p.starts_with("/v1/images/")
        || p == "/v1/search"
        || p.starts_with("/v1/web/")
        || p.starts_with("/v1/videos")
}

fn strip_api_prefix(path: &str) -> &str {
    path.strip_prefix("/api").unwrap_or(path)
}

pub async fn handle(
    state: AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    if req.method() == axum::http::Method::OPTIONS {
        return cors_preflight();
    }
    let path = strip_api_prefix(req.uri().path()).to_string();
    let query_key = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.into_owned())
    });
    gateway::authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;

    match path.as_str() {
        "/v1/embeddings" => embeddings(state, req).await,
        "/v1/audio/speech" => tts(state, req).await,
        "/v1/audio/transcriptions" | "/v1/audio/translations" => stt(state, req).await,
        p if p.starts_with("/v1/images/") => images(state, req).await,
        _ => {
            if state.config.legacy_backend_origin.is_some() {
                let method = req.method().clone();
                let uri = req.uri().clone();
                let headers = req.headers().clone();
                let (_, body) = req.into_parts();
                let raw = to_bytes(body, MAX_BODY)
                    .await
                    .map_err(|e| AppError::BadRequest(format!("legacy media body: {e}")))?;
                crate::legacy_proxy::proxy_buffered(&state, peer, &method, &uri, &headers, raw)
                    .await
            } else {
                Err(AppError::NotFound(format!(
                    "Rust media route not implemented yet: {} {}",
                    req.method(),
                    path
                )))
            }
        }
    }
}

async fn embeddings(state: AppState, req: Request<Body>) -> Result<Response<Body>, AppError> {
    ensure_post(&req)?;
    let bytes = to_bytes(req.into_body(), MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read embeddings body: {e}")))?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Missing model".into()))?;
    if body.get("input").is_none() || body.get("input") == Some(&Value::Null) {
        return Err(AppError::BadRequest("Missing required field: input".into()));
    }

    let resolved = providers::resolve_model(&state, requested)?;
    let conns = state
        .db
        .provider_connections(Some(&resolved.provider), Some(true))?;
    let mut last = None;
    for conn in conns {
        match embedding_once(&state, &resolved.provider, &resolved.model, &conn, &body).await {
            Ok(r) => return Ok(r),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        AppError::NotFound(format!(
            "no active embedding connection for {}",
            resolved.provider
        ))
    }))
}

async fn embedding_once(
    state: &AppState,
    provider: &str,
    model: &str,
    conn: &Value,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let input = body.get("input").cloned().unwrap_or(Value::Null);
    let dimensions = body.get("dimensions").and_then(Value::as_u64);
    let cfg = providers::media_config(provider, "embedding");

    let (url, request_body, mut headers) = if provider == "gemini" {
        let key = credential_token(conn)
            .ok_or_else(|| AppError::BadRequest("Gemini embedding API key missing".into()))?;
        let model_path = if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        };
        let is_batch = input.is_array();
        let op = if is_batch {
            "batchEmbedContents"
        } else {
            "embedContent"
        };
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{model_path}:{op}?key={}",
            url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        );
        let request_body = if let Some(items) = input.as_array() {
            json!({"requests": items.iter().map(|x| {
                let mut v=json!({"model":model_path,"content":{"parts":[{"text":scalar_text(x)}]}});
                if let Some(d)=dimensions {v["outputDimensionality"]=json!(d);} v
            }).collect::<Vec<_>>()})
        } else {
            let mut v =
                json!({"model":model_path,"content":{"parts":[{"text":scalar_text(&input)}]}});
            if let Some(d) = dimensions {
                v["outputDimensionality"] = json!(d);
            }
            v
        };
        (url, request_body, HeaderMap::new())
    } else {
        let (url, _) = providers::endpoint(provider, conn, "embedding", model)?;
        let mut out = json!({
            "model": model,
            "input": input,
            "encoding_format": body.get("encoding_format").cloned().unwrap_or(json!("float"))
        });
        if let Some(d) = dimensions {
            out["dimensions"] = json!(d);
        }
        let headers = media_headers(provider, conn, &cfg)?;
        (url, out, headers)
    };
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let response = state
        .http
        .post(url)
        .headers(headers)
        .json(&request_body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "{provider} embeddings HTTP {}: {}",
            status.as_u16(),
            truncate(&text, 2048)
        )));
    }
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Upstream(format!("invalid {provider} embedding JSON: {e}")))?;
    let normalized = if provider == "gemini" {
        normalize_gemini_embedding(raw, model)
    } else {
        raw
    };
    json_response(StatusCode::OK, normalized)
}

async fn tts(state: AppState, req: Request<Body>) -> Result<Response<Body>, AppError> {
    ensure_post(&req)?;
    let bytes = to_bytes(req.into_body(), MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read TTS body: {e}")))?;
    let mut body: Value = serde_json::from_slice(&bytes)?;
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?;
    let resolved = providers::resolve_model(&state, requested)?;
    let cfg = providers::media_config(&resolved.provider, "tts");
    let format = cfg
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    if format != "openai" {
        return Err(AppError::NotFound(format!(
            "dedicated Rust TTS adapter not implemented for {} format {format}",
            resolved.provider
        )));
    }
    body["model"] = Value::String(resolved.model.clone());
    media_json_passthrough(state, &resolved.provider, &resolved.model, "tts", body).await
}

async fn images(state: AppState, req: Request<Body>) -> Result<Response<Body>, AppError> {
    ensure_post(&req)?;
    let path = req.uri().path().to_string();
    if !path.ends_with("/generations") {
        return Err(AppError::NotFound(format!(
            "Rust image operation currently supports generations; requested {path}"
        )));
    }
    let bytes = to_bytes(req.into_body(), MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read image body: {e}")))?;
    let mut body: Value = serde_json::from_slice(&bytes)?;
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?;
    let resolved = providers::resolve_model(&state, requested)?;
    let cfg = providers::media_config(&resolved.provider, "image");
    let format = cfg
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    if resolved.provider == "gemini" || format.contains("gemini") {
        return Err(AppError::NotFound(
            "Gemini image adapter requires dedicated generateContent normalization".into(),
        ));
    }
    body["model"] = Value::String(resolved.model.clone());
    media_json_passthrough(state, &resolved.provider, &resolved.model, "image", body).await
}

async fn media_json_passthrough(
    state: AppState,
    provider: &str,
    model: &str,
    kind: &str,
    body: Value,
) -> Result<Response<Body>, AppError> {
    let conns = state.db.provider_connections(Some(provider), Some(true))?;
    let mut last = None;
    for conn in conns {
        let cfg = providers::media_config(provider, kind);
        let (url, _) = providers::endpoint(provider, &conn, kind, model)?;
        let headers = media_headers(provider, &conn, &cfg)?;
        match state
            .http
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                return reqwest_response(response).await
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                last = Some(AppError::Upstream(format!(
                    "{provider} {kind} HTTP {}: {}",
                    status.as_u16(),
                    truncate(&text, 2048)
                )));
            }
            Err(e) => {
                last = Some(AppError::Upstream(format!(
                    "{provider} {kind} request failed: {e}"
                )))
            }
        }
    }
    Err(last.unwrap_or_else(|| AppError::NotFound(format!("no active {provider} connection"))))
}

async fn stt(state: AppState, req: Request<Body>) -> Result<Response<Body>, AppError> {
    ensure_post(&req)?;
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("multipart/form-data content-type required".into()))?
        .to_string();
    let boundary = multer::parse_boundary(&content_type)
        .map_err(|e| AppError::BadRequest(format!("invalid multipart boundary: {e}")))?;
    let bytes = to_bytes(req.into_body(), MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read STT multipart: {e}")))?;
    let input_stream = stream::once(async move { Ok::<Bytes, Infallible>(bytes) });
    let mut mp = multer::Multipart::new(input_stream, boundary);
    let mut fields: Vec<OwnedPart> = Vec::new();
    let mut requested: Option<String> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("invalid multipart field: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(str::to_string);
        let mime = field.content_type().map(|m| m.to_string());
        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("failed multipart field {name}: {e}")))?;
        if name == "model" {
            requested = Some(String::from_utf8_lossy(&data).to_string());
        }
        fields.push(OwnedPart {
            name,
            filename,
            mime,
            data,
        });
    }
    let requested = requested.ok_or_else(|| AppError::BadRequest("model is required".into()))?;
    let resolved = providers::resolve_model(&state, requested.trim())?;
    let cfg = providers::media_config(&resolved.provider, "stt");
    let format = cfg
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    if format != "openai" {
        return Err(AppError::NotFound(format!(
            "dedicated Rust STT adapter not implemented for {} format {format}",
            resolved.provider
        )));
    }

    let conns = state
        .db
        .provider_connections(Some(&resolved.provider), Some(true))?;
    let mut last = None;
    for conn in conns {
        let (url, _) = providers::endpoint(&resolved.provider, &conn, "stt", &resolved.model)?;
        let headers = media_headers(&resolved.provider, &conn, &cfg)?;
        let mut form = reqwest::multipart::Form::new();
        for p in &fields {
            if p.name == "model" {
                form = form.text("model", resolved.model.clone());
                continue;
            }
            let mut part = reqwest::multipart::Part::bytes(p.data.to_vec());
            if let Some(filename) = &p.filename {
                part = part.file_name(filename.clone());
            }
            if let Some(mime) = &p.mime {
                part = part
                    .mime_str(mime)
                    .map_err(|e| AppError::BadRequest(format!("invalid multipart mime: {e}")))?;
            }
            form = form.part(p.name.clone(), part);
        }
        match state
            .http
            .post(&url)
            .headers(headers)
            .multipart(form)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                return reqwest_response(response).await
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                last = Some(AppError::Upstream(format!(
                    "{} STT HTTP {}: {}",
                    resolved.provider,
                    status.as_u16(),
                    truncate(&text, 2048)
                )));
            }
            Err(e) => {
                last = Some(AppError::Upstream(format!(
                    "{} STT request failed: {e}",
                    resolved.provider
                )))
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        AppError::NotFound(format!("no active {} connection", resolved.provider))
    }))
}

#[derive(Clone)]
struct OwnedPart {
    name: String,
    filename: Option<String>,
    mime: Option<String>,
    data: Bytes,
}

fn media_headers(provider: &str, conn: &Value, cfg: &Value) -> Result<HeaderMap, AppError> {
    let mut out = HeaderMap::new();
    if let Some(obj) = cfg.get("headers").and_then(Value::as_object) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                insert_header(&mut out, k, s)?;
            }
        }
    }
    let no_auth = providers::provider_entry(provider)
        .and_then(|e| e.get("noAuth"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if no_auth {
        return Ok(out);
    }
    let token = credential_token(conn);
    match cfg.get("authHeader").and_then(Value::as_str).unwrap_or("") {
        "bearer" => {
            if let Some(t) = token {
                insert_header(&mut out, "Authorization", &format!("Bearer {t}"))?;
            }
        }
        "x-api-key" => {
            if let Some(t) = token {
                insert_header(&mut out, "x-api-key", t)?;
            }
        }
        "api-key" => {
            if let Some(t) = token {
                insert_header(&mut out, "api-key", t)?;
            }
        }
        "raw" => {}
        _ => {
            let transport = providers::transport(provider);
            if let Some((name, value)) = providers::auth_header(conn, &transport) {
                insert_header(&mut out, &name, &value)?;
            }
        }
    }
    Ok(out)
}

fn credential_token(conn: &Value) -> Option<&str> {
    conn.get("apiKey")
        .and_then(Value::as_str)
        .or_else(|| conn.get("accessToken").and_then(Value::as_str))
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), AppError> {
    let n = HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| AppError::BadRequest(format!("invalid header {name}: {e}")))?;
    let v = HeaderValue::from_str(value)
        .map_err(|e| AppError::BadRequest(format!("invalid header value for {name}: {e}")))?;
    headers.insert(n, v);
    Ok(())
}

fn normalize_gemini_embedding(raw: Value, model: &str) -> Value {
    if raw.get("object").and_then(Value::as_str) == Some("list") {
        return raw;
    }
    let data = if let Some(items) = raw.get("embeddings").and_then(Value::as_array) {
        items.iter().enumerate().map(|(i,e)|json!({"object":"embedding","index":i,"embedding":e.get("values").cloned().unwrap_or(json!([]))})).collect::<Vec<_>>()
    } else if let Some(values) = raw.pointer("/embedding/values").and_then(Value::as_array) {
        vec![json!({"object":"embedding","index":0,"embedding":values})]
    } else {
        Vec::new()
    };
    json!({"object":"list","data":data,"model":model,"usage":{"prompt_tokens":0,"total_tokens":0}})
}

fn scalar_text(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}
fn ensure_post(req: &Request<Body>) -> Result<(), AppError> {
    if req.method() != axum::http::Method::POST {
        Err(AppError::NotFound(format!(
            "{} {}",
            req.method(),
            req.uri().path()
        )))
    } else {
        Ok(())
    }
}

async fn reqwest_response(response: reqwest::Response) -> Result<Response<Body>, AppError> {
    let status = response.status();
    let headers = response.headers().clone();
    let stream = response
        .bytes_stream()
        .map(|r| r.map_err(std::io::Error::other));
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in headers.iter() {
        if !is_hop_header(name.as_str()) {
            out.headers_mut().append(name.clone(), value.clone());
        }
    }
    out.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    Ok(out)
}
fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
fn cors_preflight() -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    r.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    r.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    r.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    Ok(r)
}
fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::from(serde_json::to_vec(&value)?));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    Ok(r)
}
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.into()
    } else {
        format!(
            "{}…",
            &s[..s
                .char_indices()
                .take_while(|(i, _)| *i < n)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0)]
        )
    }
}
