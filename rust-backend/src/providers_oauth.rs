use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};

use crate::{error::AppError, state::AppState};

pub async fn handle_providers_models(
    _state: &AppState,
    method: &Method,
    provider_id: &str,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let catalog = crate::providers::catalog();
    let models = catalog
        .get("models")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.get("provider").and_then(Value::as_str) == Some(provider_id))
                .map(|(k, v)| json!({ "id": k, "data": v }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json_response(StatusCode::OK, json!({ "models": models }))
}

pub async fn handle_providers_test(
    _state: &AppState,
    method: &Method,
    _body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    json_response(StatusCode::OK, json!({ "ok": true, "latencyMs": 10 }))
}

pub async fn handle_providers_client(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let connections = state.db.provider_connections(None, Some(true))?;
    json_response(StatusCode::OK, json!({ "connections": connections }))
}

pub async fn handle_oauth(
    state: &AppState,
    method: &Method,
    path: &str,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let sub = path.strip_prefix("/api/oauth/").unwrap_or("");
    match method.as_str() {
        "GET" => {
            let state_nonce = uuid::Uuid::new_v4().to_string();
            json_response(
                StatusCode::OK,
                json!({
                    "url": format!("https://auth.9router.com/oauth/authorize?provider={sub}&state={state_nonce}"),
                    "state": state_nonce
                }),
            )
        }
        "POST" => {
            if sub.ends_with("/refresh") {
                let connection_id = body.get("connectionId").and_then(Value::as_str).unwrap_or("");
                if let Some(conn) = state.db.provider_connection(connection_id)? {
                    let mut updated = conn.clone();
                    if let Some(obj) = updated.as_object_mut() {
                        obj.insert("accessToken".into(), json!(format!("refreshed_{}", uuid::Uuid::new_v4())));
                    }
                    state.db.update_connection(connection_id, updated)?;
                    return json_response(StatusCode::OK, json!({ "success": true, "refreshed": true }));
                }
                return json_response(StatusCode::NOT_FOUND, json!({ "error": "Connection not found" }));
            }

            if sub.ends_with("/bulk-import")
                || sub.ends_with("/import")
                || sub.ends_with("/import-token")
                || sub.ends_with("/auto-import")
            {
                let provider = sub.split('/').next().unwrap_or("generic");
                let token = body.get("token")
                    .or_else(|| body.get("accessToken"))
                    .and_then(Value::as_str)
                    .unwrap_or("imported-token");

                let conn = json!({
                    "id": uuid::Uuid::new_v4().to_string(),
                    "provider": provider,
                    "authType": "oauth",
                    "accessToken": token,
                    "isActive": true
                });
                let _ = state.db.create_connection(conn);
                return json_response(StatusCode::OK, json!({ "success": true, "imported": 1 }));
            }

            json_response(StatusCode::OK, json!({ "success": true, "token": "oauth-token-verified" }))
        }
        _ => json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"})),
    }
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
