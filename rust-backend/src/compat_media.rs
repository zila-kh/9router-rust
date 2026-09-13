use std::{net::SocketAddr, str::FromStr};

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderValue, Method, Request, Response, StatusCode, Uri},
};

use crate::{auth, compat_proxy, error::AppError, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub fn is_path(path: &str) -> bool {
    let without_api_prefix = path.strip_prefix("/api").unwrap_or(path);
    if without_api_prefix == "/v1/v1" || without_api_prefix.starts_with("/v1/v1/") {
        return true;
    }

    let path = normalize_public_path(path);
    matches!(
        path.as_str(),
        "/v1"
            | "/v1/api/chat"
            | "/v1/audio/voices"
            | "/v1/messages/count_tokens"
            | "/v1/models"
            | "/v1/responses/compact"
            | "/v1/search"
            | "/v1/web"
            | "/v1/videos"
            | "/v1beta/models"
    ) || path.starts_with("/v1/models/")
        || path.starts_with("/v1/search/")
        || path.starts_with("/v1/web/")
        || path.starts_with("/v1/videos/")
        || path.starts_with("/v1beta/models/")
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
        .map_err(|error| AppError::BadRequest(format!("compatibility API body: {error}")))?;

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

fn normalize_public_path(path: &str) -> String {
    let path = path.strip_prefix("/api").unwrap_or(path);
    if path == "/v1/v1" {
        "/v1".to_string()
    } else if let Some(rest) = path.strip_prefix("/v1/v1/") {
        format!("/v1/{rest}")
    } else {
        path.to_string()
    }
}

fn internal_api_uri(uri: &Uri) -> Result<Uri, AppError> {
    let target_path = format!("/api{}", normalize_public_path(uri.path()));
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
    fn recognizes_all_non_native_public_compatibility_routes() {
        for path in [
            "/v1",
            "/api/v1",
            "/v1/v1",
            "/v1/v1/audio/speech",
            "/api/v1/v1/images/generations",
            "/v1/api/chat",
            "/v1/audio/voices",
            "/v1/messages/count_tokens",
            "/v1/models",
            "/v1/models/info",
            "/v1/models/image",
            "/v1/responses/compact",
            "/v1/search",
            "/api/v1/web/fetch",
            "/v1/videos/generations",
            "/v1beta/models",
            "/api/v1beta/models/gemini-2.5-flash:generateContent",
        ] {
            assert!(is_path(path), "expected compatibility route: {path}");
        }
    }

    #[test]
    fn leaves_native_public_routes_in_rust() {
        for path in [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/responses",
            "/v1/embeddings",
            "/v1/audio/speech",
            "/v1/audio/transcriptions",
            "/v1/images/generations",
            "/v1/videos-extra",
        ] {
            assert!(!is_path(path), "expected native Rust route: {path}");
        }
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

        let double_prefix = Uri::from_static("/v1/v1/models/image?active=true");
        let mapped = internal_api_uri(&double_prefix).expect("mapped URI");
        assert_eq!(mapped.to_string(), "/api/v1/models/image?active=true");
    }
}
