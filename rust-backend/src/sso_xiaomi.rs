//! Native Xiaomi MiMo OAuth routes, ported from the pinned upstream
//! `src/app/api/oauth/xiaomi-mimo/{api-key,auto-import}/route.js` and
//! `open-sse/shared/mimoAccount.js` (`readDesktopPassToken`). The auto-import
//! route reads MiMo Desktop's Chromium cookie store the same way: copy the
//! SQLite file (Desktop holds an exclusive lock while running), then read the
//! `.account.xiaomi.com` cookies.

use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use serde_json::{json, Map, Value};

use crate::{error::AppError, state::AppState};

const DEFAULT_BASE_URL: &str = "https://api.xiaomimimo.com/v1";
const XIAOMI_PROVIDER: &str = "xiaomi-mimo";

// ---------------------------------------------------------------------------
// POST /api/oauth/xiaomi-mimo/api-key
// ---------------------------------------------------------------------------

pub async fn handle_api_key(state: &AppState, body: &Value) -> Result<Response<Body>, AppError> {
    match run_api_key(state, body).await {
        Ok(response) => Ok(response),
        Err(ApiKeyError::Client(message)) => Ok(json_error(StatusCode::BAD_REQUEST, &message)),
        Err(ApiKeyError::Server) => Ok(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "API key import failed",
        )),
    }
}

enum ApiKeyError {
    Client(String),
    Server,
}

async fn run_api_key(state: &AppState, body: &Value) -> Result<Response<Body>, ApiKeyError> {
    let object = body.as_object().cloned().unwrap_or_default();
    let str_field = |key: &str| object.get(key).and_then(Value::as_str);

    let raw_key = str_field("apiKey");
    let Some(raw_key) = raw_key else {
        return Err(ApiKeyError::Client("API key is required".into()));
    };
    let key = raw_key.trim();
    if key.is_empty() {
        return Err(ApiKeyError::Client("API key is required".into()));
    }
    if !key.starts_with("sk-") {
        return Err(ApiKeyError::Client(
            "Invalid key format — expected sk- prefix".into(),
        ));
    }
    let key = key.to_string();

    let uid = str_field("uid").filter(|value| !value.trim().is_empty());
    let base_url = str_field("baseUrl")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    let effective_base_url = base_url;

    // Validate the key against the models endpoint; network failures still
    // allow the import (soft-fail), matching upstream.
    let mut validated = false;
    let mut model_count = 0usize;
    let probe = state
        .http
        .get(format!("{effective_base_url}/models"))
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header("X-Mimo-Source", "mimocode-cli")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    if let Ok(response) = probe {
        if response.status().is_success() {
            if let Ok(data) = response.json::<Value>().await {
                if let Some(models) = data.get("data").and_then(Value::as_array) {
                    model_count = models.len();
                    validated = true;
                }
            }
        }
    }

    let specific = build_specific_data(&object, model_count);
    let connections = state
        .db
        .provider_connections(Some(XIAOMI_PROVIDER), None)
        .map_err(|_| ApiKeyError::Server)?;
    let email_for_uid = uid.map(|uid| format!("{uid}@xiaomi"));
    let existing = connections.into_iter().find(|connection| {
        connection.get("provider").and_then(Value::as_str) == Some(XIAOMI_PROVIDER)
            && (email_for_uid
                .as_ref()
                .map(|email| {
                    connection.get("email").and_then(Value::as_str) == Some(email.as_str())
                })
                .unwrap_or(false)
                || connection.get("accessToken").and_then(Value::as_str) == Some(key.as_str()))
    });

    if let Some(existing) = existing {
        let Some(existing_id) = existing
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Err(ApiKeyError::Server);
        };
        let previous = existing
            .get("providerSpecificData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut merged = previous;
        let uid_value = match uid {
            Some(uid) => Value::String(uid.to_string()),
            None => merged.get("uid").cloned().unwrap_or(Value::Null),
        };
        merged.insert("uid".into(), uid_value);
        merged.insert("baseUrl".into(), Value::String(effective_base_url.clone()));
        for field in ["mimoPassToken", "mimoUserId", "mimoCUserId"] {
            let incoming = specific.get(field).cloned().unwrap_or(Value::Null);
            let value = match incoming {
                Value::Null => merged.get(field).cloned().unwrap_or(Value::Null),
                value => value,
            };
            merged.insert(field.into(), value);
        }
        merged.insert("modelCount".into(), Value::from(model_count as u64));
        let test_status = if validated {
            Value::String("active".into())
        } else {
            existing.get("testStatus").cloned().unwrap_or(Value::Null)
        };
        let mut patch = Map::new();
        patch.insert("accessToken".into(), Value::String(key));
        patch.insert("providerSpecificData".into(), Value::Object(merged.clone()));
        if !test_status.is_null() {
            patch.insert("testStatus".into(), test_status);
        }
        let updated = state
            .db
            .update_connection(&existing_id, Value::Object(patch))
            .map_err(|_| ApiKeyError::Server)?;
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "success": true,
                "validated": validated,
                "modelCount": model_count,
                "updated": true,
                "connection": connection_summary(&updated),
            }),
        ));
    }

    let expires_at = (chrono::Utc::now() + chrono::Duration::days(365)).to_rfc3339();
    let mut connection = Map::new();
    connection.insert("provider".into(), Value::String(XIAOMI_PROVIDER.into()));
    connection.insert("authType".into(), Value::String("api_key".into()));
    connection.insert("accessToken".into(), Value::String(key));
    connection.insert("refreshToken".into(), Value::Null);
    connection.insert("expiresAt".into(), Value::String(expires_at));
    connection.insert(
        "email".into(),
        uid.map(|uid| Value::String(format!("{uid}@xiaomi")))
            .unwrap_or(Value::Null),
    );
    connection.insert(
        "displayName".into(),
        uid.map(|uid| Value::String(format!("Xiaomi {uid}")))
            .unwrap_or_else(|| Value::String("Xiaomi MiMo".into())),
    );
    connection.insert("providerSpecificData".into(), Value::Object(specific));
    connection.insert(
        "testStatus".into(),
        Value::String(if validated { "active" } else { "untested" }.into()),
    );
    let created = state
        .db
        .create_connection(Value::Object(connection))
        .map_err(|_| ApiKeyError::Server)?;
    Ok(json_response(
        StatusCode::OK,
        json!({
            "success": true,
            "validated": validated,
            "modelCount": model_count,
            "connection": connection_summary(&created),
        }),
    ))
}

