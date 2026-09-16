use std::net::SocketAddr;

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};

use crate::{auth, error::AppError, login_limiter, providers, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;
const RESET_HINT: &str =
    "Forgot password? Reset to default via 9Router CLI -> Settings -> Reset Password to Default.";

pub async fn handle(
    state: AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let uri = req.uri().clone();
    if !is_public(&method, &path) {
        auth::require_dashboard(&state, &headers)?;
    }
    if method == Method::GET && path == "/api/proxy-pools" {
        return proxy_pools_get(&state, uri.query());
    }
    if method == Method::GET
        && matches!(
            path.as_str(),
            "/api/usage" | "/api/usage/stats" | "/api/usage/history"
        )
    {
        let period = if path == "/api/usage/history" {
            "all".to_string()
        } else {
            uri.query()
                .and_then(|q| {
                    url::form_urlencoded::parse(q.as_bytes())
                        .find(|(k, _)| k == "period")
                        .map(|(_, v)| v.into_owned())
                })
                .unwrap_or_else(|| "7d".into())
        };
        return json_response(StatusCode::OK, state.db.usage_stats(&period)?);
    }
    if method == Method::DELETE && path == "/api/models/custom" {
        let params: std::collections::HashMap<String, String> = uri
            .query()
            .map(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let provider = params
            .get("providerAlias")
            .ok_or_else(|| AppError::BadRequest("providerAlias and id required".into()))?;
        let id = params
            .get("id")
            .ok_or_else(|| AppError::BadRequest("providerAlias and id required".into()))?;
        let ty = params.get("type").map(String::as_str).unwrap_or("llm");
        let key = format!("{provider}|{id}|{ty}");
        state.db.kv_delete("customModels", &key)?;
        return json_response(StatusCode::OK, json!({"success":true}));
    }
    if path == "/api/models/disabled" {
        let (_, b) = req.into_parts();
        let raw = to_bytes(b, MAX_BODY)
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let body: Value = if !raw.is_empty() {
            serde_json::from_slice(&raw).unwrap_or(json!({}))
        } else {
            json!({})
        };
        return crate::models_mgmt::handle_models_disabled(&state, &method, uri.query(), &body);
    }
    if path == "/api/usage/chart" {
        return crate::usage_mgmt::handle_usage_chart(&state, &method, uri.query());
    }
    if path == "/api/usage/providers" {
        return crate::usage_mgmt::handle_usage_providers(&state, &method);
    }
    if path == "/api/usage/request-details" {
        return crate::usage_mgmt::handle_usage_request_details(&state, &method, uri.query());
    }
    if path == "/api/usage/logs" || path == "/api/usage/request-logs" {
        return crate::usage_mgmt::handle_usage_logs(&state, &method);
    }
    if path == "/api/usage/stream" {
        return crate::usage_mgmt::handle_usage_stream(&state, &method);
    }
    if path == "/api/translator/load" {
        return crate::usage_mgmt::handle_translator_load(&method, uri.query());
    }
    if path == "/api/translator/console-logs" {
        return crate::usage_mgmt::handle_translator_console_logs(&method);
    }
    if path == "/api/translator/console-logs/stream" {
        return crate::usage_mgmt::handle_translator_console_stream(&method);
    }
    if let Some(conn_id) = path.strip_prefix("/api/usage/") {
        if let Some(sub) = conn_id.strip_suffix("/codex-reset-credits") {
            return crate::usage_mgmt::handle_usage_codex_reset(&state, &method, sub).await;
        } else if !conn_id.contains('/') {
            return crate::usage_mgmt::handle_usage_connection(&state, &method, conn_id);
        }
    }
    if path.starts_with("/api/media-providers/tts/") {
        let sub = path.strip_prefix("/api/media-providers/tts/").unwrap_or("");
        return crate::inference_media::handle_media_voices(&state, &method, sub, uri.query())
            .await;
    }
    if path == "/api/v1/audio/voices" {
        return crate::inference_media::handle_v1_audio_voices(&state, &method, uri.query()).await;
    }
    if path == "/api/v1/models/info" {
        return crate::inference_media::handle_v1_models_info(&state, &method, uri.query()).await;
    }
    if path.starts_with("/api/v1beta/models") {
        return crate::inference_media::handle_v1beta_models(&state, &method).await;
    }
    if path == "/api/providers/client" {
        return crate::providers_oauth::handle_providers_client(&state, &method).await;
    }
    let (_, b) = req.into_parts();
    let raw = to_bytes(b, MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let body = if matches!(method, Method::POST | Method::PUT | Method::PATCH) {
        if raw.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice::<Value>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    let is_form = headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|ct| ct.to_str().ok())
                        .map(|ct| ct.starts_with("application/x-www-form-urlencoded"))
                        .unwrap_or(false);
                    if is_form {
                        let mut map = serde_json::Map::new();
                        for (k, v) in url::form_urlencoded::parse(&raw) {
                            map.insert(k.into_owned(), Value::String(v.into_owned()));
                        }
                        Value::Object(map)
                    } else if state.config.legacy_backend_origin.is_some() {
                        return crate::legacy_proxy::proxy_buffered(
                            &state, peer, &method, &uri, &headers, raw,
                        )
                        .await;
                    } else {
                        return Err(AppError::BadRequest(e.to_string()));
                    }
                }
            }
        }
    } else {
        json!({})
    };
    match dispatch(&state, peer, &headers, &method, &path, body).await {
        Err(AppError::NotFound(_)) if state.config.legacy_backend_origin.is_some() => {
            crate::legacy_proxy::proxy_buffered(&state, peer, &method, &uri, &headers, raw).await
        }
        other => other,
    }
}

fn is_public(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/health")
            | ("GET", "/api/init")
            | ("GET", "/api/version")
            | ("POST", "/api/locale")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("GET", "/api/settings/require-login")
            | ("GET", "/api/auth/oidc/start")
            | ("GET", "/api/auth/oidc/callback")
            | ("GET", "/api/auth/saml/start")
            | ("POST", "/api/auth/saml/acs")
            | ("GET", "/api/auth/saml/metadata")
    )
}

