use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::{error::AppError, state::AppState};

pub const LOCALES: &[&str] = &[
    "en", "vi", "zh-CN", "zh-TW", "ja", "pt-BR", "pt-PT", "ko", "es", "de", "fr", "he", "ar", "ru",
    "pl", "cs", "nl", "tr", "uk", "tl", "id", "km", "th", "hi", "bn", "ur", "ro", "sv", "it", "el",
    "hu", "fi", "da", "no", "fa",
];

pub fn normalize_locale(l: &str) -> &'static str {
    match l {
        "zh" | "zh-CN" => "zh-CN",
        "en" => "en",
        "vi" => "vi",
        "zh-TW" => "zh-TW",
        "ja" => "ja",
        "pt-BR" => "pt-BR",
        "pt-PT" => "pt-PT",
        "ko" => "ko",
        "es" => "es",
        "de" => "de",
        "fr" => "fr",
        "he" => "he",
        "ar" => "ar",
        "ru" => "ru",
        "pl" => "pl",
        "cs" => "cs",
        "nl" => "nl",
        "tr" => "tr",
        "uk" => "uk",
        "tl" => "tl",
        "id" => "id",
        "km" => "km",
        "th" => "th",
        "hi" => "hi",
        "bn" => "bn",
        "ur" => "ur",
        "ro" => "ro",
        "sv" => "sv",
        "it" => "it",
        "el" => "el",
        "hu" => "hu",
        "fi" => "fi",
        "da" => "da",
        "no" => "no",
        "fa" => "fa",
        _ => "en",
    }
}

pub fn is_supported_locale(l: &str) -> bool {
    LOCALES.contains(&l)
}

pub fn handle_locale(method: &Method, body: &Value) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let req_locale = body.get("locale").and_then(Value::as_str).unwrap_or("");
    if req_locale.is_empty() || !is_supported_locale(req_locale) {
        return json_response(StatusCode::BAD_REQUEST, json!({"error": "Invalid locale"}));
    }
    let normalized = normalize_locale(req_locale);
    let cookie_val = format!("locale={normalized}; Path=/; Max-Age=31536000; SameSite=Lax");
    let mut resp = json_response(
        StatusCode::OK,
        json!({"success": true, "locale": normalized}),
    )?;
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie_val).map_err(|e| AppError::Internal(e.into()))?,
    );
    Ok(resp)
}

pub fn handle_models_disabled(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let params: HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    match method.as_str() {
        "GET" => {
            let all = state.db.get_disabled_models()?;
            if let Some(alias) = params.get("providerAlias") {
                let ids = all.get(alias).cloned().unwrap_or_else(|| json!([]));
                json_response(StatusCode::OK, json!({ "ids": ids }))
            } else {
                json_response(StatusCode::OK, json!({ "disabled": all }))
            }
        }
        "POST" => {
            let provider_alias = body
                .get("providerAlias")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ids: Vec<String> = body
                .get("ids")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            if provider_alias.is_empty() || !body.get("ids").map_or(false, Value::is_array) {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "providerAlias and ids[] required" }),
                );
            }
            state.db.disable_models(provider_alias, &ids)?;
            json_response(StatusCode::OK, json!({ "success": true }))
        }
        "DELETE" => {
            let provider_alias = match params.get("providerAlias") {
                Some(p) if !p.is_empty() => p,
                _ => {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "providerAlias required" }),
                    )
                }
            };
            let ids = params
                .get("id")
                .map(|id| vec![id.clone()])
                .unwrap_or_default();
            state.db.enable_models(provider_alias, &ids)?;
            json_response(StatusCode::OK, json!({ "success": true }))
        }
        _ => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        ),
    }
}

