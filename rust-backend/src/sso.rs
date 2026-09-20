//! SSO route dispatcher and the request-origin/cookie helpers shared by the
//! native OIDC and SAML endpoints.

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
};
use serde_json::{json, Value};

use crate::{auth, error::AppError, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub fn is_sso_path(path: &str) -> bool {
    path.starts_with("/api/auth/oidc/") || path.starts_with("/api/auth/saml/")
}

pub async fn handle(
    state: AppState,
    peer: std::net::SocketAddr,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let path = uri.path().to_string();

    let raw = to_bytes(body, MAX_BODY)
        .await
        .map_err(|error| AppError::BadRequest(format!("SSO request body: {error}")))?;

    let result = match (method.as_str(), path.as_str()) {
        ("GET", "/api/auth/oidc/start") => {
            crate::sso_oidc::handle_start(&state, &headers, &uri).await
        }
        ("GET", "/api/auth/oidc/callback") => {
            crate::sso_oidc::handle_callback(&state, &headers, &uri).await
        }
        ("POST", "/api/auth/oidc/test") => {
            let body = parse_json_body(&raw);
            crate::sso_oidc::handle_test(&state, &headers, &uri, &body).await
        }
        ("GET", "/api/auth/saml/start") => {
            crate::sso_saml::handle_start(&state, &headers, &uri).await
        }
        ("POST", "/api/auth/saml/acs") => {
            crate::sso_saml::handle_acs(&state, peer, &headers, &uri, &raw).await
        }
        ("GET", "/api/auth/saml/metadata") => {
            crate::sso_saml::handle_metadata(&state, &headers, &uri).await
        }
        ("POST", "/api/auth/saml/test") => {
            let body = parse_json_body(&raw);
            crate::sso_saml::handle_test(&state, &headers, &uri, &body).await
        }
        _ if path.starts_with("/api/auth/oidc") || path.starts_with("/api/auth/saml") => {
            return method_not_allowed();
        }
        _ => return Err(AppError::NotFound(path)),
    };
    result
}

fn parse_json_body(raw: &[u8]) -> Value {
    if raw.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(raw).unwrap_or(json!({}))
    }
}

fn method_not_allowed() -> Result<Response<Body>, AppError> {
    let mut response = Response::new(Body::from(
        serde_json::to_vec(&json!({"error": "Method Not Allowed"})).unwrap_or_default(),
    ));
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

/// Public origin per upstream `getPublicOrigin`: `BASE_URL` /
/// `NEXT_PUBLIC_BASE_URL` win, else the forwarded host/proto chain, else the
/// request's own authority.
pub fn public_origin(headers: &HeaderMap, uri: &axum::http::Uri) -> String {
    if let Some(configured) = configured_base_url() {
        return configured;
    }
    origin_from_headers(headers, uri)
}

/// Origin per upstream SAML routes' `new URL(request.url).origin`: the
/// request's own URL origin with no env override.
pub fn request_origin(headers: &HeaderMap, uri: &axum::http::Uri) -> String {
    origin_from_headers(headers, uri)
}

/// Origin per upstream `getSamlBaseUrl`: `settings.baseUrl`, then the
/// `BASE_URL` env vars, then the forwarded chain, then a localhost default.
pub fn saml_base_url(headers: &HeaderMap, uri: &axum::http::Uri, settings: &Value) -> String {
    if let Some(configured) = settings
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return trim_trailing_slashes(configured);
    }
    if let Some(configured) = configured_base_url() {
        return configured;
    }
    origin_from_headers(headers, uri)
        .trim_end_matches('/')
        .to_string()
}

fn configured_base_url() -> Option<String> {
    for name in ["BASE_URL", "NEXT_PUBLIC_BASE_URL"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trim_trailing_slashes(trimmed));
            }
        }
    }
    None
}

fn origin_from_headers(headers: &HeaderMap, uri: &axum::http::Uri) -> String {
    let forwarded_proto = header_str(headers, "x-forwarded-proto");
    let host = header_str(headers, "x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .filter(|value| !value.is_empty());
    if let Some(host) = host {
        let protocol = forwarded_proto
            .unwrap_or_else(|| "http".to_string())
            .trim()
            .trim_end_matches(':')
            .to_string();
        return format!("{protocol}://{host}")
            .trim_end_matches('/')
            .to_string();
    }
    if let Some(authority) = uri.authority() {
        let scheme = forwarded_proto
            .unwrap_or_else(|| "http".to_string())
            .trim()
            .to_string();
        return trim_trailing_slashes(&format!("{scheme}://{authority}"));
    }
    trim_trailing_slashes("http://127.0.0.1:20128")
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn trim_trailing_slashes(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

/// Percent-encoding matching JavaScript `encodeURIComponent`.
pub fn urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

pub fn redirect_response(location: &str, set_cookies: Vec<HeaderValue>) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    for cookie in set_cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cookie_suffix(headers: &HeaderMap) -> &'static str {
    if auth::secure_cookie(headers) {
        "; Secure"
    } else {
        ""
    }
}

/// SSO flow cookie in the upstream shape (`httpOnly`, `sameSite: lax`,
/// `path: /`, short max-age).
pub fn set_cookie_header(
    headers: &HeaderMap,
    name: &str,
    value: &str,
    max_age: i64,
) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        cookie_suffix(headers)
    ))
    .expect("SSO cookie contains only valid header characters")
}