async fn dispatch(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
    body: Value,
) -> Result<Response<Body>, AppError> {
    match (method.as_str(), path) {
        ("GET", "/api/health") => json_response(
            StatusCode::OK,
            json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"runtime":"rust","upstreamSnapshot":"17c4cc76877bd1755030a8414f8d0083f48dcccf"}),
        ),
        ("GET", "/api/init") => json_response(
            StatusCode::OK,
            json!({"initialized":true,"runtime":"rust","version":env!("CARGO_PKG_VERSION")}),
        ),
        ("GET", "/api/version") => json_response(
            StatusCode::OK,
            json!({"version":env!("CARGO_PKG_VERSION"),"name":"9router-rust","rustBackend":true,"upstreamVersion":"0.5.75"}),
        ),
        ("POST", "/api/locale") => locale_update(body),
        ("GET", "/api/rust/parity") => json_response(
            StatusCode::OK,
            serde_json::from_str(include_str!("../parity/routes.json"))?,
        ),
        ("GET", "/api/settings/require-login") => {
            let s = state.db.settings()?;
            json_response(
                StatusCode::OK,
                json!({"requireLogin":s.get("requireLogin").and_then(Value::as_bool).unwrap_or(true)}),
            )
        }
        ("POST", "/api/auth/login") => login(state, peer, headers, body),
        ("POST", "/api/auth/logout") => logout(headers),
        ("GET", "/api/auth/status") => auth_status(state, headers),
        ("POST", "/api/auth/reset-password") => reset_password(state, peer, headers),
        ("GET", "/api/settings") => settings_get(state),
        ("PATCH", "/api/settings") | ("PUT", "/api/settings") => settings_update(state, body),
        ("GET", "/api/settings/database") => settings_database_get(state, headers),
        ("POST", "/api/settings/database") => settings_database_post(state, body),
        ("GET", "/api/providers") => providers_get(state),
        ("POST", "/api/providers") => providers_post(state, body),
        ("GET", "/api/provider-nodes") => json_response(
            StatusCode::OK,
            json!({"nodes":state.db.list_json_table("providerNodes")?}),
        ),
        ("POST", "/api/provider-nodes") => provider_node_post(state, body),
        ("GET", "/api/proxy-pools") => proxy_pools_get(state, None),
        ("POST", "/api/proxy-pools") => proxy_pool_post(state, body),
        ("GET", "/api/keys") => json_response(StatusCode::OK, json!({"keys":state.db.api_keys()?})),
        ("POST", "/api/keys") => keys_post(state, body),
        ("GET", "/api/combos") => {
            json_response(StatusCode::OK, json!({"combos":state.db.combos()?}))
        }
        ("POST", "/api/combos") => combos_post(state, body),
        ("GET", "/api/models") => models_get(state),
        ("PUT", "/api/models") => model_legacy_alias(state, body),
        ("POST", "/api/models/alias") | ("PUT", "/api/models/alias") => model_alias(state, body),
        ("GET", "/api/models/custom") => json_response(
            StatusCode::OK,
            json!({"models":state.db.kv_all("customModels")?.into_values().collect::<Vec<_>>()}),
        ),
        ("POST", "/api/models/custom") => custom_model_post(state, body),
        ("GET", "/api/models/availability") | ("POST", "/api/models/availability") => {
            crate::models_mgmt::handle_models_availability(state, method, &body)
        }
        ("GET", "/api/models/catalog-sync") | ("POST", "/api/models/catalog-sync") => {
            crate::models_mgmt::handle_models_catalog_sync(state, method)
        }
        ("POST", "/api/models/test") => {
            crate::models_mgmt::handle_models_test(state, method, &body).await
        }
        ("POST", "/api/translator/save") => {
            crate::usage_mgmt::handle_translator_save(method, &body)
        }
        ("POST", "/api/translator/send") => {
            crate::usage_mgmt::handle_translator_send(state, method, &body).await
        }
        ("POST", "/api/settings/proxy-test") => {
            crate::usage_mgmt::handle_settings_proxy_test(state, method, &body).await
        }
        ("GET", "/api/pricing") => pricing_get(state),
        ("POST", "/api/translator/translate") => translator_translate(state, body),
        _ => dynamic(state, method, path, body).await,
    }
}

/// Combo names accept letters, numbers, `-`, `_` and `.` only (upstream rule).
static COMBO_NAME_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^[a-zA-Z0-9_.\-]+$").expect("combo name regex"));

const COMBO_NAME_CHARSET_ERROR: &str = "Name can only contain letters, numbers, -, _ and .";
const COMBO_NAME_TAKEN_ERROR: &str = "Combo name already exists";

fn validate_combo_name(name: &str) -> Result<(), AppError> {
    if !COMBO_NAME_RE.is_match(name) {
        return Err(AppError::BadRequest(COMBO_NAME_CHARSET_ERROR.into()));
    }
    Ok(())
}

/// Rejects a name already owned by a different combo (`self_id` = combo being renamed).
fn ensure_combo_name_available(
    state: &AppState,
    name: &str,
    self_id: Option<&str>,
) -> Result<(), AppError> {
    if let Some(existing) = state.db.combo_by_name(name)? {
        if existing.get("id").and_then(Value::as_str) != self_id {
            return Err(AppError::BadRequest(COMBO_NAME_TAKEN_ERROR.into()));
        }
    }
    Ok(())
}

fn combos_post(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Name is required".into()))?;
    validate_combo_name(name)?;
    ensure_combo_name_available(state, name, None)?;
    let c = state.db.upsert_combo(body)?;
    // Upstream returns the created row at the top level (media pages read `created.id`).
    json_response(StatusCode::CREATED, c)
}

