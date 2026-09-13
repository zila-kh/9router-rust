use crate::{
    auth, compat_proxy, error::AppError, gateway, management, media, state::AppState, ui_proxy,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
    Router,
};
use std::net::SocketAddr;

const MAX_API_BODY: usize = 128 * 1024 * 1024;
const UPSTREAM_COMMIT: &str = "17c4cc76877bd1755030a8414f8d0083f48dcccf";

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
                response
                    .headers_mut()
                    .insert("x-9router-runtime", HeaderValue::from_static("rust"));
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

    if path == "/api/health" {
        return match method {
            Method::GET => Ok(health_response()),
            Method::OPTIONS => Ok(health_options()),
            _ => management::handle(state, ConnectInfo(peer), request).await,
        };
    }

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
        ("GET", "/api/rust/parity")
            | ("GET", "/api/settings/require-login")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("POST", "/api/auth/reset-password")
    )
}

fn public_compat_path(path: &str) -> bool {
    matches!(path, "/api/init" | "/api/version" | "/api/tags")
        || path.starts_with("/api/auth/oidc")
        || path.starts_with("/api/auth/saml")
}

fn health_response() -> Response<Body> {
    let body = format!(
        "{{\"ok\":true,\"status\":\"ok\",\"runtime\":\"rust\",\"version\":\"1.0.1\",\"upstreamVersion\":\"0.5.75\",\"upstreamSnapshot\":\"{UPSTREAM_COMMIT}\"}}"
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    add_health_cors(response.headers_mut());
    response
}

fn health_options() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    add_health_cors(response.headers_mut());
    response
}

fn add_health_cors(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}