pub fn clear_cookie_header(headers: &HeaderMap, name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        cookie_suffix(headers)
    ))
    .expect("SSO cookie clear contains only valid header characters")
}

pub fn method_is(method: &Method, expected: &str) -> bool {
    method.as_str().eq_ignore_ascii_case(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn make_header(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(value) {
            if let Ok(n) = axum::http::HeaderName::try_from(name) {
                h.insert(n, v);
            }
        }
        h
    }

    #[test]
    fn request_origin_uses_host_header() {
        let headers = make_header("host", "192.168.1.50:20128");
        let uri: axum::http::Uri = "/api/test?foo=bar".parse().unwrap();
        assert_eq!(request_origin(&headers, &uri), "http://192.168.1.50:20128");
    }

    #[test]
    fn request_origin_uses_forwarded_proto_https() {
        let mut headers = make_header("host", "app.example.com");
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let uri: axum::http::Uri = "/test".parse().unwrap();
        assert_eq!(request_origin(&headers, &uri), "https://app.example.com");
    }

    #[test]
    fn request_origin_uses_forwarded_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("cdn.example.com"),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let uri: axum::http::Uri = "/test".parse().unwrap();
        assert_eq!(request_origin(&headers, &uri), "https://cdn.example.com");
    }

    #[test]
    fn request_origin_falls_back_to_localhost() {
        let headers = HeaderMap::new();
        let uri: axum::http::Uri = "/test".parse().unwrap();
        assert_eq!(request_origin(&headers, &uri), "http://127.0.0.1:20128");
    }

    #[test]
    fn saml_base_url_prefers_settings_base_url() {
        let headers = HeaderMap::new();
        let uri: axum::http::Uri = "/test".parse().unwrap();
        let settings = json!({"baseUrl": "https://custom.example.com"});
        assert_eq!(
            saml_base_url(&headers, &uri, &settings),
            "https://custom.example.com"
        );
    }

    #[test]
    fn public_origin_respects_base_url_env() {
        std::env::set_var("BASE_URL", "https://env.example.com");
        let headers = HeaderMap::new();
        let uri: axum::http::Uri = "/test".parse().unwrap();
        let result = public_origin(&headers, &uri);
        assert_eq!(result, "https://env.example.com");
        std::env::remove_var("BASE_URL");
    }

    #[test]
    fn method_not_allowed_returns_405() {
        let response = method_not_allowed().unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn redirect_response_returns_307() {
        let response = redirect_response("https://example.com", vec![]);
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers().get("location").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn is_sso_path_matches_correctly() {
        assert!(is_sso_path("/api/auth/oidc/start"));
        assert!(is_sso_path("/api/auth/oidc/callback"));
        assert!(is_sso_path("/api/auth/saml/acs"));
        assert!(!is_sso_path("/api/auth/login"));
        assert!(!is_sso_path("/api/settings"));
        assert!(!is_sso_path("/api/auth/oidc"));
    }

    #[tokio::test]
    async fn sso_metadata_returns_200_xml_through_router() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.update_settings(serde_json::json!({"samlEntryPoint":"https://idp.example.com/sso","samlCert":"MIIC1234"}))
            .unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: dir.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let request = axum::http::Request::builder()
            .uri("/api/auth/saml/metadata")
            .header("host", "localhost:20128")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = crate::app::router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/xml"
        );
        assert_eq!(
            response
                .headers()
                .get("x-9router-runtime")
                .unwrap()
                .to_str()
                .unwrap(),
            "rust"
        );
    }

    #[tokio::test]
    async fn saml_metadata_direct() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.update_settings(serde_json::json!({"samlEntryPoint":"https://idp.example.com/sso","samlCert":"MIIC1234"}))
            .unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: dir.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("localhost:20128"),
        );
        let uri: axum::http::Uri = "/api/auth/saml/metadata".parse().unwrap();
        let result = crate::sso_saml::handle_metadata(&state, &headers, &uri).await;
        match result {
            Ok(response) => {
                assert_eq!(response.status(), StatusCode::OK);
                println!("Direct handler OK");
            }
            Err(e) => {
                panic!("Direct handler error: {e}");
            }
        }
    }
}