fn login(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    body: Value,
) -> Result<Response<Body>, AppError> {
    let is_html_client = headers
        .get(header::ACCEPT)
        .and_then(|a| a.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false);
    let ip = auth::rate_limit_ip(peer, headers);
    let lock = login_limiter::check_lock(ip);
    if lock.locked {
        return login_locked_response(lock.retry_after_secs);
    }

    let settings = state.db.settings()?;
    if is_tunnel_request(headers, &settings)
        && settings
            .get("tunnelDashboardAccess")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return json_response_no_store(
            StatusCode::FORBIDDEN,
            json!({"error":"Dashboard access via tunnel is disabled"}),
        );
    }
    if let Some(message) = password_login_disabled(&settings) {
        return json_response_no_store(StatusCode::FORBIDDEN, json!({"error":message}));
    }

    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    if !auth::verify_password(state, password)? {
        let remaining = login_limiter::record_failure(ip);
        let post_lock = login_limiter::check_lock(ip);
        if post_lock.locked {
            return login_locked_response(post_lock.retry_after_secs);
        }
        return json_response_no_store(
            StatusCode::UNAUTHORIZED,
            json!({
                "error":format!("Invalid password. {remaining} attempt(s) left before lockout."),
                "remainingBeforeLock":remaining
            }),
        );
    }
    login_limiter::record_success(ip);

    let stored = settings
        .get("password")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let env_initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if stored.is_none() && env_initial.is_none() && !auth::is_direct_loopback_request(peer, headers)
    {
        return json_response_no_store(
            StatusCode::FORBIDDEN,
            json!({"success":false,"error":"Default password must be changed before remote access. Change it from the local machine (or set INITIAL_PASSWORD).","mustChangePassword":true}),
        );
    }

    let token = auth::create_session_token(state, Map::new())?;
    if is_html_client {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SEE_OTHER;
        response
            .headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/dashboard"));
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().append(
            header::SET_COOKIE,
            auth::session_cookie_header(headers, &token),
        );
        return Ok(response);
    }
    let mut response = json_response_no_store(
        StatusCode::OK,
        json!({"success":true,"mustChangePassword":false}),
    )?;
    response.headers_mut().append(
        header::SET_COOKIE,
        auth::session_cookie_header(headers, &token),
    );
    Ok(response)
}

fn logout(headers: &HeaderMap) -> Result<Response<Body>, AppError> {
    let mut response = json_response_no_store(StatusCode::OK, json!({"success":true}))?;
    response.headers_mut().append(
        header::SET_COOKIE,
        auth::clear_session_cookie_header(headers),
    );
    Ok(response)
}

fn auth_status(state: &AppState, headers: &HeaderMap) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let authenticated = auth::cookie(headers, "auth_token")
        .map(|token| auth::verify_session_token(state, &token))
        .unwrap_or(false);
    let oidc = oidc_configured(&settings);
    let saml = saml_configured(&settings);
    json_response_no_store(
        StatusCode::OK,
        json!({"requireLogin":settings.get("requireLogin").and_then(Value::as_bool).unwrap_or(true),"authMode":settings.get("authMode").cloned().unwrap_or(json!("password")),"ssoType":settings.get("ssoType").cloned().unwrap_or(json!("oidc")),"oidcConfigured":oidc,"oidcLoginLabel":settings.get("oidcLoginLabel").cloned().unwrap_or(json!("Sign in with OIDC")),"samlConfigured":saml,"samlLoginLabel":settings.get("samlLoginLabel").cloned().unwrap_or(json!("Sign in with SAML SSO")),"hasPassword":settings.get("password").and_then(Value::as_str).is_some_and(|value|!value.trim().is_empty()),"displayName":"Password user","loginMethod":"Password","authenticated":authenticated,"oidcName":null,"oidcEmail":null,"oidcLogin":false,"samlName":null,"samlEmail":null,"samlLogin":false}),
    )
}

fn login_locked_response(retry_after_secs: u64) -> Result<Response<Body>, AppError> {
    let mut response = json_response_no_store(
        StatusCode::TOO_MANY_REQUESTS,
        json!({
            "error":format!("Too many failed attempts. Try again in {retry_after_secs}s. {RESET_HINT}"),
            "retryAfter":retry_after_secs,
            "resetHint":RESET_HINT
        }),
    )?;
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_secs.to_string())
            .map_err(|error| AppError::Internal(error.into()))?,
    );
    Ok(response)
}

fn password_login_disabled(settings: &Value) -> Option<&'static str> {
    let mode = settings
        .get("authMode")
        .and_then(Value::as_str)
        .unwrap_or("password");
    if !matches!(mode, "sso" | "saml" | "oidc") {
        return None;
    }
    let sso_type = settings
        .get("ssoType")
        .and_then(Value::as_str)
        .unwrap_or(if mode == "saml" { "saml" } else { "oidc" });
    match sso_type {
        "saml" if saml_configured(settings) => {
            Some("Password login is disabled. Use SAML SSO sign in.")
        }
        "oidc" if oidc_configured(settings) => {
            Some("Password login is disabled. Use OIDC sign in.")
        }
        _ => None,
    }
}

