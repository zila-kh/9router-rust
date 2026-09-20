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
    providers, responses_stream,
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
        if api_key_consumer(&state, req.headers(), query_key.as_deref()) {
            let id = query.as_deref().and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "id")
                    .map(|(_, value)| value.into_owned())
            });
            if id.as_deref() == Some(crate::free_tier::FREE_COMBO_MODEL)
                && crate::free_tier::enabled(&state)
            {
                return json_response(
                    StatusCode::OK,
                    json!({
                        "id": crate::free_tier::FREE_COMBO_MODEL,
                        "name": "9Router Free Tier",
                        "kind": "llm",
                        "owned_by": "9router",
                        "endpoint": "/v1/chat/completions",
                        "description": "Requests are routed to third-party free providers; do not send confidential or personal data.",
                    }),
                );
            }
            if let Some(model) = id
                .as_deref()
                .and_then(|id| crate::free_tier::exposed_virtual_model_info(&state, id))
            {
                return json_response(StatusCode::OK, model);
            }
            if id.as_deref().is_some_and(|id| {
                (crate::free_tier::enabled(&state)
                    && !crate::free_tier::is_exposed_model(&state, id))
                    || crate::free_tier::is_hidden_model(&state, id)
            }) {
                let id = id.unwrap_or_default();
                return json_response(
                    StatusCode::NOT_FOUND,
                    json!({"error":{"message":format!("Model not found: {id}"),"type":"not_found"}}),
                );
            }
        }
        return crate::inference_media::handle_v1_models_info(&state, &method, query.as_deref())
            .await;
    }
    if is_v1beta_models_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let consumer = api_key_consumer(&state, req.headers(), query_key.as_deref());
        return crate::inference_media::handle_v1beta_models(&state, &method, consumer).await;
    }
    if is_models_path(&path) && req.method() == axum::http::Method::GET {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let headers = req.headers().clone();
        let consumer = api_key_consumer(&state, &headers, query_key.as_deref());
        return crate::models_list::handle_models(&state, &method, &path, &headers, consumer).await;
    }
    if is_videos_path(&path) {
        authorize_llm(&state, peer, req.headers(), query_key.as_deref())?;
        let method = req.method().clone();
        let query = req.uri().query().map(str::to_string);
        let headers = req.headers().clone();
        let (_, body) = req.into_parts();
        let bytes = to_bytes(body, MAX_BODY)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read request body: {e}")))?;
        return crate::videos_api::handle(
            &state,
            &method,
            &path,
            query.as_deref(),
            &headers,
            &bytes,
        )
        .await;
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
    let consumer = api_key_consumer(&state, req.headers(), query_key.as_deref());
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
    if consumer && !consumer_target_allowed(&state, &requested, &requested) {
        return Err(AppError::NotFound(format!(
            "The model '{requested}' does not exist or you do not have access to it."
        )));
    }

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
        if consumer && !consumer_target_allowed(&state, &requested, &target) {
            continue;
        }
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
                crate::free_tier::note_result(&state, &target, true);
                if let Some(plan) = auto_plan.as_ref() {
                    // Streamed usage is recorded from the response stream itself
                    // (see `record_usage`), so no estimate row is written here:
                    // the two would double-count the same request.
                    auto_router::remember_success(plan, &target);
                    auto_router::annotate_response(&mut resp, plan, &target);
                }
                return Ok(resp);
            }
            Err(e) => {
                crate::free_tier::note_result(&state, &target, false);
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

fn api_key_consumer(state: &AppState, headers: &HeaderMap, query_key: Option<&str>) -> bool {
    !auth::has_valid_cli_token(state, headers)
        && !auth::has_valid_dashboard_session(state, headers)
        && auth::extract_api_key(headers, query_key).is_some()
}

fn consumer_target_allowed(state: &AppState, requested: &str, target: &str) -> bool {
    if requested == crate::free_tier::FREE_COMBO_MODEL {
        return crate::free_tier::enabled(state);
    }
    if crate::free_tier::enabled(state) && !crate::free_tier::is_exposed_model(state, requested) {
        return false;
    }
    !crate::free_tier::is_hidden_model(state, target)
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

/// `/v1/videos/**` (public) and `/api/v1/videos/**` (internal Next-compat path).
/// `/v1/videos` must stay out of [`crate::media::is_media_path`] — the media
/// router is consulted first in `app.rs`.
fn is_videos_path(path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    let path = path.strip_prefix("/v1").unwrap_or(path);
    path == "/videos" || path.starts_with("/videos/") || path.starts_with("/v1/videos")
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
    if requested == crate::free_tier::FREE_COMBO_MODEL {
        if crate::free_tier::enabled(state) {
            return crate::free_tier::pool_targets(state);
        }
        return Err(AppError::NotFound("combo-free is disabled".into()));
    }
    expand_combo_targets(&state.db.combos()?, requested)
}

fn expand_combo_targets(combos: &[Value], requested: &str) -> Result<Vec<String>, AppError> {
    fn visit(
        combos: &[Value],
        name: &str,
        path: &mut Vec<String>,
        remaining: &mut usize,
        out: &mut Vec<String>,
    ) -> Result<(), AppError> {
        if *remaining == 0 {
            return Err(AppError::BadRequest(
                "combo expansion exceeds the maximum size".into(),
            ));
        }
        *remaining -= 1;
        let Some(combo) = combos
            .iter()
            .find(|combo| combo.get("name").and_then(Value::as_str) == Some(name))
        else {
            out.push(name.to_string());
            return Ok(());
        };
        if path.iter().any(|ancestor| ancestor == name) {
            return Err(AppError::BadRequest(format!(
                "combo reference graph is cyclic at {name}"
            )));
        }
        if path.len() >= 64 {
            return Err(AppError::BadRequest(
                "combo reference graph exceeds the maximum depth".into(),
            ));
        }
        path.push(name.to_string());
        let start = out.len();
        for item in combo
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let target = item.as_str().or_else(|| {
                (item.get("enabled").and_then(Value::as_bool) != Some(false))
                    .then(|| item.get("model").and_then(Value::as_str))
                    .flatten()
            });
            if let Some(target) = target {
                visit(combos, target, path, remaining, out)?;
            }
        }
        path.pop();
        if out.len() == start {
            return Err(AppError::BadRequest(format!(
                "combo {name} has no enabled models"
            )));
        }
        Ok(())
    }

    // Use one DB snapshot, and track ancestors per branch so shared sub-combos
    // remain valid and preserve their position in the fallback order.
    let mut out = Vec::new();
    visit(combos, requested, &mut Vec::new(), &mut 4096, &mut out)?;
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
    let mut last_error = None;
    for candidate in combo_targets(state, target)? {
        let mut payload = canonical.clone();
        payload["model"] = Value::String(candidate.clone());
        match execute_target(
            state,
            headers,
            caller,
            wants_stream,
            payload,
            &candidate,
            compact,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| AppError::NotFound(format!("no route for model {target}"))))
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
    // Custom transports need their dedicated executor.  Never let an explicit
    // model or combo bypass the same executability guard used by auto-routing:
    // treating (for example) Antigravity as generic OpenAI sends the wrong body
    // to the provider's base URL and turns an unsupported route into slow 404s.
    require_executable_provider(&resolved.provider)?;
    let connections = if crate::free_tier::is_virtual_provider(&resolved.provider) {
        vec![
            crate::free_tier::virtual_connection(state, &resolved.provider).ok_or_else(|| {
                AppError::NotFound(format!(
                    "free-tier member {} is not available",
                    resolved.provider
                ))
            })?,
        ]
    } else {
        state
            .db
            .provider_connections(Some(&resolved.provider), Some(true))?
    };
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
            Ok(r) => {
                crate::free_tier::note_result(state, target, true);
                return Ok(r);
            }
            Err(e) => {
                tracing::warn!(provider=%resolved.provider, model=%resolved.model, error=%e, "provider account failed");
                // A request-shaped 4xx says nothing about the credential, so it
                // must not move the conversation to another account (which would
                // abandon a warm prompt cache) or cool a healthy pool member for
                // every later request. Hand the caller the upstream error once.
                if e.request_scoped() {
                    return Err(e);
                }
                crate::free_tier::note_result(state, target, false);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        AppError::Upstream(format!("all accounts failed for {}", resolved.provider))
    }))
}

