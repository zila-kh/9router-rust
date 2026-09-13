use crate::{
    compat_proxy, error::AppError, gateway, management, media, state::AppState, ui_proxy,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, Method, Request, Response},
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
    if !state.config.compat_api_enabled {
        return management::handle(state, ConnectInfo(peer), request).await;
    }

    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let raw = to_bytes(body, MAX_API_BODY)
        .await
        .map_err(|error| AppError::BadRequest(format!("API request body: {error}")))?;
    let native_request = Request::from_parts(parts, Body::from(raw.clone()));

    match management::handle(state.clone(), ConnectInfo(peer), native_request).await {
        Err(AppError::NotFound(_)) => {
            compat_proxy::proxy_buffered(&state, peer, &method, &uri, &headers, raw).await
        }
        Err(AppError::BadRequest(_)) if should_retry_non_json(&method, &headers) => {
            compat_proxy::proxy_buffered(&state, peer, &method, &uri, &headers, raw).await
        }
        other => other,
    }
}

fn should_retry_non_json(method: &Method, headers: &HeaderMap) -> bool {
    if method != Method::POST && method != Method::PUT && method != Method::PATCH {
        return false;
    }
    !headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().starts_with("application/json"))
        .unwrap_or(false)
}