fn nonempty_setting(settings: &Value, key: &str) -> bool {
    settings
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn oidc_configured(settings: &Value) -> bool {
    nonempty_setting(settings, "oidcIssuerUrl")
        && nonempty_setting(settings, "oidcClientId")
        && nonempty_setting(settings, "oidcClientSecret")
}

fn saml_configured(settings: &Value) -> bool {
    nonempty_setting(settings, "samlEntryPoint") && nonempty_setting(settings, "samlCert")
}

fn is_tunnel_request(headers: &HeaderMap, settings: &Value) -> bool {
    let Some(host) = request_hostname(headers) else {
        return false;
    };
    ["tunnelUrl", "tailscaleUrl"].iter().any(|key| {
        settings
            .get(*key)
            .and_then(Value::as_str)
            .and_then(|value| url::Url::parse(value).ok())
            .and_then(|value| value.host_str().map(str::to_string))
            .is_some_and(|configured| configured.eq_ignore_ascii_case(&host))
    })
}

fn request_hostname(headers: &HeaderMap) -> Option<String> {
    let authority = headers
        .get(header::HOST)?
        .to_str()
        .ok()?
        .parse::<axum::http::uri::Authority>()
        .ok()?;
    Some(
        authority
            .host()
            .trim_matches(['[', ']'])
            .to_ascii_lowercase(),
    )
}

fn reset_password(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<Response<Body>, AppError> {
    if !auth::is_direct_loopback_request(peer, headers) {
        return Err(AppError::Forbidden("Local only".into()));
    }
    state.db.update_settings(json!({"password": null}))?;
    json_response(StatusCode::OK, json!({"success": true}))
}

fn detect_body_format(body: &Value) -> crate::translate::Format {
    // Mirror open-sse/services/provider.js detectFormat() from the pinned snapshot.
    if body.get("input").is_some()
        && matches!(
            body.get("input"),
            Some(Value::Array(_)) | Some(Value::String(_))
        )
        && body.get("messages").is_none()
    {
        return crate::translate::Format::Responses;
    }

    // Antigravity is Gemini-shaped. The diagnostic Rust translator does not expose
    // a separate Antigravity enum yet, so use Gemini as the lossless intermediate.
    if body
        .pointer("/request/contents")
        .and_then(Value::as_array)
        .is_some()
        && body.get("userAgent").and_then(Value::as_str) == Some("antigravity")
    {
        return crate::translate::Format::Gemini;
    }

    if body.get("contents").and_then(Value::as_array).is_some() {
        return crate::translate::Format::Gemini;
    }

    let openai_indicator = body.get("stream_options").is_some()
        || body.get("response_format").is_some()
        || body.get("logprobs").is_some()
        || body.get("top_logprobs").is_some()
        || body.get("n").is_some()
        || body.get("presence_penalty").is_some()
        || body.get("frequency_penalty").is_some()
        || body.get("logit_bias").is_some()
        || body.get("user").is_some();
    if openai_indicator {
        return crate::translate::Format::OpenAi;
    }

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        if let Some(first) = messages.first() {
            if let Some(content) = first.get("content").and_then(Value::as_array) {
                let first_type = content
                    .first()
                    .and_then(|v| v.get("type"))
                    .and_then(Value::as_str);
                let model_has_slash = body
                    .get("model")
                    .and_then(Value::as_str)
                    .map(|m| m.contains('/'))
                    .unwrap_or(false);
                if first_type == Some("text") && !model_has_slash {
                    if body.get("system").is_some() || body.get("anthropic_version").is_some() {
                        return crate::translate::Format::Claude;
                    }
                    let has_claude_image = content.iter().any(|c| {
                        c.get("type").and_then(Value::as_str) == Some("image")
                            && c.pointer("/source/type").and_then(Value::as_str) == Some("base64")
                    });
                    if has_claude_image {
                        return crate::translate::Format::Claude;
                    }
                    let has_openai_image = content.iter().any(|c| {
                        c.get("type").and_then(Value::as_str) == Some("image_url")
                            && c.pointer("/image_url/url").is_some()
                    });
                    if has_openai_image {
                        return crate::translate::Format::OpenAi;
                    }
                    let has_claude_tool = content.iter().any(|c| {
                        matches!(
                            c.get("type").and_then(Value::as_str),
                            Some("tool_use") | Some("tool_result")
                        )
                    });
                    if has_claude_tool {
                        return crate::translate::Format::Claude;
                    }
                }
            }
        }
        if body.get("system").is_some() || body.get("anthropic_version").is_some() {
            return crate::translate::Format::Claude;
        }
    }

    crate::translate::Format::OpenAi
}

fn format_name(format: crate::translate::Format) -> &'static str {
    match format {
        crate::translate::Format::OpenAi => "openai",
        crate::translate::Format::Claude => "claude",
        crate::translate::Format::Gemini => "gemini",
        crate::translate::Format::Responses => "openai-responses",
    }
}

fn translator_translate(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let step = body
        .get("step")
        .and_then(Value::as_i64)
        .ok_or_else(|| AppError::BadRequest("Step and body required".into()))?;
    let wrapper = body
        .get("body")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("Step and body required".into()))?;

    match step {
        1 | 2 => {
            let client_body = wrapper.get("body").cloned().unwrap_or(wrapper);
            let requested = client_body
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::BadRequest("model is required".into()))?;
            let resolved = providers::resolve_model(state, requested)?;
            let source = detect_body_format(&client_body);
            let target_transport = providers::transport(&resolved.provider);
            let target = crate::translate::Format::from_provider(
                target_transport
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or("openai"),
            );
            if step == 1 {
                return json_response(
                    StatusCode::OK,
                    json!({
                        "success": true,
                        "result": {
                            "provider": resolved.provider,
                            "model": resolved.model,
                            "sourceFormat": format_name(source),
                            "targetFormat": format_name(target)
                        }
                    }),
                );
            }
            let mut result = crate::translate::normalize_request(client_body, source)?;
            result["model"] = json!(resolved.model);
            json_response(
                StatusCode::OK,
                json!({"success": true, "result": {"body": result}}),
            )
        }
        3 => {
            let openai_body = wrapper
                .get("body")
                .cloned()
                .unwrap_or_else(|| wrapper.clone());
            let provider = wrapper
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::BadRequest("provider and model required".into()))?;
            let model = wrapper
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::BadRequest("provider and model required".into()))?;
            let transport = providers::transport(provider);
            let transport_name = transport
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("openai");
            if matches!(
                transport_name,
                "kiro" | "commandcode" | "cursor" | "windsurf"
            ) {
                return Err(AppError::NotFound(format!(
                    "translator step 3 for special transport {transport_name} requires its dedicated executor"
                )));
            }
            let target = crate::translate::Format::from_provider(transport_name);
            let stream = openai_body
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let mut translated = crate::translate::provider_request(openai_body, target)?;
            translated["model"] = json!(model);
            translated["stream"] = json!(stream);
            let connection = state
                .db
                .provider_connections(Some(provider), Some(true))?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    AppError::BadRequest(format!("No active connection for provider: {provider}"))
                })?;
            let (base, _) = providers::endpoint(provider, &connection, "chat", model)?;
            let url = crate::gateway::build_format_url(&base, target, model, stream);
            let mut headers = Map::new();
            if let Some(map) = transport.get("headers").and_then(Value::as_object) {
                for (key, value) in map {
                    if value.is_string() {
                        headers.insert(key.clone(), value.clone());
                    }
                }
            }
            if let Some((name, value)) = providers::auth_header(&connection, &transport) {
                headers.insert(name, json!(value));
            }
            if transport_name == "claude" && !headers.contains_key("anthropic-version") {
                headers.insert("anthropic-version".into(), json!("2023-06-01"));
            }
            json_response(
                StatusCode::OK,
                json!({"success": true, "result": {"url": url, "headers": headers, "body": translated}}),
            )
        }
        _ => Err(AppError::BadRequest("Invalid step (1-3)".into())),
    }
}

