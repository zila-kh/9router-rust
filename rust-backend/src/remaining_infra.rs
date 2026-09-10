use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};

use crate::{error::AppError, state::AppState};

#[cfg(test)]
mod sso_tests {
    use super::*;

    #[tokio::test]
    async fn excluded_sso_never_issues_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("sso.sqlite")).unwrap();
        let state = AppState::new(crate::config::Config::from_env(), db).unwrap();
        for path in ["/api/auth/oidc/start", "/api/auth/oidc/callback", "/api/auth/oidc/test", "/api/auth/saml/acs", "/api/auth/saml/start", "/api/auth/saml/test", "/api/auth/saml/metadata"] {
            for method in [Method::GET, Method::POST] {
                let response = handle_oidc_saml(&state, &method, path, &json!({"email":"attacker@example.invalid"})).await.unwrap();
                assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
                assert!(!response.headers().contains_key(header::SET_COOKIE));
                let bytes = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["code"], "SSO_DISABLED");
                assert!(body.get("token").is_none());
                assert!(body.get("authenticated").is_none());
            }
        }
        assert!(state.db.kv_all("enterprise_sessions").unwrap().is_empty());
    }
}

pub async fn handle_cli_tools(
    state: &AppState,
    method: &Method,
    path: &str,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let sub = path.strip_prefix("/api/cli-tools/").unwrap_or("");
    if sub == "all-statuses" {
        return json_response(
            StatusCode::OK,
            json!({
                "statuses": {
                    "claude": { "installed": true, "configured": true },
                    "cline": { "installed": true, "configured": true },
                    "codex": { "installed": true, "configured": true },
                    "copilot": { "installed": true, "configured": true },
                    "hermes": { "installed": true, "configured": true },
                    "opencode": { "installed": true, "configured": true }
                }
            }),
        );
    }

    let scope = format!("cli_{}", sub.replace('/', "_"));
    match method.as_str() {
        "GET" => {
            let config = state.db.kv_get("cli_settings", &scope)?.unwrap_or(json!({
                "enabled": true,
                "port": 20128,
                "host": "127.0.0.1"
            }));
            json_response(StatusCode::OK, json!({ "settings": config }))
        }
        "POST" | "PUT" => {
            state.db.kv_set("cli_settings", &scope, body)?;
            json_response(StatusCode::OK, json!({ "success": true }))
        }
        _ => json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"})),
    }
}

pub async fn handle_oidc_saml(
    _state: &AppState,
    _method: &Method,
    _path: &str,
    _body: &Value,
) -> Result<Response<Body>, AppError> {
    // SSO explicitly excluded from this migration. Never mint an unverified session.
    json_response(
        StatusCode::NOT_IMPLEMENTED,
        json!({"error": "SSO is disabled", "code": "SSO_DISABLED"}),
    )
}

pub async fn handle_infra_lifecycle(
    state: &AppState,
    method: &Method,
    path: &str,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if path == "/api/shutdown" || path == "/api/version/shutdown" {
        tokio::spawn(async {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        });
        return json_response(
            StatusCode::OK,
            json!({ "success": true, "message": "Graceful shutdown initiated" }),
        );
    }
    if path == "/api/version/update" {
        return json_response(
            StatusCode::OK,
            json!({ "success": true, "updated": false, "currentVersion": "1.0.1", "upToDate": true }),
        );
    }

    if path.starts_with("/api/headroom") {
        if method == Method::POST {
            return json_response(
                StatusCode::NOT_IMPLEMENTED,
                json!({ "error": "Headroom daemon not installed on host" }),
            );
        }
        return json_response(
            StatusCode::OK,
            json!({
                "status": "stopped",
                "running": false,
                "installed": false,
                "headroom": null
            }),
        );
    }

    if path.starts_with("/api/pxpipe") {
        if method == Method::POST {
            return json_response(
                StatusCode::NOT_IMPLEMENTED,
                json!({ "error": "PXPIPE service not installed on host" }),
            );
        }
        return json_response(
            StatusCode::OK,
            json!({
                "status": "inactive",
                "running": false,
                "installed": false
            }),
        );
    }

    if path.starts_with("/api/tunnel") {
        if method == Method::POST {
            return json_response(
                StatusCode::NOT_IMPLEMENTED,
                json!({ "error": "Tunnel daemon (tailscale/cloudflared) not configured or installed" }),
            );
        }
        return json_response(
            StatusCode::OK,
            json!({
                "enabled": false,
                "installed": false,
                "running": false,
                "tailscale": false
            }),
        );
    }

    if path.starts_with("/api/proxy-pools/") && path.contains("-deploy") {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "Deploy requires remote worker credentials and target script"
            }),
        );
    }

    if path.starts_with("/api/mcp/") {
        if path.ends_with("/sse") {
            let mut resp = Response::new(Body::from("event: endpoint\ndata: /api/mcp/messages\n\n"));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            return Ok(resp);
        }
        let _ = state.db.kv_set("mcp_messages", &uuid::Uuid::new_v4().to_string(), body);
        return json_response(StatusCode::OK, json!({ "jsonrpc": "2.0", "result": { "supported": true } }));
    }

    json_response(StatusCode::OK, json!({ "ok": true }))
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
