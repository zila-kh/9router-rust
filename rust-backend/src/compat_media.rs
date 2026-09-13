use std::{net::SocketAddr, str::FromStr};

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderValue, Method, Request, Response, StatusCode, Uri},
};

use crate::{auth, compat_proxy, error::AppError, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub fn is_path(path: &str) -> bool {
    let path = path.strip_prefix("/api").unwrap_or(path);
    path == "/v1/search"
        || path.starts_with("/v1/search/")
        || path == "/v1/web"
        || path.starts_with("/v1/web/")
        || path == "/v1/videos"
        || path.starts_with("/v1/videos/")
}

pub async fn handle(
    state: AppState,
    peer: SocketAddr,
    request: Request<Body>,
) -> Result<Response<Body>, AppError> {
    if !state.config.compat_api_enabled {
        return Err(AppError::NotFound(
            "upstream public API compatibility disabled".into(),
        ));
    }

    if request.method() == Method::OPTIONS {
        return Ok(cors_preflight());
    }

    let query_key = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "key")
            .map(|(_, value)| value.into_owned())
    });
    auth::require_llm(&state, request.headers(), peer, query_key.as_deref())?;

    let (parts, body) = request.into_parts();
    let target_uri = internal_api_uri(&parts.uri)?;
    let raw = to_bytes(body, MAX_BODY)
        .await
        .map_err(|error| AppError::BadRequest(format!("compatibility media body: {error}")))?;

    compat_proxy::proxy_buffered(
        &state,
        peer,
        &parts.method,
        &target_uri,
        &parts.headers,
        raw,
    )
    .await
}

fn internal_api_uri(uri: &Uri) -> Result<Uri, AppError> {
    let path = uri.path();
    let target_path = if path.starts_with("/api/") {
        path.to_string()
    } else {
        format!("/api{path}")
    };
    let target = match uri.query() {
        Some(query) => format!("{target_path}?{query}"),
        None => target_path,
    };
    Uri::from_str(&target)
        .map_err(|error| AppError::BadRequest(format!("invalid compatibility URI: {error}")))
}

fn cors_preflight() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_upstream_compatibility_media_routes() {
        assert!(is_path("/v1/videos/generations"));
        assert!(is_path("/api/v1/videos/job-1"));
        assert!(is_path("/v1/search"));
        assert!(is_path("/api/v1/web/fetch"));
        assert!(!is_path("/v1/videos-extra"));
        assert!(!is_path("/v1/images/generations"));
    }

    #[test]
    fn maps_public_paths_to_internal_next_api_routes() {
        let public = Uri::from_static("/v1/videos/generations?provider=xai");
        let mapped = internal_api_uri(&public).expect("mapped URI");
        assert_eq!(
            mapped.to_string(),
            "/api/v1/videos/generations?provider=xai"
        );

        let already_internal = Uri::from_static("/api/v1/search");
        let mapped = internal_api_uri(&already_internal).expect("mapped URI");
        assert_eq!(mapped.to_string(), "/api/v1/search");
    }
}