fn settings_database_get(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Response<Body>, AppError> {
    let password = headers
        .get("x-9r-password")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !auth::verify_password(state, password)? {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "Invalid password"}),
        );
    }
    json_response(StatusCode::OK, state.db.export_db()?)
}

fn settings_database_post(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let mut payload = body
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::BadRequest("Invalid database payload".into()))?;
    let password = payload
        .remove("password")
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    if !auth::verify_password(state, &password)? {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "Invalid password"}),
        );
    }
    state.db.import_db(&Value::Object(payload))?;
    json_response(StatusCode::OK, json!({"success": true}))
}

fn settings_get(state: &AppState) -> Result<Response<Body>, AppError> {
    let mut s = state.db.settings()?;
    if let Some(o) = s.as_object_mut() {
        for k in ["password", "oidcClientSecret"] {
            o.remove(k);
        }
    }
    json_response(StatusCode::OK, json!({"settings":s}))
}
fn settings_update(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Invalid settings payload".into()))?;

    object.remove("password");
    object.remove("mitmSudoEncrypted");
    let new_password = object
        .remove("newPassword")
        .and_then(|value| value.as_str().map(str::to_string))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let current_password = object
        .remove("currentPassword")
        .and_then(|value| value.as_str().map(str::to_string));

    if object
        .get("oidcClientSecret")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().is_empty())
    {
        object.remove("oidcClientSecret");
    }

    if let Some(new_password) = new_password {
        let settings = state.db.settings()?;
        let has_stored_password = settings
            .get("password")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        if has_stored_password {
            let Some(current_password) = current_password.as_deref() else {
                return json_response_no_store(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Current password required"}),
                );
            };
            if !auth::verify_password(state, current_password)? {
                return json_response_no_store(
                    StatusCode::UNAUTHORIZED,
                    json!({"error":"Invalid current password"}),
                );
            }
        } else if let Some(current_password) = current_password
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            if !auth::verify_password(state, current_password)? {
                return json_response_no_store(
                    StatusCode::UNAUTHORIZED,
                    json!({"error":"Invalid current password"}),
                );
            }
        }
        object.insert(
            "password".into(),
            json!(bcrypt::hash(new_password, 10)
                .map_err(|error| AppError::Internal(anyhow::anyhow!(error.to_string())))?),
        );
    }

    let settings = state.db.update_settings(body)?;
    let mut safe = settings.clone();
    if let Some(object) = safe.as_object_mut() {
        for key in ["password", "oidcClientSecret"] {
            object.remove(key);
        }
    }
    json_response_no_store(StatusCode::OK, json!({"settings":safe,"success":true}))
}

fn providers_get(state: &AppState) -> Result<Response<Body>, AppError> {
    let mut cs = state.db.provider_connections(None, None)?;
    for connection in &mut cs {
        redact_secrets(connection);
    }
    json_response(StatusCode::OK, json!({"connections":cs}))
}
fn providers_post(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Invalid provider".into()))?
        .to_string();
    if providers::provider_entry(&provider).is_none()
        && !provider.starts_with("openai-compatible-")
        && !provider.starts_with("anthropic-compatible-")
    {
        return Err(AppError::BadRequest("Invalid provider".into()));
    }
    if body.get("authType").is_none() {
        body["authType"] = json!("apikey")
    }
    if body.get("name").is_none() {
        body["name"] = providers::provider_entry(&provider)
            .and_then(|p| p.pointer("/display/name"))
            .cloned()
            .unwrap_or(json!(provider.clone()))
    }
    let mut c = state.db.create_connection(body)?;
    redact_secrets(&mut c);
    json_response(StatusCode::CREATED, json!({"connection":c}))
}

fn proxy_pools_get(state: &AppState, query: Option<&str>) -> Result<Response<Body>, AppError> {
    let params: std::collections::HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let active_filter = params.get("isActive").and_then(|v| match v.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    });
    let include_usage = params.get("includeUsage").map(String::as_str) == Some("true");

    let mut pools = state.db.list_json_table("proxyPools")?;
    if let Some(active) = active_filter {
        pools.retain(|pool| {
            pool.get("isActive")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                == active
        });
    }

    if include_usage {
        let connections = state.db.provider_connections(None, None)?;
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for connection in connections {
            if let Some(id) = connection
                .pointer("/providerSpecificData/proxyPoolId")
                .and_then(Value::as_str)
            {
                *counts.entry(id.to_string()).or_default() += 1;
            }
        }
        for pool in &mut pools {
            if let Some(object) = pool.as_object_mut() {
                let id = object.get("id").and_then(Value::as_str).unwrap_or("");
                object.insert(
                    "boundConnectionCount".into(),
                    json!(counts.get(id).copied().unwrap_or(0)),
                );
            }
        }
    }

    pools.sort_by(|a, b| {
        b.get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(a.get("updatedAt").and_then(Value::as_str).unwrap_or(""))
    });
    json_response(StatusCode::OK, json!({"proxyPools": pools}))
}

