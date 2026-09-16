//! Native port of the pinned upstream web-fetch endpoint.
//!
//! Mirrors `frontend/src/sse/handlers/fetch.js` (validation, provider
//! resolution, connection selection, error envelopes) and
//! `frontend/open-sse/handlers/fetch/index.js` (per-provider runners and the
//! response payload).

use axum::{body::Body, http::Method, response::Response};
use bytes::Bytes;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::time::Instant;

use crate::search_api::Credentials;
use crate::{
    api_errors::{error_response, method_not_allowed, ok_response, truncate_utf16},
    error::AppError,
    providers, ssrf_guard,
    state::AppState,
};

/// `DEFAULT_TIMEOUT_MS` from the upstream fetch handler.
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_FORMAT: &str = "markdown";

static JINA_TITLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?im)^\s*Title:\s*(.+)$").expect("title regex"));
static JINA_HEADING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*#\s+(.+)$").expect("heading regex"));

fn parse_jina_title(text: &str) -> Option<String> {
    if let Some(captures) = JINA_TITLE_RE.captures(text) {
        if let Some(title) = captures.get(1) {
            return Some(title.as_str().trim().to_string());
        }
    }
    JINA_HEADING_RE
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|title| title.as_str().trim().to_string())
}

struct FetchOutcome {
    status: u16,
    error: String,
}

type FetchResult = Result<Value, FetchOutcome>;

fn failure(status: u16, error: impl Into<String>) -> FetchResult {
    Err(FetchOutcome {
        status,
        error: error.into(),
    })
}

/// Upstream `truncate(text, max)` (a `max` of 0 means "no limit").
fn truncate(text: &str, max: Option<u64>) -> String {
    let max = max.filter(|value| *value > 0).map(|value| value as usize);
    truncate_utf16(text, max)
}

fn build_data(
    provider: &str,
    url: &str,
    title: Option<String>,
    format: &str,
    text: &str,
    links: Option<Value>,
    cost_usd: Value,
    response_ms: i64,
    upstream_ms: i64,
) -> Value {
    let mut data = Map::new();
    data.insert("provider".into(), json!(provider));
    data.insert("url".into(), json!(url));
    data.insert(
        "title".into(),
        title.map(|title| json!(title)).unwrap_or(Value::Null),
    );
    data.insert(
        "content".into(),
        json!({
            "format": format,
            "text": text,
            "length": text.encode_utf16().count(),
        }),
    );
    data.insert(
        "metadata".into(),
        json!({ "author": Value::Null, "published_at": Value::Null, "language": Value::Null }),
    );
    data.insert("usage".into(), json!({ "fetch_cost_usd": cost_usd }));
    data.insert(
        "metrics".into(),
        json!({ "response_time_ms": response_ms, "upstream_latency_ms": upstream_ms }),
    );
    if let Some(links) = links.filter(Value::is_array) {
        data.insert("links".into(), links);
    }
    Value::Object(data)
}

/// One upstream call with a hard timeout, mirroring upstream `tryFetch`.
async fn try_fetch(
    state: &AppState,
    url: &str,
    headers: &[(String, String)],
    body: Option<Value>,
    timeout_ms: u64,
) -> Result<reqwest::Response, FetchOutcome> {
    let mut request = state
        .http
        .post(url)
        .timeout(std::time::Duration::from_millis(timeout_ms.max(1)));
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    match request.send().await {
        Ok(response) => Ok(response),
        Err(error) => {
            let message = if error.is_timeout() {
                // Node reports aborted fetches as `AbortError: This operation was aborted`.
                "This operation was aborted".to_string()
            } else {
                error.to_string()
            };
            Err(FetchOutcome {
                status: if error.is_timeout() { 504 } else { 502 },
                error: message,
            })
        }
    }
}

/// Upstream `readJsonOrText`.
async fn read_json_or_text(response: reqwest::Response) -> (Option<Value>, Option<String>) {
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);
    if is_json {
        match response.json::<Value>().await {
            Ok(value) => (Some(value), None),
            Err(_) => (None, Some(String::new())),
        }
    } else {
        let text = response.text().await.unwrap_or_default();
        (None, Some(text))
    }
}