fn require_executable_provider(provider: &str) -> Result<(), AppError> {
    if auto_router::rust_gateway_supports_provider(provider) {
        return Ok(());
    }
    Err(AppError::NotFound(format!(
        "provider {provider} requires a dedicated transport executor that is not available in the native Rust gateway"
    )))
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
    // `promptCacheTtl` lets a deployment whose turns are more than five minutes
    // apart hold the head cache for an hour instead of rewriting it each turn.
    let cache_ttl = translate::CacheTtl::from_setting(
        state
            .db
            .settings()?
            .get("promptCacheTtl")
            .and_then(Value::as_str),
    );
    let mut upstream_body =
        translate::provider_request_with_cache_ttl(canonical.clone(), provider_format, cache_ttl)?;
    upstream_body["model"] = Value::String(model.to_string());
    apply_provider_body_requirements(provider, &mut upstream_body);

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
        // The client gets these bytes verbatim, so this is the last chance to
        // observe the usage event and price the request.
        let usage_state = state.clone();
        let usage_provider = provider.to_string();
        let usage_model = model.to_string();
        let usage_connection = connection
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage_endpoint = url.clone();
        let mut sniffer = streaming::UsageSniffer::new(provider_format);
        let mut upstream = response.bytes_stream();
        let first_chunk_timeout = state.config.stream_first_chunk_timeout;
        let stall_timeout = state.config.stream_stall_timeout;
        // The client's own format decides the terminal frame shape: a stream that
        // dies after HTTP 200 can no longer change its status code, so the only
        // honest signal left is an in-band error, and a silent close reads as a
        // complete answer to most clients.
        let client_format = caller;
        let stream = async_stream::stream! {
            let mut flowing = false;
            loop {
                let limit = if flowing { stall_timeout } else { first_chunk_timeout };
                let next = match tokio::time::timeout(limit, upstream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        let stage = if flowing { "stalled mid-stream" } else { "sent no first chunk" };
                        yield Ok::<Bytes, std::io::Error>(streaming::abort_terminal_frames(
                            client_format,
                            &format!("upstream {stage} after {}s", limit.as_secs()),
                        ));
                        break;
                    }
                };
                let Some(chunk) = next else { break };
                match chunk {
                    Ok(bytes) => {
                        flowing = true;
                        sniffer.feed(&bytes);
                        yield Ok::<Bytes, std::io::Error>(bytes);
                    }
                    Err(error) => {
                        yield Ok::<Bytes, std::io::Error>(streaming::abort_terminal_frames(
                            client_format,
                            &format!("upstream stream failed: {error}"),
                        ));
                        break;
                    }
                }
            }
            // A client that leaves before any usage event arrived leaves nothing
            // to record; a zero row would only pollute the request list.
            if sniffer.observed() {
                record_usage(
                    &usage_state,
                    &usage_provider,
                    &usage_model,
                    usage_connection.as_deref(),
                    &usage_endpoint,
                    translate::canonical_usage_from_native(&sniffer.native_usage(), provider_format),
                    started.elapsed().as_millis() as u64,
                );
            }
        };
        let mut out = Response::new(Body::from_stream(stream));
        *out.status_mut() = StatusCode::OK;
        copy_response_headers(&headers, out.headers_mut());
        return Ok(out);
    }

    // Cross-format streaming: a Chat Completions caller of a Responses provider.
    // Buffering here would make the client wait for the whole generation before
    // its first token, so the events are translated as they arrive instead.
    if wants_stream
        && upstream_stream
        && caller == Format::OpenAi
        && provider_format == Format::Responses
        && responses_stream::upstream_is_event_stream(content_type.as_deref())
    {
        let headers = response.headers().clone();
        let usage_state = state.clone();
        let usage_provider = provider.to_string();
        let usage_model = model.to_string();
        let usage_connection = connection
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage_endpoint = url.clone();
        let usage_started = started;
        let stream = responses_stream::incremental_chat_stream(
            response.bytes_stream(),
            model.to_string(),
            responses_stream::StreamGuards {
                first_chunk: state.config.stream_first_chunk_timeout,
                stall: state.config.stream_stall_timeout,
            },
            move |pipeline| {
                if pipeline.observed() {
                    record_usage(
                        &usage_state,
                        &usage_provider,
                        &usage_model,
                        usage_connection.as_deref(),
                        &usage_endpoint,
                        pipeline.canonical_usage(),
                        usage_started.elapsed().as_millis() as u64,
                    );
                }
            },
        );
        let mut out = Response::new(Body::from_stream(stream));
        *out.status_mut() = StatusCode::OK;
        copy_response_headers(&headers, out.headers_mut());
        out.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
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
    record_usage(
        state,
        provider,
        model,
        connection.get("id").and_then(Value::as_str),
        &url,
        usage,
        started.elapsed().as_millis() as u64,
    );

    if wants_stream {
        let bytes = streaming::synthesize(&canonical_response, caller)?;
        return bytes_response(StatusCode::OK, "text/event-stream", bytes);
    }
    let final_body = translate::caller_response(canonical_response, caller)?;
    json_response(StatusCode::OK, final_body)
}

