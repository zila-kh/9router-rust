use crate::{
    auth, compat_media, compat_proxy, error::AppError, gateway, management, media, state::AppState,
    ui_proxy,
};
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
    Router,
};
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;

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
    "/api/oauth/xiaomi-mimo/auto-import",
];

const LOCAL_ONLY_PREFIXES: &[&str] = &[
    "/api/cli-tools/",
    "/api/mcp/",
    "/api/tunnel/",
    "/api/oauth/cursor/auto-import",
    "/api/oauth/kiro/auto-import",
    "/api/oauth/xiaomi-mimo/auto-import",
    "/api/auth/reset-password",
    "/api/headroom/",
    "/api/pxpipe/",
    "/api/shutdown",
    "/api/version/shutdown",
    "/api/version/update",
];

const LOCAL_ONLY_OAUTH_ACTIONS: &[&str] = &[
    "ide-status",
    "manual-code",
    "poll-status",
    "register-session",
    "start-proxy",
    "stop-proxy",
];

pub fn router(state: AppState) -> Router {
    Router::new()
        .fallback(entry)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn entry(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    let is_compat_media = state.config.compat_api_enabled && compat_media::is_path(&path);
    let is_backend = is_compat_media
        || media::is_media_path(&path)
        || gateway::is_llm_path(&path)
        || path.starts_with("/api/");
    let result: Result<Response<Body>, AppError> = if is_compat_media {
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
        Err(error) => {
            let mut response = error.into_response();
            if is_backend && !response.headers().contains_key("x-9router-runtime") {
                response
                    .headers_mut()
                    .insert("x-9router-runtime", HeaderValue::from_static("rust"));
            }
            response
        }
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

    let has_cli_token = auth::has_valid_cli_token(&state, request.headers());

    if method == Method::POST
        && path == "/api/auth/login"
        && !auth::is_direct_loopback_request(peer, request.headers())
    {
        let settings = state.db.settings()?;
        let has_stored_password = settings
            .get("password")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let has_initial_password = std::env::var("INITIAL_PASSWORD")
            .ok()
            .map(|value| value.trim().to_string())
            .is_some_and(|value| !value.is_empty());
        if !has_stored_password && !has_initial_password {
            return Err(AppError::Forbidden(
                "Default password must be changed locally before remote login".into(),
            ));
        }
    }

    if is_local_only_path(&path)
        && !has_cli_token
        && (!auth::is_direct_loopback_request(peer, request.headers())
            || !auth::dashboard_authenticated(&state, request.headers())?)
    {
        return Err(AppError::Forbidden(
            "Local only: CLI token or authenticated direct-loopback request required".into(),
        ));
    }

    if is_always_protected_path(&path)
        && !has_cli_token
        && !auth::has_valid_dashboard_session(&state, request.headers())
    {
        return Err(AppError::Unauthorized);
    }

    if !is_public_management_request(&method, &path) && !has_cli_token {
        auth::require_dashboard(&state, request.headers())?;
    }

    // Compatibility mode deliberately prefers the pinned upstream route handlers
    // for dashboard APIs. This restores exact response shapes and newly added
    // endpoints while the native Rust implementations continue to mature. The
    // security/auth policy above remains Rust-owned in every mode.
    if !state.config.compat_api_enabled || native_in_compat_mode(&method, &path) {
        return management::handle(state, ConnectInfo(peer), request).await;
    }

    let (parts, body) = request.into_parts();
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

fn is_public_management_request(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/init")
            | ("GET", "/api/version")
            | ("POST", "/api/locale")
            | ("GET", "/api/settings/require-login")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("GET", "/api/auth/oidc/start")
            | ("GET", "/api/auth/oidc/callback")
            | ("GET", "/api/auth/saml/start")
            | ("POST", "/api/auth/saml/acs")
            | ("GET", "/api/auth/saml/metadata")
    )
}

fn is_always_protected_path(path: &str) -> bool {
    ALWAYS_PROTECTED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn is_local_oauth_action(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let Some(rest) = path.strip_prefix("/api/oauth/") else {
        return false;
    };
    let mut segments = rest.split('/');
    let (Some(provider), Some(action), None) = (segments.next(), segments.next(), segments.next())
    else {
        return false;
    };
    if provider.is_empty() || action.is_empty() {
        return false;
    }

    LOCAL_ONLY_OAUTH_ACTIONS.contains(&action)
        || (provider == "xiaomi-mimo" && matches!(action, "authorize" | "exchange"))
}

fn is_local_only_path(path: &str) -> bool {
    LOCAL_ONLY_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || is_local_oauth_action(path)
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

    #[test]
    fn public_management_paths_are_exact() {
        assert!(is_public_management_request(
            &Method::GET,
            "/api/auth/oidc/start"
        ));
        assert!(is_public_management_request(
            &Method::POST,
            "/api/auth/saml/acs"
        ));
        assert!(is_public_management_request(&Method::POST, "/api/locale"));
        assert!(!is_public_management_request(&Method::GET, "/api/locale"));
        assert!(!is_public_management_request(
            &Method::POST,
            "/api/auth/oidc/test"
        ));
        assert!(!is_public_management_request(
            &Method::POST,
            "/api/auth/saml/test"
        ));
        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/oidc-extra"
        ));
        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/login"
        ));
        assert!(!is_public_management_request(
            &Method::POST,
            "/api/auth/oidc/start"
        ));
        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/saml/acs"
        ));
    }

    #[test]
    fn sensitive_route_classes_are_preserved() {
        assert!(is_always_protected_path("/api/settings/database"));
        assert!(is_always_protected_path("/api/version/update/check"));
        assert!(is_always_protected_path(
            "/api/oauth/xiaomi-mimo/auto-import"
        ));
        assert!(is_local_only_path("/api/headroom/start"));
        assert!(is_local_only_path("/api/headroom/status"));
        assert!(is_local_only_path("/api/pxpipe/logs"));
        assert!(is_local_only_path("/api/cli-tools/codex-settings"));
        assert!(is_local_only_path("/api/mcp/tools"));
        assert!(is_local_only_path("/api/oauth/xiaomi-mimo/auto-import"));
        assert!(!is_local_only_path("/api/providers"));
    }

    #[test]
    fn local_oauth_host_actions_are_exact_and_trailing_slash_safe() {
        for path in [
            "/api/oauth/codex/start-proxy",
            "/api/oauth/codex/start-proxy/",
            "/api/oauth/xai/manual-code",
            "/api/oauth/trae/register-session",
            "/api/oauth/windsurf/poll-status",
            "/api/oauth/zed/ide-status",
            "/api/oauth/xiaomi-mimo/authorize",
            "/api/oauth/xiaomi-mimo/exchange",
        ] {
            assert!(is_local_only_path(path), "{path}");
        }
        for path in [
            "/api/oauth/github/device-code",
            "/api/oauth/github/poll",
            "/api/oauth/codex/exchange",
            "/api/oauth/xiaomi-mimo/api-key",
            "/api/oauth/codex/start-proxy/extra",
        ] {
            assert!(!is_local_only_path(path), "{path}");
        }
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