pub fn handle_models_availability(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    const MODEL_LOCK_PREFIX: &str = "modelLock_";
    let now = Utc::now();

    match method.as_str() {
        "GET" => {
            let connections = state.db.provider_connections(None, None)?;
            let mut models: Vec<Value> = Vec::new();

            for conn in connections {
                let conn_id = conn.get("id").and_then(Value::as_str).unwrap_or("");
                let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
                let name = conn
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| conn.get("email").and_then(Value::as_str))
                    .unwrap_or(conn_id);
                let last_error = conn.get("lastError").cloned().unwrap_or(Value::Null);
                let test_status = conn.get("testStatus").and_then(Value::as_str).unwrap_or("");

                let mut lock_count = 0;
                if let Some(obj) = conn.as_object() {
                    for (k, v) in obj {
                        if k.starts_with(MODEL_LOCK_PREFIX) {
                            if let Some(until_str) = v.as_str() {
                                if let Ok(until_dt) = DateTime::parse_from_rfc3339(until_str) {
                                    if until_dt.with_timezone(&Utc) > now {
                                        let model_suffix = &k[MODEL_LOCK_PREFIX.len()..];
                                        let model = if model_suffix.is_empty() {
                                            "__all"
                                        } else {
                                            model_suffix
                                        };
                                        lock_count += 1;
                                        models.push(json!({
                                            "provider": provider,
                                            "model": model,
                                            "status": "cooldown",
                                            "until": until_str,
                                            "connectionId": conn_id,
                                            "connectionName": name,
                                            "lastError": last_error,
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }

                if lock_count == 0 && test_status == "unavailable" {
                    models.push(json!({
                        "provider": provider,
                        "model": "__all",
                        "status": "unavailable",
                        "connectionId": conn_id,
                        "connectionName": name,
                        "lastError": last_error,
                    }));
                }
            }

            let len = models.len();
            json_response(
                StatusCode::OK,
                json!({
                    "models": models,
                    "unavailableCount": len,
                }),
            )
        }
        "POST" => {
            let action = body.get("action").and_then(Value::as_str).unwrap_or("");
            let provider = body.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = body.get("model").and_then(Value::as_str).unwrap_or("");

            if action != "clearCooldown" || provider.is_empty() || model.is_empty() {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "Invalid request" }),
                );
            }

            let lock_key = format!("{MODEL_LOCK_PREFIX}{model}");
            let connections = state.db.provider_connections(Some(provider), None)?;
            for conn in connections {
                if conn.get(&lock_key).is_some() {
                    let id = conn.get("id").and_then(Value::as_str).unwrap_or("");
                    if !id.is_empty() {
                        let mut patch = json!({ lock_key.clone(): Value::Null });
                        if conn.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
                            patch["testStatus"] = json!("active");
                            patch["lastError"] = Value::Null;
                            patch["lastErrorAt"] = Value::Null;
                            patch["backoffLevel"] = json!(0);
                        }
                        let _ = state.db.update_connection(id, patch);
                    }
                }
            }
            json_response(StatusCode::OK, json!({ "ok": true }))
        }
        _ => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        ),
    }
}

pub fn handle_models_catalog_sync(
    _state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    match method.as_str() {
        "GET" => {
            let catalog = crate::providers::catalog();
            let models_cnt = catalog
                .get("models")
                .and_then(Value::as_object)
                .map(|m| m.len())
                .unwrap_or(0);
            let providers_cnt = catalog
                .get("providers")
                .and_then(Value::as_object)
                .map(|p| p.len())
                .unwrap_or(0);

            let resp_payload = json!({
                "lastSync": null,
                "isSyncing": false,
                "lastError": null,
                "catalog": {
                    "syncedAt": Utc::now().to_rfc3339(),
                    "models": models_cnt,
                    "providers": providers_cnt,
                    "bytes": 0,
                }
            });
            json_response(StatusCode::OK, resp_payload)
        }
        "POST" => json_response(
            StatusCode::OK,
            json!({
                "success": true,
                "result": { "synced": false, "source": "static-catalog", "message": "Catalog is embedded at compile time; runtime sync disabled" }
            }),
        ),
        _ => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        ),
    }
}

pub async fn handle_models_test(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m,
        _ => return json_response(StatusCode::BAD_REQUEST, json!({"error": "Model required"})),
    };

    let ping_payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1
    });
    let canonical =
        match crate::translate::normalize_request(ping_payload, crate::translate::Format::OpenAi) {
            Ok(c) => c,
            Err(e) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"ok": false, "error": e.to_string()}),
                )
            }
        };
    let empty_headers = axum::http::HeaderMap::new();
    let start = std::time::Instant::now();
    match crate::gateway::execute_target_direct(
        state,
        &empty_headers,
        crate::translate::Format::OpenAi,
        false,
        canonical,
        model,
    )
    .await
    {
        Ok(resp) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let is_success = resp.status().is_success();
            let status = resp.status().as_u16();
            json_response(
                StatusCode::OK,
                json!({
                    "ok": is_success,
                    "status": status,
                    "latencyMs": latency_ms,
                    "error": if is_success { None } else { Some(format!("Model ping returned status {status}")) }
                }),
            )
        }
        Err(e) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            json_response(
                StatusCode::OK,
                json!({
                    "ok": false,
                    "status": 502,
                    "latencyMs": latency_ms,
                    "error": e.to_string()
                }),
            )
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locale_normalization() {
        assert_eq!(normalize_locale("zh"), "zh-CN");
        assert_eq!(normalize_locale("vi"), "vi");
        assert_eq!(normalize_locale("en"), "en");
        assert_eq!(normalize_locale("invalid-xyz"), "en");
        assert!(is_supported_locale("zh-CN"));
        assert!(is_supported_locale("en"));
        assert!(!is_supported_locale("invalid-xyz"));
    }

    #[test]
    fn test_locale_handler() {
        let resp = handle_locale(&Method::POST, &json!({"locale": "vi"})).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key(header::SET_COOKIE));

        let invalid = handle_locale(&Method::POST, &json!({"locale": "invalid-foo"})).unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_catalog_sync_handler() {
        let dummy_cfg = crate::config::Config::from_env().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let state = AppState::new(dummy_cfg, db).unwrap();

        let resp_get = handle_models_catalog_sync(&state, &Method::GET).unwrap();
        assert_eq!(resp_get.status(), StatusCode::OK);

        let resp_post = handle_models_catalog_sync(&state, &Method::POST).unwrap();
        assert_eq!(resp_post.status(), StatusCode::OK);
    }
}
