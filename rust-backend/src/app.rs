use crate::{
    auth, compat_media, compat_proxy, error::AppError, gateway, management, media,
    state::AppState, ui_proxy,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
    Router,
};
use std::net::{IpAddr, SocketAddr};

const MAX_API_BODY: usize = 128 * 1024 * 1024;
const UPSTREAM_COMMIT: &str = "17c4cc76877bd1755030a8414f8d0083f48dcccf";
const UPSTREAM_VERSION: &str = "0.5.75";

const ALWAYS_PROTECTED_PREFIXES: &[&str] = &[
    "/api/shutdown",
    "/api/settings/database",
    "/api/version/shutdown",
    "/api/version/update",
    "/api/oauth/cursor/auto-import",
    "/api/oauth/kiro/auto-import",
];

const LOCAL_ONLY_PREFIXES: &[&str] = &[
    "/api/cli-tools/cowork-settings",
    "/api/cli-tools/antigravity-mitm",
    "/api/mcp/",
    "/api/tunnel/tailscale-install",
    "/api/tunnel/tailscale-enable",
    "/api/tunnel/tailscale-disable",
    "/api/tunnel/tailscale-check",
    "/api/tunnel/enable",
    "/api/tunnel/disable",
    "/api/oauth/cursor/auto-import",
    "/api/oauth/kiro/auto-import",
    "/api/auth/reset-password",
    "/api/headroom/start",
    "/api/headroom/stop",
    "/api/headroom/proxy",
];

const FORWARDED_PEER_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "x-client-ip",
    "x-cluster-client-ip",
    "x-9r-via-proxy",
];

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
    let result: Result<Response<Body>, AppError> = if state.config.compat_api_enabled
        && compat_media::is_path(&path)
    {
        compat_media::handle(state, peer, req).await
    } else if media::is_media_path(&path) {
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

    if !state.config.compat_api_enabled && method == Method::GET {
        if let Some(response) = strict_metadata_response(&path) {
            return Ok(response);
        }
    }

    // Compatibility mode deliberately prefers the pinned upstream route handlers
    // for dashboard APIs. This restores exact response shapes and newly added
    // endpoints while the native Rust implementations continue to mature. The
    // small security/auth allow-list remains Rust-owned in every mode.
    if !state.config.compat_api_enabled || native_in_compat_mode(&method, &path) {
        return management::handle(state, ConnectInfo(peer), request).await;
    }

    let (parts, body) = request.into_parts();
    let has_cli_token = auth::has_valid_cli_token(&state, &parts.headers);

    if is_local_only_path(&path)
        && !has_cli_token
        && (!is_safe_local_request(peer, &parts.headers)
            || !auth::dashboard_authenticated(&state, &parts.headers)?)
    {
        return Err(AppError::Forbidden(
            "Local only: CLI token or authenticated loopback request required".into(),
        ));
    }

    if is_always_protected_path(&path)
        && !has_cli_token
        && !auth::has_valid_dashboard_session(&state, &parts.headers)
    {
        return Err(AppError::Unauthorized);
    }

    if !public_compat_path(&path) && !has_cli_token {
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
    matches!(path, "/api/init" | "/api/version" | "/api/locale")
        || path == "/api/auth/oidc"
        || path.starts_with("/api/auth/oidc/")
        || path == "/api/auth/saml"
        || path.starts_with("/api/auth/saml/")
}

fn is_always_protected_path(path: &str) -> bool {
    ALWAYS_PROTECTED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn is_local_only_path(path: &str) -> bool {
    LOCAL_ONLY_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn is_safe_local_request(peer: SocketAddr, headers: &HeaderMap) -> bool {
    if !auth::is_loopback(peer)
        || FORWARDED_PEER_HEADERS
            .iter()
            .any(|name| headers.contains_key(*name))
    {
        return false;
    }

    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let Some(host) = origin.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(auth::is_loopback_ip)
        .unwrap_or(false)
}

fn strict_metadata_response(path: &str) -> Option<Response<Body>> {
    let body = match path {
        "/api/init" => format!(
            "{{\"initialized\":true,\"runtime\":\"rust\",\"version\":\"{}\",\"upstreamVersion\":\"{UPSTREAM_VERSION}\",\"upstreamSnapshot\":\"{UPSTREAM_COMMIT}\"}}",
            env!("CARGO_PKG_VERSION")
        ),
        "/api/version" => format!(
            "{{\"version\":\"{}\",\"name\":\"9router-rust\",\"rustBackend\":true,\"upstreamVersion\":\"{UPSTREAM_VERSION}\",\"upstreamSnapshot\":\"{UPSTREAM_COMMIT}\"}}",
            env!("CARGO_PKG_VERSION")
        ),
        _ => return None,
    };
    Some(json_ok(body))
}

fn health_response() -> Response<Body> {
    let body = format!(
        "{{\"ok\":true,\"status\":\"ok\",\"runtime\":\"rust\",\"version\":\"{}\",\"upstreamVersion\":\"{UPSTREAM_VERSION}\",\"upstreamSnapshot\":\"{UPSTREAM_COMMIT}\"}}",
        env!("CARGO_PKG_VERSION")
    );
    let mut response = json_ok(body);
    add_health_cors(response.headers_mut());
    response
}

fn json_ok(body: String) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn health_options() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    add_health_cors(response.headers_mut());
    response
}

fn add_health_cors(headers: &mut HeaderMap) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(value: &str) -> SocketAddr {
        value.parse().expect("valid test socket address")
    }

    #[test]
    fn compatibility_public_paths_match_upstream_allowlist() {
        assert!(public_compat_path("/api/init"));
        assert!(public_compat_path("/api/locale"));
        assert!(public_compat_path("/api/auth/oidc/callback"));
        assert!(public_compat_path("/api/auth/saml/metadata"));
        assert!(!public_compat_path("/api/tags"));
        assert!(!public_compat_path("/api/auth/oidc-extra"));
    }

    #[test]
    fn sensitive_route_classes_are_preserved() {
        assert!(is_always_protected_path("/api/settings/database"));
        assert!(is_always_protected_path("/api/version/update/check"));
        assert!(is_local_only_path("/api/headroom/start"));
        assert!(is_local_only_path("/api/mcp/tools"));
        assert!(!is_local_only_path("/api/providers"));
    }

    #[test]
    fn local_only_gate_rejects_remote_or_forwarded_requests() {
        let headers = HeaderMap::new();
        assert!(is_safe_local_request(peer("127.0.0.1:1234"), &headers));
        assert!(!is_safe_local_request(peer("192.0.2.20:1234"), &headers));

        let mut forwarded = HeaderMap::new();
        forwarded.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.20"));
        assert!(!is_safe_local_request(peer("127.0.0.1:1234"), &forwarded));
    }

    #[test]
    fn local_only_gate_validates_browser_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:20128"),
        );
        assert!(is_safe_local_request(peer("127.0.0.1:1234"), &headers));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://router.example.com"),
        );
        assert!(!is_safe_local_request(peer("127.0.0.1:1234"), &headers));
    }

    #[test]
    fn strict_metadata_uses_current_upstream_snapshot() {
        let init = strict_metadata_response("/api/init").expect("init metadata");
        let version = strict_metadata_response("/api/version").expect("version metadata");
        assert_eq!(init.status(), StatusCode::OK);
        assert_eq!(version.status(), StatusCode::OK);
        assert!(strict_metadata_response("/api/providers").is_none());
    }
}