fn proxy_pool_post(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Name is required".into()))?
        .to_string();
    let url = body
        .get("proxyUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Proxy URL is required".into()))?
        .to_string();
    let ty = body
        .get("type")
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "http" | "vercel" | "cloudflare" | "deno"))
        .unwrap_or("http")
        .to_string();
    body["name"] = json!(name);
    body["proxyUrl"] = json!(url);
    body["type"] = json!(ty);
    if body.get("noProxy").is_none() {
        body["noProxy"] = json!("")
    }
    if body.get("isActive").is_none() {
        body["isActive"] = json!(true)
    }
    if body.get("strictProxy").is_none() {
        body["strictProxy"] = json!(false)
    }
    body["testStatus"] = body.get("testStatus").cloned().unwrap_or(json!("unknown"));
    let pool = state.db.create_proxy_pool(body)?;
    json_response(StatusCode::CREATED, json!({"proxyPool":pool}))
}

fn provider_node_post(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Name is required".into()))?
        .to_string();
    let prefix = body
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Prefix is required".into()))?
        .to_string();
    let ty = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("openai-compatible")
        .to_string();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let id = match ty.as_str() {
        "openai-compatible" => {
            let api = body
                .get("apiType")
                .and_then(Value::as_str)
                .filter(|v| matches!(*v, "chat" | "responses"))
                .ok_or_else(|| AppError::BadRequest("Invalid OpenAI compatible API type".into()))?;
            format!("openai-compatible-{api}-{}", &suffix[..12])
        }
        "anthropic-compatible" => format!("anthropic-compatible-{}", &suffix[..12]),
        "custom-embedding" => format!("custom-embedding-{}", &suffix[..12]),
        _ => return Err(AppError::BadRequest("Invalid provider node type".into())),
    };
    let mut base = body
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or(match ty.as_str() {
            "anthropic-compatible" => "https://api.anthropic.com/v1",
            _ => "https://api.openai.com/v1",
        })
        .trim()
        .trim_end_matches('/')
        .to_string();
    if ty == "anthropic-compatible" && base.ends_with("/messages") {
        base.truncate(base.len() - 9)
    }
    if ty == "custom-embedding" && base.ends_with("/embeddings") {
        base.truncate(base.len() - 11)
    }
    body["id"] = json!(id);
    body["type"] = json!(ty);
    body["name"] = json!(name);
    body["prefix"] = json!(prefix);
    body["baseUrl"] = json!(base);
    let node = state.db.create_provider_node(body)?;
    json_response(StatusCode::CREATED, json!({"node":node}))
}
fn custom_model_post(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let provider = body
        .get("providerAlias")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("providerAlias and id required".into()))?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("providerAlias and id required".into()))?;
    let ty = body.get("type").and_then(Value::as_str).unwrap_or("llm");
    let key = format!("{provider}|{id}|{ty}");
    let existed = state.db.kv_get("customModels", &key)?.is_some();
    let mut value = state
        .db
        .kv_get("customModels", &key)?
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    value.insert("providerAlias".into(), json!(provider));
    value.insert("id".into(), json!(id));
    value.insert("type".into(), json!(ty));
    if let Some(name) = body.get("name") {
        value.insert("name".into(), name.clone());
    } else if !value.contains_key("name") {
        value.insert("name".into(), json!(id));
    }
    if let Some(caps) = body.get("caps") {
        value.insert("caps".into(), caps.clone());
    }
    state
        .db
        .kv_set("customModels", &key, &Value::Object(value))?;
    json_response(StatusCode::OK, json!({"success":true,"added":!existed}))
}

fn keys_post(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Name is required".into()))?;
    let k = state.db.create_api_key(Some(name), None)?;
    json_response(
        StatusCode::CREATED,
        json!({"key":k.get("key"),"name":k.get("name"),"id":k.get("id"),"machineId":k.get("machineId")}),
    )
}

