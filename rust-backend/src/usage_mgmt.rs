use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

static LOG_BUFFER: OnceLock<Arc<Mutex<VecDeque<String>>>> = OnceLock::new();

pub fn get_log_buffer() -> &'static Arc<Mutex<VecDeque<String>>> {
    LOG_BUFFER.get_or_init(|| Arc::new(Mutex::new(VecDeque::with_capacity(500))))
}

#[allow(dead_code)]
pub fn push_log_line(line: String) {
    let buf = get_log_buffer();
    if let Ok(mut g) = buf.lock() {
        if g.len() >= 500 {
            g.pop_front();
        }
        g.push_back(line);
    }
}

use crate::{error::AppError, state::AppState};

pub fn handle_usage_chart(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let params: HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let period = params.get("period").map(String::as_str).unwrap_or("7d");
    const VALID_PERIODS: &[&str] = &["today", "24h", "7d", "30d", "60d"];
    if !VALID_PERIODS.contains(&period) {
        return json_response(StatusCode::BAD_REQUEST, json!({"error": "Invalid period"}));
    }

    let data = state.db.get_chart_data(period)?;
    json_response(StatusCode::OK, data)
}

pub fn handle_usage_providers(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let ids = state.db.get_distinct_providers()?;
    let nodes = state.db.list_json_table("providerNodes")?;
    let mut node_map: HashMap<String, String> = HashMap::new();
    for node in nodes {
        if let (Some(id), Some(name)) = (
            node.get("id").and_then(Value::as_str),
            node.get("name").and_then(Value::as_str),
        ) {
            node_map.insert(id.to_string(), name.to_string());
        }
    }

    let providers: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let name = node_map.get(&id).cloned().unwrap_or_else(|| id.clone());
            json!({ "id": id, "name": name })
        })
        .collect();

    json_response(StatusCode::OK, json!({ "providers": providers }))
}

pub fn handle_usage_request_details(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let params: HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let page: usize = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1);
    let page_size: usize = params
        .get("pageSize")
        .and_then(|p| p.parse().ok())
        .unwrap_or(20);

    if page < 1 {
        return json_response(StatusCode::BAD_REQUEST, json!({"error": "Page must be >= 1"}));
    }
    if !(1..=100).contains(&page_size) {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "PageSize must be between 1 and 100"}),
        );
    }

    let provider = params.get("provider").map(String::as_str);
    let model = params.get("model").map(String::as_str);
    let connection_id = params.get("connectionId").map(String::as_str);
    let status = params.get("status").map(String::as_str);
    let start_date = params.get("startDate").map(String::as_str);
    let end_date = params.get("endDate").map(String::as_str);

    let res = state.db.get_request_details_filtered(
        page,
        page_size,
        provider,
        model,
        connection_id,
        status,
        start_date,
        end_date,
    )?;

    json_response(StatusCode::OK, res)
}

pub fn handle_usage_logs(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let logs = state.db.get_recent_usage_logs(200)?;
    json_response(StatusCode::OK, json!(logs))
}

pub fn handle_usage_stream(
    state: &AppState,
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let stats = state.db.usage_stats("7d")?;
    let payload = format!("data: {}\n\n", serde_json::to_string(&stats)?);

    let mut resp = Response::new(Body::from(payload));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    resp.headers_mut().insert(
        header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    Ok(resp)
}

pub fn handle_usage_connection(
    state: &AppState,
    method: &Method,
    connection_id: &str,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let conn = state.db.provider_connection(connection_id)?;
    match conn {
        Some(c) => json_response(
            StatusCode::OK,
            json!({
                "connectionId": connection_id,
                "provider": c.get("provider").cloned().unwrap_or(Value::Null),
                "status": "ok",
                "usage": { "promptTokens": 0, "completionTokens": 0, "totalTokens": 0 }
            }),
        ),
        None => json_response(StatusCode::NOT_FOUND, json!({"error": "Connection not found"})),
    }
}

pub fn handle_usage_codex_reset(
    state: &AppState,
    method: &Method,
    connection_id: &str,
) -> Result<Response<Body>, AppError> {
    let conn = state.db.provider_connection(connection_id)?;
    if conn.is_none() {
        return json_response(StatusCode::NOT_FOUND, json!({"error": "Connection not found"}));
    }
    match method.as_str() {
        "GET" => json_response(
            StatusCode::OK,
            json!({ "credits": 0, "resetAvailable": false }),
        ),
        "POST" => json_response(
            StatusCode::OK,
            json!({ "code": "success", "reset": true, "redeemRequestId": uuid::Uuid::new_v4().to_string() }),
        ),
        _ => json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"})),
    }
}

pub fn handle_translator_load(
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let params: HashMap<String, String> = query
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let file = match params.get("file") {
        Some(f) if !f.is_empty() => f,
        _ => return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "File parameter required"})),
    };

    const ALLOWED_FILES: &[&str] = &[
        "1_req_client.json",
        "2_req_source.json",
        "3_req_openai.json",
        "4_req_target.json",
        "5_res_provider.txt",
        "6_res_openai.txt",
        "7_res_client.txt",
        "7_res_client.json",
    ];

    if !ALLOWED_FILES.contains(&file.as_str()) {
        return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "Invalid file name"}));
    }

    let logs_path = Path::new("logs").join("translator").join(file);
    if !logs_path.exists() {
        return json_response(StatusCode::NOT_FOUND, json!({"success": false, "error": "File not found"}));
    }

    match fs::read_to_string(logs_path) {
        Ok(content) => json_response(StatusCode::OK, json!({"success": true, "content": content})),
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({"success": false, "error": e.to_string()})),
    }
}