fn upstream_error(json: &Option<Value>, fallback: &str) -> String {
    json.as_ref()
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// Upstream `handleFetchCore`.
async fn handle_fetch_core(
    state: &AppState,
    url: &str,
    format: Option<&str>,
    max_characters: Option<u64>,
    provider_id: &str,
    provider_config: &Value,
    api_key: Option<&str>,
) -> FetchResult {
    let started_at = Instant::now();
    let fmt = format.unwrap_or(DEFAULT_FORMAT);
    let timeout_ms = provider_config
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let cost_per_query = provider_config
        .get("costPerQuery")
        .cloned()
        .unwrap_or(Value::Null);
    let response_ms = |started: Instant| started.elapsed().as_millis() as i64;
    let auth_header = |name: &str, value: Option<&str>| -> Vec<(String, String)> {
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(value) = value {
            headers.push((name.to_string(), format!("Bearer {value}")));
        }
        headers
    };

    match provider_id {
        "firecrawl" => {
            let upstream_start = Instant::now();
            let response = match try_fetch(
                state,
                "https://api.firecrawl.dev/v1/scrape",
                &auth_header("authorization", api_key),
                Some(json!({ "url": url, "formats": [fmt] })),
                timeout_ms,
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as i64;
            let status = response.status().as_u16();
            let (json_body, _) = read_json_or_text(response).await;
            if !(200..300).contains(&status) {
                return failure(
                    status,
                    upstream_error(&json_body, &format!("Firecrawl error: {status}")),
                );
            }
            let data = json_body
                .as_ref()
                .and_then(|value| value.get("data"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let text = truncate(
                data.get("markdown")
                    .and_then(Value::as_str)
                    .or_else(|| data.get("html").and_then(Value::as_str))
                    .or_else(|| data.get("text").and_then(Value::as_str))
                    .unwrap_or(""),
                max_characters,
            );
            let title = data
                .get("metadata")
                .and_then(|metadata| metadata.get("title"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(build_data(
                "firecrawl",
                url,
                title,
                fmt,
                &text,
                None,
                cost_per_query,
                response_ms(started_at),
                upstream_ms,
            ))
        }
        "jina-reader" => {
            let upstream_start = Instant::now();
            let response = match try_fetch(
                state,
                "https://r.jina.ai/",
                &auth_header("authorization", api_key),
                Some(json!({ "url": url })),
                timeout_ms,
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as i64;
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                let message = truncate_utf16(&body, Some(500));
                return failure(
                    status,
                    if message.is_empty() {
                        format!("Jina error: {status}")
                    } else {
                        message
                    },
                );
            }
            let text = truncate(&body, max_characters);
            Ok(build_data(
                "jina-reader",
                url,
                parse_jina_title(&body),
                fmt,
                &text,
                None,
                cost_per_query,
                response_ms(started_at),
                upstream_ms,
            ))
        }
        "tavily" => {
            let upstream_start = Instant::now();
            let response = match try_fetch(
                state,
                "https://api.tavily.com/extract",
                &auth_header("authorization", api_key),
                Some(json!({ "urls": [url], "extract_depth": "basic" })),
                timeout_ms,
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as i64;
            let status = response.status().as_u16();
            let (json_body, _) = read_json_or_text(response).await;
            if !(200..300).contains(&status) {
                return failure(
                    status,
                    upstream_error(&json_body, &format!("Tavily error: {status}")),
                );
            }
            let first = json_body
                .as_ref()
                .and_then(|value| value.get("results"))
                .and_then(Value::as_array)
                .and_then(|results| results.first())
                .cloned()
                .unwrap_or_else(|| json!({}));
            let text = truncate(
                first
                    .get("raw_content")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                max_characters,
            );
            Ok(build_data(
                "tavily",
                url,
                None,
                fmt,
                &text,
                None,
                cost_per_query,
                response_ms(started_at),
                upstream_ms,
            ))
        }
        "exa" => {
            let upstream_start = Instant::now();
            let headers = match api_key {
                Some(key) => vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("x-api-key".to_string(), key.to_string()),
                ],
                None => vec![("content-type".to_string(), "application/json".to_string())],
            };
            let response = match try_fetch(
                state,
                "https://api.exa.ai/contents",
                &headers,
                Some(json!({ "ids": [url], "text": true })),
                timeout_ms,
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as i64;
            let status = response.status().as_u16();
            let (json_body, _) = read_json_or_text(response).await;
            if !(200..300).contains(&status) {
                return failure(
                    status,
                    upstream_error(&json_body, &format!("Exa error: {status}")),
                );
            }
            let first = json_body
                .as_ref()
                .and_then(|value| value.get("results"))
                .and_then(Value::as_array)
                .and_then(|results| results.first())
                .cloned()
                .unwrap_or_else(|| json!({}));
            let text = truncate(
                first.get("text").and_then(Value::as_str).unwrap_or(""),
                max_characters,
            );
            let title = first
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(build_data(
                "exa",
                url,
                title,
                fmt,
                &text,
                None,
                cost_per_query,
                response_ms(started_at),
                upstream_ms,
            ))
        }
        "ollama" => {
            let base_url = provider_config
                .get("baseUrl")
                .and_then(Value::as_str)
                .unwrap_or("https://ollama.com/api/web_fetch");
            let upstream_start = Instant::now();
            let response = match try_fetch(
                state,
                base_url,
                &auth_header("authorization", api_key),
                Some(json!({ "url": url })),
                timeout_ms,
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as i64;
            let status = response.status().as_u16();
            let (json_body, text_body) = read_json_or_text(response).await;
            if !(200..300).contains(&status) {
                let message = json_body
                    .as_ref()
                    .and_then(|value| {
                        value
                            .get("error")
                            .and_then(Value::as_str)
                            .or_else(|| value.get("message").and_then(Value::as_str))
                    })
                    .map(str::to_string)
                    .or_else(|| {
                        text_body
                            .as_ref()
                            .map(|text| truncate_utf16(text, Some(500)))
                            .filter(|text| !text.is_empty())
                    })
                    .unwrap_or_else(|| format!("Ollama error: {status}"));
                return failure(status, message);
            }
            let Some(content) = json_body
                .as_ref()
                .and_then(|value| value.get("content"))
                .and_then(Value::as_str)
            else {
                return failure(
                    502,
                    "Ollama returned an empty or invalid web fetch response",
                );
            };
            let text = truncate(content, max_characters);
            let title = json_body
                .as_ref()
                .and_then(|value| value.get("title"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let links = json_body
                .as_ref()
                .and_then(|value| value.get("links"))
                .cloned();
            Ok(build_data(
                "ollama",
                url,
                title,
                fmt,
                &text,
                links,
                cost_per_query,
                response_ms(started_at),
                upstream_ms,
            ))
        }
        other => failure(400, format!("Unsupported provider: {other}")),
    }
}

/// Combo lookup mirroring `getComboModelsFromData`.
fn combo_models(state: &AppState, requested: &str) -> Option<Vec<String>> {
    if requested.contains('/') {
        return None;
    }
    let combo = state.db.combo_by_name(requested).ok().flatten()?;
    let models: Vec<String> = combo
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    (!models.is_empty()).then_some(models)
}

fn pick_credentials(
    state: &AppState,
    provider_id: &str,
    excluded: &HashSet<String>,
) -> Option<Credentials> {
    state
        .db
        .provider_connections(Some(provider_id), Some(true))
        .unwrap_or_default()
        .into_iter()
        .find(|connection| {
            connection
                .get("id")
                .and_then(Value::as_str)
                .map(|id| !excluded.contains(id))
                .unwrap_or(false)
        })
        .map(|connection| Credentials {
            api_key: connection
                .get("apiKey")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    connection
                        .get("accessToken")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                }),
            provider_specific_data: Some(connection.clone()),
            connection_id: connection
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
}

/// Upstream `handleSingleProviderFetch`.
async fn handle_single_provider(
    state: &AppState,
    body: &Value,
    provider_input: &str,
) -> Result<Response<Body>, AppError> {
    let Some(entry) = providers::provider_entry(provider_input) else {
        return error_response(400, Some(&format!("Unknown provider: {provider_input}")));
    };
    let provider_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(provider_input)
        .to_string();
    let provider_config = providers::media_config(&provider_id, "fetch");
    let has_config = provider_config
        .as_object()
        .map(|config| !config.is_empty())
        .unwrap_or(false);
    if !has_config {
        return error_response(
            400,
            Some(&format!(
                "Provider {provider_id} does not support web fetch"
            )),
        );
    }

    let url = body.get("url").and_then(Value::as_str).unwrap_or_default();
    let format = body
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_string);
    let max_characters = body.get("max_characters").and_then(Value::as_u64);

    let no_auth = entry
        .get("noAuth")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if no_auth {
        return match handle_fetch_core(
            state,
            url,
            format.as_deref(),
            max_characters,
            &provider_id,
            &provider_config,
            None,
        )
        .await
        {
            Ok(data) => ok_response(data),
            Err(outcome) => error_response(outcome.status, Some(&outcome.error)),
        };
    }

    let mut excluded: HashSet<String> = HashSet::new();
    let mut last: Option<FetchOutcome> = None;
    loop {
        let Some(credentials) = pick_credentials(state, &provider_id, &excluded) else {
            if excluded.is_empty() {
                return error_response(
                    400,
                    Some(&format!("No credentials for provider: {provider_id}")),
                );
            }
            let outcome = last.unwrap_or(FetchOutcome {
                status: 503,
                error: "All accounts unavailable".to_string(),
            });
            return error_response(outcome.status, Some(&outcome.error));
        };

        match handle_fetch_core(
            state,
            url,
            format.as_deref(),
            max_characters,
            &provider_id,
            &provider_config,
            credentials.api_key.as_deref(),
        )
        .await
        {
            Ok(data) => return ok_response(data),
            Err(outcome) => {
                // Upstream always falls back to the next connection.
                excluded.insert(credentials.connection_id);
                last = Some(outcome);
            }
        }
    }
}

/// POST /v1/web/fetch entry point.
pub async fn handle(
    state: &AppState,
    method: &Method,
    raw: &Bytes,
) -> Result<Response<Body>, AppError> {
    if method != Method::POST {
        return method_not_allowed();
    }
    let body: Value = match serde_json::from_slice(raw) {
        Ok(body @ Value::Object(_)) => body,
        _ => return error_response(400, Some("Invalid JSON body")),
    };
    let provider_input = body
        .get("provider")
        .and_then(Value::as_str)
        .or_else(|| body.get("model").and_then(Value::as_str));
    let Some(provider_input) = provider_input else {
        return error_response(400, Some("Missing required field: provider (or model)"));
    };
    let Some(url) = body.get("url").and_then(Value::as_str) else {
        return error_response(400, Some("Missing required field: url"));
    };
    if url::Url::parse(url).is_err() {
        return error_response(400, Some("Invalid URL format"));
    }
    if let Err(message) = ssrf_guard::assert_public_url_resolved(url).await {
        return error_response(400, Some(&message));
    }

    if let Some(models) = combo_models(state, provider_input) {
        let mut last: Option<Response<Body>> = None;
        for model in models {
            let response = handle_single_provider(state, &body, &model).await?;
            if response.status().is_success() {
                return Ok(response);
            }
            last = Some(response);
        }
        if let Some(response) = last {
            return Ok(response);
        }
    }

    handle_single_provider(state, &body, provider_input).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn jina_title_prefers_metadata_line() {
        assert_eq!(
            parse_jina_title("Title: Rust Programming Language\n\n# Other"),
            Some("Rust Programming Language".to_string())
        );
        assert_eq!(
            parse_jina_title("noise\n# Markdown Title\n"),
            Some("Markdown Title".to_string())
        );
        assert_eq!(parse_jina_title("nothing here"), None);
    }

    #[test]
    fn truncate_treats_zero_as_unlimited() {
        assert_eq!(truncate("abcdef", Some(3)), "abc");
        assert_eq!(truncate("abcdef", Some(0)), "abcdef");
        assert_eq!(truncate("abcdef", None), "abcdef");
    }

    #[test]
    fn fetch_data_shape_matches_upstream() {
        let data = build_data(
            "exa",
            "https://example.com",
            Some("Title".to_string()),
            "markdown",
            "hello",
            None,
            Value::Null,
            12,
            7,
        );
        assert_eq!(data["provider"], "exa");
        assert_eq!(data["title"], "Title");
        assert_eq!(data["content"]["format"], "markdown");
        assert_eq!(data["content"]["text"], "hello");
        assert_eq!(data["content"]["length"], 5);
        assert_eq!(data["metadata"]["author"], Value::Null);
        assert_eq!(data["usage"]["fetch_cost_usd"], Value::Null);
        assert_eq!(data["metrics"]["response_time_ms"], 12);
        assert!(data.get("links").is_none());

        let with_links = build_data(
            "ollama",
            "https://example.com",
            None,
            "markdown",
            "hi",
            Some(json!([{ "url": "https://example.com" }])),
            json!(0.001),
            3,
            1,
        );
        assert_eq!(with_links["links"][0]["url"], "https://example.com");
        assert_eq!(with_links["title"], Value::Null);
        assert_eq!(with_links["usage"]["fetch_cost_usd"], 0.001);
    }

    #[test]
    fn upstream_error_prefers_json_error_field() {
        assert_eq!(
            upstream_error(&Some(json!({ "error": "bad key" })), "Firecrawl error: 401"),
            "bad key"
        );
        assert_eq!(
            upstream_error(&Some(json!({})), "Firecrawl error: 401"),
            "Firecrawl error: 401"
        );
        assert_eq!(
            upstream_error(&None, "Tavily error: 500"),
            "Tavily error: 500"
        );
    }
}