fn models_get(state: &AppState) -> Result<Response<Body>, AppError> {
    let aliases = state.db.kv_all("modelAliases")?;
    let mut out = Vec::new();
    if let Some(entries) = providers::catalog()
        .get("registry")
        .and_then(Value::as_array)
    {
        for e in entries {
            let pid = e.get("id").and_then(Value::as_str).unwrap_or("");
            let palias = e.get("alias").and_then(Value::as_str).unwrap_or(pid);
            for m in providers::models_for(pid) {
                let Some(mid) = m.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if m.get("kind").and_then(Value::as_str).unwrap_or("llm") != "llm" {
                    continue;
                }
                let full = format!("{pid}/{mid}");
                let routed = format!("{palias}/{mid}");
                let alias = aliases.get(&full).cloned().unwrap_or_else(|| json!(mid));
                out.push(json!({"provider":pid,"model":mid,"name":m.get("name").cloned().unwrap_or(json!(mid)),"fullModel":full,"routedModel":routed,"alias":alias,"caps":m.get("caps").cloned().unwrap_or(json!({}))}))
            }
        }
    }
    json_response(StatusCode::OK, json!({"models":out}))
}
fn model_alias(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let alias = body
        .get("alias")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Model and alias required".into()))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Model and alias required".into()))?;
    state.db.kv_set("modelAliases", alias, &json!(model))?;
    json_response(
        StatusCode::OK,
        json!({"success":true,"model":model,"alias":alias}),
    )
}
fn model_legacy_alias(state: &AppState, body: Value) -> Result<Response<Body>, AppError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Model and alias required".into()))?;
    let alias = body
        .get("alias")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("Model and alias required".into()))?;
    state.db.kv_set("modelAliases", model, &json!(alias))?;
    json_response(
        StatusCode::OK,
        json!({"success":true,"model":model,"alias":alias}),
    )
}
fn pricing_get(state: &AppState) -> Result<Response<Body>, AppError> {
    let p = state.db.kv_all("pricing")?;
    json_response(StatusCode::OK, json!({"pricing":p}))
}
async fn dynamic(
    state: &AppState,
    method: &Method,
    path: &str,
    body: Value,
) -> Result<Response<Body>, AppError> {
    if let Some(id) = path.strip_prefix("/api/proxy-pools/") {
        return match method.as_str() {
            "GET" => {
                let p = state
                    .db
                    .proxy_pool(id)?
                    .ok_or_else(|| AppError::NotFound("Proxy pool not found".into()))?;
                json_response(StatusCode::OK, json!({"proxyPool":p}))
            }
            "PUT" | "PATCH" => {
                let p = state.db.update_proxy_pool(id, body)?;
                json_response(StatusCode::OK, json!({"proxyPool":p}))
            }
            "DELETE" => json_response(
                StatusCode::OK,
                json!({"success":state.db.delete_proxy_pool(id)?}),
            ),
            _ => Err(AppError::NotFound(path.into())),
        };
    }
    if let Some(id) = path.strip_prefix("/api/provider-nodes/") {
        return match method.as_str() {
            "GET" => {
                let node = state
                    .db
                    .provider_node(id)?
                    .ok_or_else(|| AppError::NotFound("Provider node not found".into()))?;
                json_response(StatusCode::OK, json!({"node":node}))
            }
            "PUT" | "PATCH" => {
                let existing = state
                    .db
                    .provider_node(id)?
                    .ok_or_else(|| AppError::NotFound("Provider node not found".into()))?;
                let name = body
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| AppError::BadRequest("Name is required".into()))?;
                let prefix = body
                    .get("prefix")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| AppError::BadRequest("Prefix is required".into()))?;
                let mut base = body
                    .get("baseUrl")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| AppError::BadRequest("Base URL is required".into()))?
                    .trim_end_matches('/')
                    .to_string();
                let ty = existing.get("type").and_then(Value::as_str).unwrap_or("");
                if ty == "anthropic-compatible" && base.ends_with("/messages") {
                    base.truncate(base.len() - 9)
                }
                if ty == "custom-embedding" && base.ends_with("/embeddings") {
                    base.truncate(base.len() - 11)
                }
                let mut patch = json!({"name":name,"prefix":prefix,"baseUrl":base});
                if ty == "openai-compatible" {
                    let api = body
                        .get("apiType")
                        .and_then(Value::as_str)
                        .filter(|v| matches!(*v, "chat" | "responses"))
                        .ok_or_else(|| {
                            AppError::BadRequest("Invalid OpenAI compatible API type".into())
                        })?;
                    patch["apiType"] = json!(api);
                }
                let node = state.db.update_provider_node(id, patch)?;
                for c in state.db.provider_connections(Some(id), None)? {
                    if let Some(cid) = c.get("id").and_then(Value::as_str) {
                        let mut ps = c
                            .get("providerSpecificData")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        ps.insert(
                            "prefix".into(),
                            node.get("prefix").cloned().unwrap_or(Value::Null),
                        );
                        ps.insert(
                            "apiType".into(),
                            node.get("apiType").cloned().unwrap_or(Value::Null),
                        );
                        ps.insert(
                            "baseUrl".into(),
                            node.get("baseUrl").cloned().unwrap_or(Value::Null),
                        );
                        ps.insert(
                            "nodeName".into(),
                            node.get("name").cloned().unwrap_or(Value::Null),
                        );
                        let _ = state.db.update_connection(
                            cid,
                            json!({"providerSpecificData":Value::Object(ps)}),
                        );
                    }
                }
                json_response(StatusCode::OK, json!({"node":node}))
            }
            "DELETE" => {
                if state.db.provider_node(id)?.is_none() {
                    return Err(AppError::NotFound("Provider node not found".into()));
                }
                state.db.delete_connections_by_provider(id)?;
                state.db.delete_provider_node(id)?;
                json_response(StatusCode::OK, json!({"success":true}))
            }
            _ => Err(AppError::NotFound(path.into())),
        };
    }
    if let Some(sub) = path.strip_prefix("/api/providers/") {
        if sub.ends_with("/models") {
            let pid = sub.trim_end_matches("/models");
            return crate::providers_oauth::handle_providers_models(state, method, pid).await;
        }
        if sub.ends_with("/test")
            || sub.ends_with("/test-models")
            || sub == "test-batch"
            || sub == "validate"
            || sub == "suggested-models"
            || sub == "kilo/free-models"
        {
            return crate::providers_oauth::handle_providers_test(state, method, &body).await;
        }
        if !sub.contains('/') && !sub.is_empty() {
            let id = sub;
            return match method.as_str() {
                "GET" => {
                    let mut c = state
                        .db
                        .provider_connection(id)?
                        .ok_or_else(|| AppError::NotFound("provider connection".into()))?;
                    redact_secrets(&mut c);
                    json_response(StatusCode::OK, json!({"connection":c}))
                }
                "PATCH" | "PUT" => {
                    let mut c = state.db.update_connection(id, body)?;
                    redact_secrets(&mut c);
                    json_response(StatusCode::OK, json!({"connection":c}))
                }
                "DELETE" => json_response(
                    StatusCode::OK,
                    json!({"success":state.db.delete_connection(id)?}),
                ),
                _ => Err(AppError::NotFound(path.into())),
            };
        }
    }
    if let Some(id) = path.strip_prefix("/api/keys/") {
        if method == Method::DELETE {
            return json_response(
                StatusCode::OK,
                json!({"success":state.db.delete_api_key(id)?}),
            );
        }
    }
    if let Some(id) = path.strip_prefix("/api/combos/") {
        return match method.as_str() {
            "GET" => match state.db.combo_by_id(id)? {
                Some(c) => json_response(StatusCode::OK, c),
                None => Err(AppError::NotFound("Combo not found".into())),
            },
            "PUT" | "PATCH" => {
                if let Some(name) = body.get("name").and_then(Value::as_str) {
                    validate_combo_name(name)?;
                    ensure_combo_name_available(state, name, Some(id))?;
                }
                match state.db.update_combo(id, body)? {
                    Some(c) => json_response(StatusCode::OK, c),
                    None => Err(AppError::NotFound("Combo not found".into())),
                }
            }
            "DELETE" => json_response(
                StatusCode::OK,
                json!({"success":state.db.delete_combo(id)?}),
            ),
            _ => Err(AppError::NotFound(path.into())),
        };
    }
    if let Some(alias) = path.strip_prefix("/api/models/alias/") {
        if method == Method::DELETE {
            return json_response(
                StatusCode::OK,
                json!({"success":state.db.kv_delete("modelAliases",alias)?}),
            );
        }
    }
    if path.starts_with("/api/cli-tools/") {
        return crate::remaining_infra::handle_cli_tools(state, method, path, &body).await;
    }
    if path.starts_with("/api/auth/oidc/") || path.starts_with("/api/auth/saml/") {
        return crate::remaining_infra::handle_oidc_saml(state, method, path, &body).await;
    }
    if path.starts_with("/api/headroom")
        || path.starts_with("/api/pxpipe")
        || path.starts_with("/api/tunnel")
        || path.starts_with("/api/proxy-pools/") && path.contains("-deploy")
        || path.starts_with("/api/mcp/")
        || path.starts_with("/api/shutdown")
        || path.starts_with("/api/version/")
    {
        return crate::remaining_infra::handle_infra_lifecycle(state, method, path, &body).await;
    }
    if path.starts_with("/api/oauth/") {
        return crate::providers_oauth::handle_oauth(state, method, path, &body).await;
    }
    if path.starts_with("/api/provider-nodes/validate")
        || path.starts_with("/api/proxy-pools/") && path.ends_with("/test")
    {
        return crate::providers_oauth::handle_providers_test(state, method, &body).await;
    }
    if path == "/api/v1/messages/count_tokens" {
        return crate::inference_media::handle_count_tokens(state, method, &body).await;
    }
    if path == "/api/v1/search" {
        return crate::inference_media::handle_v1_search(state, method, &body).await;
    }
    if path == "/api/v1/web/fetch" {
        return crate::inference_media::handle_v1_web_fetch(state, method, &body).await;
    }
    if path.starts_with("/api/v1/videos") {
        return crate::inference_media::handle_v1_videos(state, method, path, &body).await;
    }
    if path == "/api/v1/api/chat" || path == "/api/v1/route.js" || path == "/api/v1" {
        return crate::inference_media::handle_v1_api_chat(state, method, &body).await;
    }
    if path.starts_with("/api/v1/models/") {
        return crate::inference_media::handle_v1_models_info(state, method, None).await;
    }
    Err(AppError::NotFound(format!(
        "Rust API route not implemented: {method} {path}"
    )))
}

fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "apikey"
                        | "api_key"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "authtoken"
                        | "sessiontoken"
                        | "accounttoken"
                        | "password"
                        | "clientsecret"
                        | "client_secret"
                        | "privatekey"
                        | "private_key"
                        | "cookie"
                        | "authorization"
                        | "token"
                )
            });
            for child in object.values_mut() {
                redact_secrets(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_secrets(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn json_response_no_store(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
    let mut response = json_response(status, value)?;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
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

const SUPPORTED_LOCALES: &[&str] = &[
    "en", "vi", "zh-CN", "zh-TW", "ja", "pt-BR", "pt-PT", "ko", "es", "de", "fr", "he", "ar", "ru",
    "pl", "cs", "nl", "tr", "uk", "tl", "id", "km", "th", "hi", "bn", "ur", "ro", "sv", "it", "el",
    "hu", "fi", "da", "no", "fa",
];

fn locale_update(body: Value) -> Result<Response<Body>, AppError> {
    let locale = body
        .get("locale")
        .and_then(Value::as_str)
        .filter(|locale| SUPPORTED_LOCALES.contains(locale))
        .ok_or_else(|| AppError::BadRequest("Invalid locale".into()))?;
    let mut response =
        json_response_no_store(StatusCode::OK, json!({"success":true,"locale":locale}))?;
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "locale={locale}; Path=/; Max-Age=31536000; SameSite=Lax"
        ))
        .map_err(|error| AppError::Internal(error.into()))?,
    );
    Ok(response)
}

#[cfg(test)]
mod locale_release_tests {
    use super::*;

    #[tokio::test]
    async fn every_supported_locale_sets_cookie_without_login() {
        assert!(is_public(&Method::POST, "/api/locale"));
        assert!(!is_public(&Method::GET, "/api/locale"));
        for locale in SUPPORTED_LOCALES {
            let response = locale_update(json!({"locale":locale})).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                response.headers()[header::SET_COOKIE].to_str().unwrap(),
                format!("locale={locale}; Path=/; Max-Age=31536000; SameSite=Lax")
            );
            let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body, json!({"success":true,"locale":locale}));
        }
    }

    #[test]
    fn invalid_locale_values_are_bad_requests() {
        for body in [
            json!({}),
            json!(null),
            json!([]),
            json!({"locale":null}),
            json!({"locale":123}),
            json!({"locale":""}),
            json!({"locale":"xx"}),
            json!({"locale":"en; auth_token=bad"}),
            json!({"locale":"en\r\nX-Test: bad"}),
        ] {
            assert!(matches!(locale_update(body), Err(AppError::BadRequest(_))));
        }
    }
}