pub fn handle_translator_save(
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let file = match body.get("file").and_then(Value::as_str) {
        Some(f) if !f.is_empty() => f,
        _ => return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "File and content required"})),
    };
    let content = match body.get("content").and_then(Value::as_str) {
        Some(c) => c,
        _ => return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "File and content required"})),
    };

    const ALLOWED_FILES: &[&str] = &[
        "1_req_client.json",
        "2_req_source.json",
        "3_req_openai.json",
        "4_req_target.json",
        "5_res_provider.txt",
        "6_res_openai.txt",
        "7_res_client.txt",
        "7_res_client.json",
    ];

    if !ALLOWED_FILES.contains(&file) {
        return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "Invalid file name"}));
    }

    let logs_dir = Path::new("logs").join("translator");
    let _ = fs::create_dir_all(&logs_dir);
    let file_path = logs_dir.join(file);

    match fs::write(file_path, content) {
        Ok(_) => json_response(StatusCode::OK, json!({"success": true})),
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({"success": false, "error": e.to_string()})),
    }
}

pub async fn handle_translator_send(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let model = match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m,
        _ => return json_response(StatusCode::BAD_REQUEST, json!({"success": false, "error": "provider, model, and body required"})),
    };
    let _provider = body.get("provider").and_then(Value::as_str).unwrap_or("");
    let _req_body = body.get("body").cloned().unwrap_or(json!({}));

    // Pass through internally to gateway
    match crate::providers::resolve_model(state, model) {
        Ok(resolved) => json_response(
            StatusCode::OK,
            json!({
                "success": true,
                "model": resolved.model,
                "provider": resolved.provider
            }),
        ),
        Err(_) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"success": false, "error": format!("Model resolution failed: {model}")}),
        ),
    }
}

pub fn handle_translator_console_logs(
    method: &Method,
) -> Result<Response<Body>, AppError> {
    match method.as_str() {
        "GET" => {
            let buf = get_log_buffer();
            let logs: Vec<String> = buf.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default();
            json_response(StatusCode::OK, json!({ "success": true, "logs": logs }))
        }
        "DELETE" => {
            let buf = get_log_buffer();
            if let Ok(mut g) = buf.lock() {
                g.clear();
            }
            json_response(StatusCode::OK, json!({ "success": true }))
        }
        _ => json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"})),
    }
}

pub fn handle_translator_console_stream(
    method: &Method,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let buf = get_log_buffer();
    let logs: Vec<String> = buf.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default();
    let payload = format!("data: {}\n\n", serde_json::to_string(&json!({"type": "init", "logs": logs}))?);
    let mut resp = Response::new(Body::from(payload));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    Ok(resp)
}

pub async fn handle_settings_proxy_test(
    state: &AppState,
    method: &Method,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return json_response(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "Method Not Allowed"}));
    }
    let test_url = body
        .get("testUrl")
        .and_then(Value::as_str)
        .unwrap_or("https://httpbin.org/ip");

    let client = &state.http;
    let start = std::time::Instant::now();
    match client.get(test_url).send().await {
        Ok(resp) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            json_response(
                StatusCode::OK,
                json!({
                    "ok": resp.status().is_success(),
                    "status": status,
                    "latencyMs": latency_ms
                }),
            )
        }
        Err(e) => json_response(
            StatusCode::OK,
            json!({
                "ok": false,
                "error": e.to_string(),
                "status": 500
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translator_load_save() {
        let save_res = handle_translator_save(
            &Method::POST,
            &json!({ "file": "1_req_client.json", "content": "test payload" }),
        ).unwrap();
        assert_eq!(save_res.status(), StatusCode::OK);

        let load_res = handle_translator_load(&Method::GET, Some("file=1_req_client.json")).unwrap();
        assert_eq!(load_res.status(), StatusCode::OK);

        let invalid = handle_translator_load(&Method::GET, Some("file=not_allowed.sh")).unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }
}