/// Write one finished request to the usage ledger.
///
/// `usage` is canonical (see `translate::stored_tokens`), so the cache counters
/// the dashboard reads are recorded rather than dropped, and the cost is priced
/// here because the dashboard only reads the stored number back. Cache reads
/// bill at their own rate, which is what turns a hit into visible savings.
fn record_usage(
    state: &AppState,
    provider: &str,
    model: &str,
    connection_id: Option<&str>,
    endpoint: &str,
    usage: Value,
    duration_ms: u64,
) {
    let tokens = translate::stored_tokens(&usage);
    let user_pricing = state
        .db
        .kv_all("pricing")
        .map(Value::Object)
        .unwrap_or_else(|_| json!({}));
    let cost = crate::pricing::cost_for(Some(&user_pricing), provider, model, &tokens);
    let _ = state.db.usage_record_tokens(
        Some(provider),
        Some(model),
        connection_id,
        endpoint,
        &tokens,
        cost,
        "ok",
        &json!({"durationMs": duration_ms}),
    );
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

fn apply_provider_body_requirements(provider: &str, body: &mut Value) {
    // ChatGPT's Codex Responses endpoint rejects stored responses. The
    // official CLI always sends this explicitly, so enforce it even when the
    // caller used the Chat Completions shape and had no `store` field.
    if provider == "codex" {
        body["store"] = Value::Bool(false);
        if let Some(object) = body.as_object_mut() {
            // The ChatGPT Codex backend controls output limits itself and
            // rejects the public Responses API's max_output_tokens field.
            object.remove("max_output_tokens");
        }
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
    if provider == "opencode-go" {
        let session = client_headers
            .get("x-opencode-session")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        h.insert(
            reqwest::header::HeaderName::from_static("x-opencode-session"),
            reqwest::header::HeaderValue::from_str(&session)
                .map_err(|e| AppError::Internal(e.into()))?,
        );
        h.entry(reqwest::header::USER_AGENT)
            .or_insert(reqwest::header::HeaderValue::from_static(concat!(
                "9router-rust/",
                env!("CARGO_PKG_VERSION")
            )));
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

    fn test_state(temp: &tempfile::TempDir) -> AppState {
        let db_path = temp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        AppState::new(
            crate::config::Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                ui_origin: "http://127.0.0.1:20129".into(),
                data_dir: temp.path().to_path_buf(),
                db_path,
                upstream_timeout_secs: 1,
                stream_first_chunk_timeout: std::time::Duration::from_secs(200),
                stream_stall_timeout: std::time::Duration::from_secs(360),
                ui_only_header_secret: "test-only".into(),
                legacy_backend_origin: None,
                compat_api_enabled: false,
            },
            db,
        )
        .unwrap()
    }

    #[test]
    fn a_finished_request_lands_in_the_ledger_with_cache_and_cost() {
        let temp = tempfile::tempdir().unwrap();
        let state = test_state(&temp);
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "cache_read_input_tokens": 20000,
            "cache_creation_input_tokens": 4000,
        });
        record_usage(
            &state,
            "claude",
            "claude-sonnet-4.5",
            Some("conn-1"),
            "https://api.anthropic.com/v1/messages",
            translate::canonical_usage_from_native(&usage, Format::Claude),
            42,
        );

        let stats = state.db.usage_stats("all").unwrap();
        assert_eq!(stats["totalCachedTokens"], json!(20000));
        assert_eq!(stats["totalPromptTokens"], json!(25000));
        // Rates come from the exported table, which is the same one the
        // dashboard prices with: (1000 uncached * 3 + 20000 cached * 0.3
        //  + 4000 creation * 3 + 50 output * 15) / 1e6.
        let cost = stats["totalCost"].as_f64().unwrap();
        assert!((cost - 0.02175).abs() < 1e-9, "{cost}");

        // A provider that answers without a usage event still counts as a request.
        let before = stats["totalRequests"].as_i64().unwrap();
        record_usage(
            &state,
            "claude",
            "claude-sonnet-4.5",
            Some("conn-1"),
            "https://api.anthropic.com/v1/messages",
            translate::canonical_usage_from_native(&json!({}), Format::Claude),
            5,
        );
        let after = state.db.usage_stats("all").unwrap();
        assert_eq!(after["totalRequests"].as_i64().unwrap(), before + 1);
    }

    #[test]
    fn combo_members_route_internally_and_only_expose_directly_on_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        let state = test_state(&temp);
        let target = "openai-compatible-free-kilo/kilo-auto/free";
        assert!(consumer_target_allowed(
            &state,
            crate::free_tier::FREE_COMBO_MODEL,
            target
        ));
        assert!(!consumer_target_allowed(&state, target, target));

        crate::free_tier::set_exposed(&state, "kilo", true).unwrap();
        assert!(consumer_target_allowed(&state, target, target));
        state
            .db
            .update_settings(json!({"builtinFreeCombo":false}))
            .unwrap();
        assert!(!consumer_target_allowed(
            &state,
            crate::free_tier::FREE_COMBO_MODEL,
            target
        ));
        assert!(consumer_target_allowed(&state, target, target));
    }

    #[tokio::test]
    async fn model_test_resolves_nested_combo_before_provider_lookup() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.upsert_combo(json!({"name":"combo-ui-lite", "models":["combo-free"]}))
            .unwrap();
        db.upsert_combo(json!({"name":"combo-free", "models":["missing-leaf-model"]}))
            .unwrap();
        let state = AppState::new(
            crate::config::Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                ui_origin: "http://127.0.0.1:20129".into(),
                data_dir: temp.path().to_path_buf(),
                db_path,
                upstream_timeout_secs: 1,
                stream_first_chunk_timeout: std::time::Duration::from_secs(200),
                stream_stall_timeout: std::time::Duration::from_secs(360),
                ui_only_header_secret: "test-only".into(),
                legacy_backend_origin: None,
                compat_api_enabled: false,
            },
            db,
        )
        .unwrap();
        let result = execute_target_direct(
            &state,
            &HeaderMap::new(),
            Format::OpenAi,
            false,
            json!({"model":"combo-ui-lite", "messages":[]}),
            "combo-ui-lite",
            false,
        )
        .await;
        assert!(
            matches!(result, Err(AppError::NotFound(ref message)) if message == "no active provider for model missing-leaf-model")
        );
    }

    #[test]
    fn nested_combos_expand_in_fallback_order() {
        let combos = vec![
            json!({"name":"combo-ui-lite", "models":["ocg/mimo-v2.5", "combo-free", "cx/last"]}),
            json!({"name":"combo-free", "models":["tokenrouter/free", {"model":"mmf/mimo-auto", "enabled":true}, {"model":"disabled", "enabled":false}]}),
        ];
        assert_eq!(
            expand_combo_targets(&combos, "combo-ui-lite").unwrap(),
            vec![
                "ocg/mimo-v2.5",
                "tokenrouter/free",
                "mmf/mimo-auto",
                "cx/last"
            ]
        );
        assert_eq!(
            expand_combo_targets(&combos, "cx/direct").unwrap(),
            vec!["cx/direct"]
        );
    }

    #[test]
    fn shared_combos_are_not_cycles() {
        let combos = vec![
            json!({"name":"root", "models":["child", "child"]}),
            json!({"name":"child", "models":["provider/model"]}),
        ];
        assert_eq!(
            expand_combo_targets(&combos, "root").unwrap(),
            vec!["provider/model", "provider/model"]
        );
    }

    #[test]
    fn invalid_combo_graphs_fail_without_recursing_forever() {
        for combos in [
            vec![json!({"name":"root", "models":["root"]})],
            vec![
                json!({"name":"root", "models":["child"]}),
                json!({"name":"child", "models":["root"]}),
            ],
            vec![json!({"name":"root", "models":[{"model":"root", "enabled":false}]})],
        ] {
            assert!(matches!(
                expand_combo_targets(&combos, "root"),
                Err(AppError::BadRequest(_))
            ));
        }
        let deep: Vec<Value> = (0..66)
            .map(|i| json!({"name":format!("c{i}"), "models":[format!("c{}", i+1)]}))
            .collect();
        assert!(expand_combo_targets(&deep, "c0").is_err());
        let broad = vec![json!({"name":"root", "models":vec!["provider/model"; 4097]})];
        assert!(expand_combo_targets(&broad, "root").is_err());
    }

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

    #[test]
    fn opencode_go_preserves_or_generates_a_session_header() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let state = AppState::new(
            crate::config::Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                ui_origin: "http://127.0.0.1:20129".into(),
                data_dir: temp.path().to_path_buf(),
                db_path,
                upstream_timeout_secs: 1,
                stream_first_chunk_timeout: std::time::Duration::from_secs(200),
                stream_stall_timeout: std::time::Duration::from_secs(360),
                ui_only_header_secret: "test-only".into(),
                legacy_backend_origin: None,
                compat_api_enabled: false,
            },
            db,
        )
        .unwrap();
        let transport = json!({"format":"openai"});
        let connection = json!({"apiKey":"test-key"});
        let body = json!({"model":"test", "messages":[]});

        let generated = build_upstream_request(
            &state,
            "opencode-go",
            &transport,
            &connection,
            &HeaderMap::new(),
            "https://example.invalid/v1/chat/completions",
            &body,
        )
        .unwrap()
        .build()
        .unwrap();
        assert!(generated.headers().contains_key("x-opencode-session"));
        assert_eq!(
            generated.headers()[reqwest::header::USER_AGENT],
            concat!("9router-rust/", env!("CARGO_PKG_VERSION"))
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-opencode-session",
            HeaderValue::from_static("stable-session"),
        );
        let preserved = build_upstream_request(
            &state,
            "opencode-go",
            &transport,
            &connection,
            &headers,
            "https://example.invalid/v1/chat/completions",
            &body,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(preserved.headers()["x-opencode-session"], "stable-session");
    }

    #[test]
    fn codex_requests_never_enable_upstream_storage() {
        for initial in [
            json!({"max_output_tokens":16}),
            json!({"store":true, "max_output_tokens":16}),
        ] {
            let mut body = initial;
            apply_provider_body_requirements("codex", &mut body);
            assert_eq!(body["store"], false);
            assert!(body.get("max_output_tokens").is_none());
        }
        let mut other = json!({"store":true, "max_output_tokens":16});
        apply_provider_body_requirements("openai", &mut other);
        assert_eq!(other["store"], true);
        assert_eq!(other["max_output_tokens"], 16);
    }

    #[test]
    fn explicit_models_and_combos_reject_unported_custom_transports() {
        assert!(require_executable_provider("codex").is_ok());
        let error = require_executable_provider("antigravity").unwrap_err();
        assert!(error.to_string().contains("dedicated transport executor"));
    }
}