fn build_specific_data(object: &Map<String, Value>, model_count: usize) -> Map<String, Value> {
    let str_or_null = |key: &str| -> Value {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(|value| Value::String(value.to_string()))
            .unwrap_or(Value::Null)
    };
    let mut specific = Map::new();
    specific.insert("uid".into(), str_or_null("uid"));
    specific.insert(
        "baseUrl".into(),
        Value::String(
            object
                .get("baseUrl")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        ),
    );
    specific.insert("authMethod".into(), Value::String("api_key".into()));
    specific.insert("provider".into(), Value::String("API Key".into()));
    specific.insert("modelCount".into(), Value::from(model_count as u64));
    for field in ["mimoPassToken", "mimoUserId", "mimoCUserId"] {
        specific.insert(field.into(), str_or_null(field));
    }
    specific
}

fn connection_summary(connection: &Value) -> Value {
    let data = connection
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let field = |name: &str| -> Value {
        connection
            .get(name)
            .cloned()
            .or_else(|| data.get(name).cloned())
            .unwrap_or(Value::Null)
    };
    let display_name = match field("displayName") {
        Value::Null => field("name"),
        value => value,
    };
    json!({
        "id": field("id"),
        "provider": field("provider"),
        "email": field("email"),
        "displayName": display_name,
    })
}

// ---------------------------------------------------------------------------
// GET /api/oauth/xiaomi-mimo/auto-import
// ---------------------------------------------------------------------------

pub async fn handle_auto_import(_state: &AppState) -> Result<Response<Body>, AppError> {
    match run_auto_import() {
        Ok(response) => Ok(response),
        Err(message) => Ok(json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "found": false, "error": message }),
        )),
    }
}

fn run_auto_import() -> Result<Response<Body>, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let home = std::path::PathBuf::from(home);

    let mut candidates: Vec<std::path::PathBuf> = vec![home
        .join(".local")
        .join("share")
        .join("mimocode")
        .join("auth.json")];
    if cfg!(windows) {
        let app_data = std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"));
        candidates.push(app_data.join("Xiaomi MiMo").join("auth.json"));
    }
    if cfg!(target_os = "macos") {
        candidates.push(
            home.join("Library")
                .join("Application Support")
                .join("mimocode")
                .join("auth.json"),
        );
    }

    let candidate_list = candidates
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let not_found_error = format!(
        "Xiaomi MiMo Desktop auth file not found. Checked:\n{candidate_list}\n\nMake sure Xiaomi MiMo Desktop is installed and you are signed in."
    );

    let auth_path = candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .ok_or(not_found_error)?;

    let raw = std::fs::read_to_string(auth_path).map_err(|error| error.to_string())?;
    let auth: Value = serde_json::from_str(&raw).map_err(|_| {
        "auth.json is not valid JSON. Please sign in to Xiaomi MiMo Desktop again.".to_string()
    })?;

    let xiaomi = auth.get("xiaomi").cloned().unwrap_or(Value::Null);
    let key_raw = xiaomi
        .get("key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(
            "No Xiaomi credentials found in auth.json. Please sign in to Xiaomi MiMo Desktop.",
        )?;
    let key = key_raw.trim().to_string();
    if !key.starts_with("sk-") {
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "found": false,
                "error": "Xiaomi key does not appear to be a valid API key (expected sk- prefix)."
            }),
        ));
    }

    let metadata = xiaomi.get("metadata").cloned().unwrap_or(Value::Null);
    let uid = metadata
        .get("uid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let base_url = metadata
        .get("base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .to_string();

    let desktop_pass_token = read_desktop_pass_token();

    Ok(json_response(
        StatusCode::OK,
        json!({
            "found": true,
            "apiKey": key,
            "uid": uid.map(Value::String).unwrap_or(Value::Null),
            "baseUrl": base_url,
            "source": auth_path.to_string_lossy(),
            "mimoPassToken": desktop_pass_token.as_ref().map(|t| t.pass_token.clone()).unwrap_or(Value::Null),
            "mimoUserId": desktop_pass_token.as_ref().map(|t| t.user_id.clone()).unwrap_or(Value::Null),
            "mimoCUserId": desktop_pass_token.as_ref().map(|t| t.c_user_id.clone()).unwrap_or(Value::Null),
        }),
    ))
}

struct DesktopPassToken {
    pass_token: Value,
    user_id: Value,
    c_user_id: Value,
}

/// Port of `readDesktopPassToken` from `open-sse/shared/mimoAccount.js`: read
/// the persisted Xiaomi account cookies out of MiMo Desktop's Chromium
/// profile. The cookie DB is exclusively locked while Desktop runs, so the
/// copy is what fails (returning `None`), mirroring upstream.
fn read_desktop_pass_token() -> Option<DesktopPassToken> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let home = std::path::PathBuf::from(home);
    let base = if cfg!(windows) {
        std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"))
            .join("Xiaomi MiMo")
    } else if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Xiaomi MiMo")
    } else {
        home.join(".config").join("Xiaomi MiMo")
    };
    let source = base
        .join("Partitions")
        .join("xiaomi-account")
        .join("Network")
        .join("Cookies");
    read_desktop_pass_token_from(&source)
}

