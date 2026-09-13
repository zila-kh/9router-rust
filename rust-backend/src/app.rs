use crate::{
    auth, compat_proxy, error::AppError, gateway, management, media, state::AppState, ui_proxy,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{Method, Request, Response},
    response::IntoResponse,
    Router,
};
use std::net::SocketAddr;

const MAX_API_BODY: usize = 128 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new().fallback(entry).with_state(state)
}

async fn entry(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    let is_backend =
        media::is_media_path(&path) || gateway::is_llm_path(&path) || path.starts_with("/api/");
    let result: Result<Response<Body>, AppError> = if media::is_media_path(&path) {
        media::handle(state, ConnectInfo(peer), req).await
    } else if gateway::is_llm_path(&path) {
        gateway::handle(state, ConnectInfo(peer), req).await
    } else if path.starts_with("/api/") {
        handle_management(state, peer, req).await
    } else {
        ui_proxy::handle(state, ConnectInfo(peer), req).await
    };
    match result {
        Ok(mut response) => {
            if is_backend && !response.headers().contains_key("x-9router-runtime") {
                response.headers_mut().insert(
                    "x-9router-runtime",
                    axum::http::HeaderValue::from_static("rust"),
                );
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

async fn handle_management(
    state: AppState,
    peer: SocketAddr,
    request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // Compatibility mode deliberately prefers the pinned upstream route handlers
    // for dashboard APIs. This restores exact response shapes and newly added
    // endpoints while the native Rust implementations continue to mature. The
    // small security/auth allow-list remains Rust-owned in every mode.
    if !state.config.compat_api_enabled || native_in_compat_mode(&method, &path) {
        return management::handle(state, ConnectInfo(peer), request).await;
    }

    let (parts, body) = request.into_parts();
    if !public_compat_path(&path) {
        auth::require_dashboard(&state, &parts.headers)?;
    }

    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let raw = to_bytes(body, MAX_API_BODY)
        .await
        .map_err(|error| AppError::BadRequest(format!("API request body: {error}")))?;

    compat_proxy::proxy_buffered(&state, peer, &method, &uri, &headers, raw).await
}

fn native_in_compat_mode(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/health")
            | ("GET", "/api/rust/parity")
            | ("GET", "/api/settings/require-login")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("POST", "/api/auth/reset-password")
    )
}

fn public_compat_path(path: &str) -> bool {
    matches!(
        path,
        "/api/health" | "/api/init" | "/api/version" | "/api/tags"
    ) || path.starts_with("/api/auth/oidc")
        || path.starts_with("/api/auth/saml")
}
