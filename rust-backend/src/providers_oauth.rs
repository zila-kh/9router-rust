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
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
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
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    json_response(StatusCode::OK, json!({ "ok": true, "latencyMs": 10 }))
}

pub async fn handle_providers_client(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let connections = state.db.provider_connections(None, Some(true))?;
    json_response(StatusCode::OK, json!({ "connections": connections }))
}

fn resolve_token_url(conn: &Value, provider: &str) -> Option<String> {
    if let Some(url) = conn.get("tokenUrl").and_then(Value::as_str) {
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    if let Some(url) = conn.get("refreshUrl").and_then(Value::as_str) {
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    if let Some(entry) = crate::providers::provider_entry(provider) {
        if let Some(oauth) = entry.get("oauth") {
            if let Some(url) = oauth
                .get("refreshUrl")
                .or_else(|| oauth.get("tokenUrl"))
                .and_then(Value::as_str)
            {
                return Some(url.to_string());
            }
        }
        if let Some(transport) = entry.get("transport") {
            if let Some(url) = transport
                .get("refreshUrl")
                .or_else(|| transport.get("tokenUrl"))
                .and_then(Value::as_str)
            {
                return Some(url.to_string());
            }
        }
    }
    None
}

pub async fn handle_oauth(
    state: &AppState,
    method: &Method,
    path: &str,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let sub = path.strip_prefix("/api/oauth/").unwrap_or("");
    if sub == "xiaomi-mimo/auto-import" && method == Method::GET {
        return crate::sso_xiaomi::handle_auto_import(state).await;
    }
    if sub == "xiaomi-mimo/api-key" && method == Method::POST {
        return crate::sso_xiaomi::handle_api_key(state, body).await;
    }
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
                let connection_id = body
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(conn) = state.db.provider_connection(connection_id)? else {
                    return json_response(
                        StatusCode::NOT_FOUND,
                        json!({ "error": "Connection not found" }),
                    );
                };
                let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
                let refresh_token = conn
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if refresh_token.is_empty() {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "No refresh token available for connection" }),
                    );
                }
                let Some(token_url) = resolve_token_url(&conn, provider) else {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!("No token refresh URL configured for provider '{provider}'") }),
                    );
                };
                let client_id = conn.get("clientId").and_then(Value::as_str).unwrap_or("");
                let client_secret = conn
                    .get("clientSecret")
                    .and_then(Value::as_str)
                    .unwrap_or("");

                let mut form = vec![
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ];
                if !client_id.is_empty() {
                    form.push(("client_id", client_id));
                }
                if !client_secret.is_empty() {
                    form.push(("client_secret", client_secret));
                }

                match state.http.post(&token_url).form(&form).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let token_data: Value = resp.json().await.unwrap_or(json!({}));
                        if let Some(new_access) =
                            token_data.get("access_token").and_then(Value::as_str)
                        {
                            let mut patch = json!({ "accessToken": new_access });
                            if let Some(new_refresh) =
                                token_data.get("refresh_token").and_then(Value::as_str)
                            {
                                patch["refreshToken"] = json!(new_refresh);
                            }
                            state.db.update_connection(connection_id, patch)?;
                            return json_response(
                                StatusCode::OK,
                                json!({ "success": true, "refreshed": true }),
                            );
                        }
                        return json_response(
                            StatusCode::BAD_GATEWAY,
                            json!({ "error": "Provider response did not contain access_token", "details": token_data }),
                        );
                    }
                    Ok(resp) => {
                        let err_text = resp.text().await.unwrap_or_default();
                        return json_response(
                            StatusCode::BAD_GATEWAY,
                            json!({ "error": "Upstream token refresh failed", "details": err_text }),
                        );
                    }
                    Err(e) => {
                        return json_response(
                            StatusCode::BAD_GATEWAY,
                            json!({ "error": format!("Network error refreshing token: {e}") }),
                        );
                    }
                }
            }

            if sub.ends_with("/bulk-import")
                || sub.ends_with("/import")
                || sub.ends_with("/import-token")
                || sub.ends_with("/auto-import")
            {
                let provider = sub.split('/').next().unwrap_or("generic");
                let token = match body
                    .get("token")
                    .or_else(|| body.get("accessToken"))
                    .and_then(Value::as_str)
                {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            json!({ "error": "token or accessToken required in body" }),
                        );
                    }
                };

                let conn = json!({
                    "id": uuid::Uuid::new_v4().to_string(),
                    "provider": provider,
                    "authType": "oauth",
                    "accessToken": token,
                    "isActive": true
                });
                state.db.create_connection(conn)?;
                return json_response(StatusCode::OK, json!({ "success": true, "imported": 1 }));
            }

            json_response(
                StatusCode::NOT_IMPLEMENTED,
                json!({ "error": format!("OAuth POST flow '{sub}' not supported natively") }),
            )
        }
        _ => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        ),
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