fn read_desktop_pass_token_from(source: &std::path::Path) -> Option<DesktopPassToken> {
    if !source.is_file() {
        return None;
    }

    let unique = format!(
        "9r-mimo-cookies-{}-{:x}.db",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let temp = std::env::temp_dir().join(unique);
    std::fs::copy(source, &temp).ok()?;

    let result = (|| -> Option<DesktopPassToken> {
        let connection = rusqlite::Connection::open_with_flags(
            &temp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        let mut statement = connection
            .prepare("SELECT name, value FROM cookies WHERE host_key = ?1")
            .ok()?;
        let rows = statement
            .query_map([".account.xiaomi.com"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()?;
        let mut jar = std::collections::HashMap::new();
        for row in rows.flatten() {
            jar.insert(row.0, row.1);
        }
        let pass_token = jar.get("passToken")?;
        Some(DesktopPassToken {
            pass_token: Value::String(pass_token.clone()),
            user_id: jar
                .get("userId")
                .map(|value| Value::String(value.clone()))
                .unwrap_or(Value::Null),
            c_user_id: jar
                .get("cUserId")
                .map(|value| Value::String(value.clone()))
                .unwrap_or(Value::Null),
        })
    })();

    let _ = std::fs::remove_file(&temp);
    result
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(serde_json::to_vec(&value).unwrap_or_default()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, json!({ "error": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Start a tiny mock HTTP server for the /v1/models endpoint.
    fn start_mock_server(response_body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = response_body.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    #[test]
    fn read_desktop_pass_token_from_missing_file_returns_none() {
        let result = read_desktop_pass_token_from(std::path::Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }

    #[test]
    fn read_desktop_pass_token_from_valid_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("cookies.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies VALUES ('.account.xiaomi.com', 'passToken', 'test-pt')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies VALUES ('.account.xiaomi.com', 'userId', 'user-42')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies VALUES ('.account.xiaomi.com', 'cUserId', 'cuser-42')",
                [],
            )
            .unwrap();
        }
        let result = read_desktop_pass_token_from(&db_path).unwrap();
        assert_eq!(result.pass_token, Value::String("test-pt".into()));
        assert_eq!(result.user_id, Value::String("user-42".into()));
        assert_eq!(result.c_user_id, Value::String("cuser-42".into()));
    }

    #[test]
    fn read_desktop_pass_token_from_db_missing_passtoken() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("cookies.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cookies VALUES ('.account.xiaomi.com', 'userId', 'u1')",
                [],
            )
            .unwrap();
        }
        let result = read_desktop_pass_token_from(&db_path);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn api_key_create_new_connection() {
        let mock_response = r#"{"data":[{"id":"model-a"},{"id":"model-b"}]}"#;
        let base_url = start_mock_server(mock_response);

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: tmp.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let body = json!({
            "apiKey": "sk-test-12345",
            "uid": "alice",
            "baseUrl": base_url,
        });
        let response = handle_api_key(&state, &body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let data: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(data["success"], json!(true));
        assert_eq!(data["validated"], json!(true));
        assert_eq!(data["modelCount"], json!(2));
        assert_eq!(data["updated"], Value::Null);
        assert_eq!(data["connection"]["provider"], json!("xiaomi-mimo"));
        assert_eq!(data["connection"]["email"], json!("alice@xiaomi"));
    }

    #[tokio::test]
    async fn api_key_missing_key_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: tmp.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let response = handle_api_key(&state, &json!({})).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn api_key_non_sk_prefix_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: tmp.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let response = handle_api_key(&state, &json!({"apiKey": "bad-prefix"}))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
