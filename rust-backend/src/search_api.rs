//! Native port of the pinned upstream web-search endpoint.
//!
//! Mirrors `frontend/src/sse/handlers/search.js` (request validation, provider
//! resolution, connection selection, connection failover) and
//! `frontend/open-sse/handlers/search/{index,callers,normalizers}.js` (query
//! sanitising, per-provider request construction, upstream error mapping and
//! response normalisation).

use axum::{
    body::Body,
    http::{HeaderMap, Method},
    response::Response,
};
use bytes::Bytes;
use chrono::Datelike;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::time::Instant;

use crate::{
    api_errors::{
        error_response, method_not_allowed, ok_response, provider_error_response, truncate_utf16,
    },
    error::AppError,
    providers,
    ssrf_guard::{self, FetchFailure, PublicRequest},
    state::AppState,
};

/// `GLOBAL_TIMEOUT_MS` from the upstream search handler.
const GLOBAL_TIMEOUT_MS: u64 = 15_000;
/// `providerConfig.timeoutMs || 10000`.
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Upstream caps the provider error text at 200 characters.
const ERROR_TEXT_LIMIT: usize = 200;

/// Query control characters rejected by upstream `sanitizeQuery`.
fn has_control_chars(query: &str) -> bool {
    query.chars().any(|ch| {
        let code = ch as u32;
        matches!(code, 0..=8 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
    })
}

/// Upstream `sanitizeQuery`: NFKC normalise, trim and collapse whitespace.
fn sanitize_query(query: &str) -> Result<String, String> {
    if has_control_chars(query) {
        return Err("Query contains invalid control characters".to_string());
    }
    // `char::is_whitespace` covers the Unicode whitespace classes JS `\s` matches.
    let collapsed = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err("Query is empty after normalization".to_string());
    }
    Ok(collapsed)
}

/// Upstream `sanitizeHeaders`: drop non-Latin1 characters and trim values.
fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| (*ch as u32) <= 0xff)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Parameters passed to the per-provider request builders.
pub struct SearchParams {
    pub query: String,
    pub search_type: String,
    pub max_results: u64,
    pub token: Option<String>,
    pub country: Option<String>,
    pub language: Option<String>,
    pub time_range: Option<String>,
    pub offset: Option<f64>,
    pub domain_filter: Vec<String>,
    pub content_options: Option<Value>,
    pub provider_options: Option<Value>,
    pub provider_specific_data: Option<Value>,
}

/// A fully constructed upstream request.
#[derive(Debug)]
pub struct BuiltRequest {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

fn setting_string(value: Option<&Value>, key: &str) -> Option<String> {
    let raw = value?.get(key)?;
    let text = raw.as_str()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Upstream `getProviderSetting`: `provider_options` wins over connection data.
fn provider_setting(params: &SearchParams, key: &str) -> Option<String> {
    setting_string(params.provider_options.as_ref(), key)
        .or_else(|| setting_string(params.provider_specific_data.as_ref(), key))
}

/// Upstream `parseDomainFilter`.
fn parse_domain_filter(domain_filter: &[String]) -> (Vec<String>, Vec<String>) {
    let includes = domain_filter
        .iter()
        .filter(|entry| !entry.starts_with('-'))
        .cloned()
        .collect();
    let excludes = domain_filter
        .iter()
        .filter(|entry| entry.starts_with('-'))
        .map(|entry| entry[1..].to_string())
        .collect();
    (includes, excludes)
}

/// Upstream `resolveBaseUrl` (trailing slashes stripped, override validated).
fn resolve_base_url(config: &Value, params: &SearchParams) -> Result<String, String> {
    let base = config
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let override_url = provider_setting(params, "baseUrl");
    if let Some(override_url) = override_url.as_deref() {
        let parsed = url::Url::parse(override_url)
            .map_err(|_| format!("Invalid baseUrl: {override_url}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(format!("Invalid baseUrl protocol: {}:", parsed.scheme()));
        }
        ssrf_guard::assert_public_url(override_url)
            .map_err(|_| "Blocked URL: internal host".to_string())?;
    }
    let target = override_url.unwrap_or_else(|| base.to_string());
    Ok(target.trim_end_matches('/').to_string())
}

/// Upstream `toPageNumber`.
fn to_page_number(offset: Option<f64>, max_results: u64) -> Option<u64> {
    let offset = offset?;
    if !offset.is_finite() || offset <= 0.0 || max_results == 0 {
        return None;
    }
    Some((offset / max_results as f64).floor() as u64 + 1)
}

fn query_string(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn json_body(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn bearer_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(token) = token {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

fn utc_days_ago(days: u32) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days as i64))
        .format("%Y-%m-%d")
        .to_string()
}

fn utc_months_ago(months: u32) -> String {
    let now = chrono::Utc::now();
    let year = now.year();
    let month = now.month();
    let target_month = month as i32 - months as i32;
    let (year, month) = if target_month <= 0 {
        (year - 1, (target_month + 12) as u32)
    } else {
        (year, target_month as u32)
    };
    chrono::NaiveDate::from_ymd_opt(year, month, now.day())
        .unwrap_or_else(|| now.date_naive())
        .format("%Y-%m-%d")
        .to_string()
}

fn utc_years_ago(years: u32) -> String {
    let now = chrono::Utc::now();
    chrono::NaiveDate::from_ymd_opt(now.year() - years as i32, now.month(), now.day())
        .unwrap_or_else(|| now.date_naive())
        .format("%Y-%m-%d")
        .to_string()
}

/// Upstream `BUILDERS` table plus the generic fallback for unknown providers.
pub fn build_search_request(
    provider_id: &str,
    config: &Value,
    params: &SearchParams,
) -> Result<BuiltRequest, String> {
    match provider_id {
        "serper" => build_serper(config, params),
        "brave-search" => build_brave(config, params),
        "exa" => build_exa(config, params),
        "tavily" => build_tavily(config, params),
        "google-pse" => build_google_pse(config, params),
        "linkup" => build_linkup(config, params),
        "searchapi" => build_searchapi(config, params),
        "youcom" => build_youcom(config, params),
        "searxng" => build_searxng(config, params),
        "xquik" => build_xquik(config, params),
        "ollama-search" => build_ollama_search(config, params),
        "glm" => build_glm(config, params),
        _ => generic_search_request(provider_id, config, params),
    }
}

fn build_serper(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let endpoint = if params.search_type == "news" {
        "/news"
    } else {
        "/search"
    };
    let mut body = Map::new();
    body.insert("q".into(), json!(params.query));
    body.insert("num".into(), json!(params.max_results));
    if let Some(country) = params.country.as_deref() {
        body.insert("gl".into(), json!(country.to_lowercase()));
    }
    if let Some(language) = params.language.as_deref() {
        body.insert("hl".into(), json!(language));
    }
    let token = params.token.clone().unwrap_or_default();
    Ok(BuiltRequest {
        url: format!("{}{endpoint}", resolve_base_url(config, params)?),
        method: Method::POST,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("X-API-Key".into(), token),
        ],
        body: Some(json_body(&Value::Object(body))),
    })
}

fn build_brave(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let endpoint = if params.search_type == "news" {
        "/news/search"
    } else {
        "/web/search"
    };
    let mut pairs = vec![
        ("q".to_string(), params.query.clone()),
        ("count".to_string(), params.max_results.to_string()),
    ];
    if let Some(country) = params.country.as_deref() {
        pairs.push(("country".to_string(), country.to_string()));
    }
    if let Some(language) = params.language.as_deref() {
        pairs.push(("search_lang".to_string(), language.to_string()));
    }
    Ok(BuiltRequest {
        url: format!(
            "{}{endpoint}?{}",
            resolve_base_url(config, params)?,
            query_string(&pairs)
        ),
        method: Method::GET,
        headers: vec![
            ("Accept".into(), "application/json".into()),
            (
                "X-Subscription-Token".into(),
                params.token.clone().unwrap_or_default(),
            ),
        ],
        body: None,
    })
}

fn build_exa(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let (includes, excludes) = parse_domain_filter(&params.domain_filter);
    let mut body = Map::new();
    body.insert("query".into(), json!(params.query));
    body.insert("numResults".into(), json!(params.max_results));
    body.insert("type".into(), json!("auto"));
    body.insert("text".into(), json!(true));
    body.insert("highlights".into(), json!(true));
    if !includes.is_empty() {
        body.insert("includeDomains".into(), json!(includes));
    }
    if !excludes.is_empty() {
        body.insert("excludeDomains".into(), json!(excludes));
    }
    if params.search_type == "news" {
        body.insert("category".into(), json!("news"));
    }
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::POST,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("x-api-key".into(), params.token.clone().unwrap_or_default()),
        ],
        body: Some(json_body(&Value::Object(body))),
    })
}

fn build_tavily(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let (includes, excludes) = parse_domain_filter(&params.domain_filter);
    let mut body = Map::new();
    body.insert("query".into(), json!(params.query));
    body.insert("max_results".into(), json!(params.max_results));
    body.insert(
        "topic".into(),
        json!(if params.search_type == "news" {
            "news"
        } else {
            "general"
        }),
    );
    if !includes.is_empty() {
        body.insert("include_domains".into(), json!(includes));
    }
    if !excludes.is_empty() {
        body.insert("exclude_domains".into(), json!(excludes));
    }
    if let Some(country) = params.country.as_deref() {
        body.insert("country".into(), json!(country));
    }
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::POST,
        headers: bearer_headers(params.token.as_deref())
            .into_iter()
            .map(|(name, value)| (normalize_header_name(&name), value))
            .collect(),
        body: Some(json_body(&Value::Object(body))),
    })
}

fn build_google_pse(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let api_key = params.token.clone().unwrap_or_default();
    let cx = provider_setting(params, "cx");
    if api_key.is_empty() || cx.is_none() {
        return Err("Google Programmable Search requires both apiKey and cx".to_string());
    }
    let mut pairs = vec![
        ("key".to_string(), api_key),
        ("cx".to_string(), cx.unwrap_or_default()),
        ("q".to_string(), params.query.clone()),
        ("num".to_string(), params.max_results.min(10).to_string()),
    ];
    if let Some(country) = params.country.as_deref() {
        pairs.push(("gl".to_string(), country.to_lowercase()));
    }
    if let Some(language) = params.language.as_deref() {
        pairs.push(("hl".to_string(), language.to_string()));
    }
    if let Some(time_range) = params.time_range.as_deref() {
        if time_range != "any" {
            let date_restrict = match time_range {
                "day" => Some("d1"),
                "week" => Some("w1"),
                "month" => Some("m1"),
                "year" => Some("y1"),
                _ => None,
            };
            if let Some(date_restrict) = date_restrict {
                pairs.push(("dateRestrict".to_string(), date_restrict.to_string()));
            }
        }
    }
    if let Some(offset) = params.offset {
        if offset.is_finite() && offset > 0.0 {
            pairs.push((
                "start".to_string(),
                format!("{}", ((offset + 1.0) as u64).min(91)),
            ));
        }
    }
    Ok(BuiltRequest {
        url: format!(
            "{}?{}",
            resolve_base_url(config, params)?,
            query_string(&pairs)
        ),
        method: Method::GET,
        headers: vec![("Accept".into(), "application/json".into())],
        body: None,
    })
}

fn build_linkup(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let api_key = params
        .token
        .clone()
        .ok_or_else(|| "Linkup Search requires an API key".to_string())?;
    let (includes, excludes) = parse_domain_filter(&params.domain_filter);
    let depth = match provider_setting(params, "depth").as_deref() {
        Some(value @ ("fast" | "standard" | "deep")) => value.to_string(),
        _ => "standard".to_string(),
    };
    let mut body = Map::new();
    body.insert("q".into(), json!(params.query));
    body.insert("depth".into(), json!(depth));
    body.insert("outputType".into(), json!("searchResults"));
    body.insert("maxResults".into(), json!(params.max_results));
    if !includes.is_empty() {
        body.insert("includeDomains".into(), json!(includes));
    }
    if !excludes.is_empty() {
        body.insert("excludeDomains".into(), json!(excludes));
    }
    if let Some(time_range) = params.time_range.as_deref() {
        if time_range != "any" {
            let from_date = match time_range {
                "day" => Some(utc_days_ago(1)),
                "week" => Some(utc_days_ago(7)),
                "month" => Some(utc_months_ago(1)),
                "year" => Some(utc_years_ago(1)),
                _ => None,
            };
            if let Some(from_date) = from_date {
                body.insert("fromDate".into(), json!(from_date));
                body.insert(
                    "toDate".into(),
                    json!(chrono::Utc::now().format("%Y-%m-%d").to_string()),
                );
            }
        }
    }
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::POST,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {api_key}")),
        ],
        body: Some(json_body(&Value::Object(body))),
    })
}

fn build_searchapi(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let api_key = params
        .token
        .clone()
        .ok_or_else(|| "SearchAPI requires an API key".to_string())?;
    let mut pairs = vec![
        (
            "engine".to_string(),
            if params.search_type == "news" {
                "google_news".to_string()
            } else {
                "google".to_string()
            },
        ),
        ("q".to_string(), params.query.clone()),
        ("api_key".to_string(), api_key),
    ];
    if let Some(country) = params.country.as_deref() {
        pairs.push(("gl".to_string(), country.to_lowercase()));
    }
    if let Some(language) = params.language.as_deref() {
        pairs.push(("hl".to_string(), language.to_string()));
    }
    if let Some(page) = to_page_number(params.offset, params.max_results) {
        pairs.push(("page".to_string(), page.to_string()));
    }
    Ok(BuiltRequest {
        url: format!(
            "{}?{}",
            resolve_base_url(config, params)?,
            query_string(&pairs)
        ),
        method: Method::GET,
        headers: vec![("Accept".into(), "application/json".into())],
        body: None,
    })
}

fn build_youcom(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let api_key = params
        .token
        .clone()
        .ok_or_else(|| "You.com Search requires an API key".to_string())?;
    let (includes, excludes) = parse_domain_filter(&params.domain_filter);
    let mut pairs = vec![
        ("query".to_string(), params.query.clone()),
        ("count".to_string(), params.max_results.min(100).to_string()),
    ];
    if let Some(time_range) = params.time_range.as_deref() {
        if time_range != "any" {
            pairs.push(("freshness".to_string(), time_range.to_string()));
        }
    }
    if let Some(offset) = params.offset {
        if offset.is_finite() && offset > 0.0 && params.max_results > 0 {
            pairs.push((
                "offset".to_string(),
                format!(
                    "{}",
                    ((offset / params.max_results as f64).floor() as u64).min(9)
                ),
            ));
        }
    }
    if let Some(country) = params.country.as_deref() {
        pairs.push(("country".to_string(), country.to_string()));
    }
    if let Some(language) = params.language.as_deref() {
        pairs.push(("language".to_string(), language.to_string()));
    }
    if !includes.is_empty() {
        pairs.push(("include_domains".to_string(), includes.join(",")));
    }
    if !excludes.is_empty() {
        pairs.push(("exclude_domains".to_string(), excludes.join(",")));
    }
    let full_page = params
        .content_options
        .as_ref()
        .and_then(|options| options.get("full_page"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if full_page {
        pairs.push((
            "livecrawl".to_string(),
            if params.search_type == "news" {
                "news".to_string()
            } else {
                "web".to_string()
            },
        ));
        let format = params
            .content_options
            .as_ref()
            .and_then(|options| options.get("format"))
            .and_then(Value::as_str)
            .filter(|format| *format == "markdown")
            .map(|_| "markdown".to_string())
            .unwrap_or_else(|| "html".to_string());
        pairs.push(("livecrawl_formats".to_string(), format));
    }
    Ok(BuiltRequest {
        url: format!(
            "{}?{}",
            resolve_base_url(config, params)?,
            query_string(&pairs)
        ),
        method: Method::GET,
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("X-API-Key".into(), api_key),
        ],
        body: None,
    })
}

fn build_searxng(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let base_url = resolve_base_url(config, params)?;
    let url = if base_url.ends_with("/search") {
        base_url
    } else {
        format!("{base_url}/search")
    };
    let mut pairs = vec![
        ("q".to_string(), params.query.clone()),
        ("format".to_string(), "json".to_string()),
        (
            "categories".to_string(),
            if params.search_type == "news" {
                "news".to_string()
            } else {
                "general".to_string()
            },
        ),
    ];
    if let Some(language) = params.language.as_deref() {
        pairs.push(("language".to_string(), language.to_string()));
    }
    if let Some(time_range) = params.time_range.as_deref() {
        if time_range != "any" {
            pairs.push(("time_range".to_string(), time_range.to_string()));
        }
    }
    if let Some(page) = to_page_number(params.offset, params.max_results) {
        pairs.push(("pageno".to_string(), page.to_string()));
    }
    Ok(BuiltRequest {
        url: format!("{}?{}", url, query_string(&pairs)),
        method: Method::GET,
        headers: vec![("Accept".into(), "application/json".into())],
        body: None,
    })
}

fn build_xquik(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let api_key = params
        .token
        .clone()
        .ok_or_else(|| "Xquik requires an API key".to_string())?;
    let query_type = provider_setting(params, "queryType");
    if let Some(query_type) = query_type.as_deref() {
        if query_type != "Latest" && query_type != "Top" {
            return Err("Xquik queryType must be Latest or Top".to_string());
        }
    }
    let mut pairs = vec![
        ("q".to_string(), params.query.clone()),
        ("limit".to_string(), params.max_results.to_string()),
    ];
    if let Some(cursor) = provider_setting(params, "cursor") {
        pairs.push(("cursor".to_string(), cursor));
    }
    if let Some(query_type) = query_type {
        pairs.push(("queryType".to_string(), query_type));
    }
    if let Some(language) = params.language.as_deref() {
        pairs.push(("language".to_string(), language.to_string()));
    }
    Ok(BuiltRequest {
        url: format!(
            "{}?{}",
            resolve_base_url(config, params)?,
            query_string(&pairs)
        ),
        method: Method::GET,
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("x-api-key".into(), api_key),
        ],
        body: None,
    })
}

fn build_ollama_search(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let mut body = Map::new();
    body.insert("query".into(), json!(params.query));
    body.insert("max_results".into(), json!(params.max_results));
    if let Some(country) = params.country.as_deref() {
        body.insert("country".into(), json!(country));
    }
    if let Some(language) = params.language.as_deref() {
        body.insert("language".into(), json!(language));
    }
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::POST,
        headers: bearer_headers(params.token.as_deref()),
        body: Some(json_body(&Value::Object(body))),
    })
}

fn build_glm(config: &Value, params: &SearchParams) -> Result<BuiltRequest, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": format!("9r-{}", chrono::Utc::now().timestamp_millis()),
        "method": "tools/call",
        "params": {
            "name": "web_search_prime",
            "arguments": { "search_query": params.query, "count": params.max_results },
        }
    });
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::POST,
        headers: bearer_headers(params.token.as_deref()),
        body: Some(json_body(&body)),
    })
}

/// Upstream fallback for providers without a dedicated builder.
fn generic_search_request(
    _provider_id: &str,
    config: &Value,
    params: &SearchParams,
) -> Result<BuiltRequest, String> {
    let method = config
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST");
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(token) = params.token.as_deref() {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    let body = json!({
        "query": params.query,
        "max_results": params.max_results,
        "search_type": params.search_type,
    });
    Ok(BuiltRequest {
        url: resolve_base_url(config, params)?,
        method: Method::from_bytes(method.as_bytes()).unwrap_or(Method::POST),
        headers,
        body: Some(json_body(&body)),
    })
}

fn normalize_header_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "content-type" => "Content-Type".to_string(),
        "authorization" => "Authorization".to_string(),
        _ => name.to_string(),
    }
}

/// Normalised provider payload.
pub struct Normalized {
    pub results: Vec<Value>,
    pub total_results: Value,
    pub pagination: Option<Value>,
}

impl Normalized {
    fn empty() -> Self {
        Self {
            results: Vec::new(),
            total_results: Value::Null,
            pagination: None,
        }
    }

    fn with_length(results: Vec<Value>) -> Self {
        let total = json!(results.len());
        Self {
            results,
            total_results: total,
            pagination: None,
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn value_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Upstream `makeResult(providerId, item, idx, now)`.
fn make_result(provider_id: &str, item: &Value, index: usize, now: &str) -> Value {
    let url = item.get("url").and_then(Value::as_str).unwrap_or("");
    let display_url = (!url.is_empty()).then(|| {
        let stripped = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);
        let stripped = stripped.strip_prefix("www.").unwrap_or(stripped);
        stripped.split('?').next().unwrap_or(stripped).to_string()
    });
    let score = item.get("score").and_then(Value::as_f64);
    let full_text = item.get("full_text").and_then(Value::as_str);
    let mut result = Map::new();
    result.insert(
        "title".into(),
        json!(item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()),
    );
    result.insert("url".into(), json!(url));
    if let Some(display_url) = display_url {
        result.insert("display_url".into(), json!(display_url));
    }
    result.insert(
        "snippet".into(),
        json!(item
            .get("snippet")
            .and_then(Value::as_str)
            .unwrap_or_default()),
    );
    result.insert("position".into(), json!(index + 1));
    result.insert(
        "score".into(),
        match score {
            Some(score) if score.is_finite() => json!(score.clamp(0.0, 1.0)),
            _ => Value::Null,
        },
    );
    result.insert(
        "published_at".into(),
        item.get("published_at")
            .and_then(Value::as_str)
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    result.insert(
        "favicon_url".into(),
        item.get("favicon_url")
            .and_then(Value::as_str)
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    result.insert(
        "content".into(),
        match full_text {
            Some(text) => json!({
                "format": item
                    .get("text_format")
                    .and_then(Value::as_str)
                    .unwrap_or("text"),
                "text": text,
                "length": text.encode_utf16().count(),
            }),
            None => Value::Null,
        },
    );
    result.insert(
        "metadata".into(),
        json!({
            "author": item.get("author").and_then(Value::as_str),
            "language": Value::Null,
            "source_type": item.get("source_type").and_then(Value::as_str),
            "image_url": item.get("image_url").and_then(Value::as_str),
        }),
    );
    result.insert(
        "citation".into(),
        json!({ "provider": provider_id, "retrieved_at": now, "rank": index + 1 }),
    );
    result.insert("provider_raw".into(), Value::Null);
    Value::Object(result)
}

fn item_str<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

/// Build the intermediate item upstream passes into `makeResult` and render it.
fn mapped(provider_id: &str, item: &Value, index: usize, now: &str) -> Value {
    make_result(provider_id, item, index, now)
}

/// Build the intermediate item upstream passes into `makeResult`.
fn source_item(entries: Vec<(&str, Option<Value>)>) -> Value {
    let mut out = Map::new();
    for (key, value) in entries {
        match value {
            Some(value) if !value.is_null() => {
                out.insert(key.to_string(), value);
            }
            _ => {}
        }
    }
    Value::Object(out)
}

fn text(value: Option<&str>) -> Option<Value> {
    value.map(|value| json!(value))
}

fn text_owned(value: Option<String>) -> Option<Value> {
    value.map(|value| json!(value))
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

/// JS `Number(value)` for the payloads upstream coerces, preserving integers.
fn js_number(raw: &Value) -> Option<Value> {
    match raw {
        Value::Number(number) => Some(Value::Number(number.clone())),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(value) = trimmed.parse::<i64>() {
                return Some(json!(value));
            }
            trimmed.parse::<f64>().ok().map(|value| json!(value))
        }
        _ => None,
    }
}

fn normalize_serper(data: &Value, search_type: &str) -> Normalized {
    let now = now_iso();
    let items = if search_type == "news" {
        data.get("news")
    } else {
        data.get("organic")
    };
    let Some(items) = items.and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            mapped(
                "serper",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("link").cloned()),
                    (
                        "snippet",
                        item.get("snippet")
                            .cloned()
                            .or_else(|| item.get("description").cloned()),
                    ),
                    ("published_at", item.get("date").cloned()),
                ]),
                index,
                &now,
            )
        })
        .collect();
    let total = nested(data, &["searchParameters", "totalResults"])
        .filter(|value| value.is_number())
        .cloned()
        .unwrap_or(Value::Null);
    Normalized {
        results,
        total_results: total,
        pagination: None,
    }
}

fn normalize_brave(data: &Value, search_type: &str) -> Normalized {
    let now = now_iso();
    let container = if search_type == "news" {
        data.get("news").unwrap_or(data)
    } else {
        data.get("web").unwrap_or(data)
    };
    let Some(items) = container.get("results").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            mapped(
                "brave-search",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", item.get("description").cloned()),
                    (
                        "published_at",
                        item.get("page_age")
                            .cloned()
                            .or_else(|| item.get("age").cloned()),
                    ),
                    (
                        "favicon_url",
                        nested(item, &["meta_url", "favicon"])
                            .cloned()
                            .or_else(|| item.get("favicon").cloned()),
                    ),
                ]),
                index,
                &now,
            )
        })
        .collect();
    let total = container.get("totalCount").cloned().unwrap_or(Value::Null);
    Normalized {
        results,
        total_results: total,
        pagination: None,
    }
}

fn normalize_exa(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let Some(items) = data.get("results").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let snippet = nested(item, &["highlights"])
                .and_then(Value::as_array)
                .and_then(|highlights| highlights.first())
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .map(|text| truncate_utf16(text, Some(300)))
                })
                .unwrap_or_default();
            mapped(
                "exa",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", Some(json!(snippet))),
                    ("score", item.get("score").cloned()),
                    ("published_at", item.get("publishedDate").cloned()),
                    ("favicon_url", item.get("favicon").cloned()),
                    ("author", item.get("author").cloned()),
                    ("image_url", item.get("image").cloned()),
                    ("full_text", item.get("text").cloned()),
                    ("text_format", Some(json!("text"))),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_tavily(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let Some(items) = data.get("results").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            mapped(
                "tavily",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", item.get("content").cloned()),
                    ("score", item.get("score").cloned()),
                    ("published_at", item.get("published_date").cloned()),
                    ("full_text", item.get("raw_content").cloned()),
                    ("text_format", Some(json!("text"))),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_google_pse(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let image_url = nested(item, &["pagemap", "cse_image"])
                .and_then(Value::as_array)
                .and_then(|entries| entries.first())
                .and_then(|entry| entry.get("src"))
                .cloned()
                .or_else(|| {
                    nested(item, &["pagemap", "cse_thumbnail"])
                        .and_then(Value::as_array)
                        .and_then(|entries| entries.first())
                        .and_then(|entry| entry.get("src"))
                        .cloned()
                })
                .or_else(|| {
                    nested(item, &["pagemap", "metatags"])
                        .and_then(Value::as_array)
                        .and_then(|entries| entries.first())
                        .and_then(|entry| entry.get("og:image"))
                        .cloned()
                });
            mapped(
                "google-pse",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("link").cloned()),
                    ("snippet", item.get("snippet").cloned()),
                    ("image_url", image_url),
                ]),
                index,
                &now,
            )
        })
        .collect();
    let raw = nested(data, &["searchInformation", "totalResults"])
        .cloned()
        .or_else(|| {
            nested(data, &["queries", "request"])
                .and_then(Value::as_array)
                .and_then(|entries| entries.first())
                .and_then(|entry| entry.get("totalResults"))
                .cloned()
        });
    let total = raw.as_ref().and_then(js_number);
    Normalized {
        results,
        total_results: total.unwrap_or(Value::Null),
        pagination: None,
    }
}

fn normalize_linkup(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let Some(items) = data.get("results").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let title = item
                .get("name")
                .cloned()
                .or_else(|| item.get("title").cloned());
            let snippet = item
                .get("content")
                .cloned()
                .or_else(|| item.get("snippet").cloned())
                .unwrap_or_else(|| json!(""));
            mapped(
                "linkup",
                &source_item(vec![
                    ("title", title),
                    ("url", item.get("url").cloned()),
                    ("snippet", Some(snippet)),
                    (
                        "source_type",
                        Some(item.get("type").cloned().unwrap_or_else(|| json!("web"))),
                    ),
                    (
                        "image_url",
                        item.get("image_url")
                            .cloned()
                            .or_else(|| item.get("imageUrl").cloned()),
                    ),
                    ("full_text", item.get("content").cloned()),
                    ("text_format", Some(json!("text"))),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_searchapi(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let items: Vec<Value> = data
        .get("organic_results")
        .and_then(Value::as_array)
        .or_else(|| data.get("top_stories").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let results: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let snippet = item
                .get("snippet")
                .cloned()
                .or_else(|| item.get("description").cloned())
                .unwrap_or_else(|| json!(""));
            mapped(
                "searchapi",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("link").cloned()),
                    ("snippet", Some(snippet)),
                    (
                        "published_at",
                        item.get("date")
                            .cloned()
                            .or_else(|| item.get("published_at").cloned()),
                    ),
                    ("favicon_url", item.get("favicon").cloned()),
                    ("author", item.get("source").cloned()),
                    ("image_url", item.get("thumbnail").cloned()),
                ]),
                index,
                &now,
            )
        })
        .collect();
    let raw = nested(data, &["search_information", "total_results"]).cloned();
    let total_results = raw
        .as_ref()
        .and_then(js_number)
        .unwrap_or_else(|| json!(results.len()));
    Normalized {
        results,
        total_results,
        pagination: None,
    }
}

fn normalize_youcom(data: &Value, search_type: &str) -> Normalized {
    let now = now_iso();
    let container = data.get("results").filter(|value| value.is_object());
    let section = container
        .and_then(|container| {
            if search_type == "news" {
                container.get("news")
            } else {
                container.get("web")
            }
        })
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let results = section
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let first_snippet = item
                .get("snippets")
                .and_then(Value::as_array)
                .and_then(|snippets| snippets.iter().find_map(Value::as_str))
                .map(str::to_string);
            let livecrawl_text = item
                .get("markdown")
                .and_then(Value::as_str)
                .or_else(|| item.get("html").and_then(Value::as_str))
                .map(str::to_string);
            let livecrawl_format = if item.get("markdown").and_then(Value::as_str).is_some() {
                "markdown"
            } else {
                "html"
            };
            let snippet = first_snippet
                .clone()
                .or_else(|| {
                    item.get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let text_format = livecrawl_text
                .as_ref()
                .map(|_| livecrawl_format.to_string());
            mapped(
                "youcom",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", Some(json!(snippet))),
                    ("published_at", item.get("page_age").cloned()),
                    ("favicon_url", item.get("favicon_url").cloned()),
                    ("image_url", item.get("thumbnail_url").cloned()),
                    ("source_type", Some(json!(search_type))),
                    ("full_text", text_owned(livecrawl_text)),
                    ("text_format", text_owned(text_format)),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_searxng(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let Some(items) = data.get("results").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let snippet = item
                .get("content")
                .cloned()
                .or_else(|| item.get("snippet").cloned())
                .unwrap_or_else(|| json!(""));
            let source_type = item
                .get("engines")
                .and_then(Value::as_array)
                .map(|engines| {
                    engines
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .or_else(|| {
                    item.get("engine")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .or_else(|| {
                    item.get("category")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            mapped(
                "searxng",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", Some(snippet)),
                    (
                        "published_at",
                        item.get("publishedDate")
                            .cloned()
                            .or_else(|| item.get("published_date").cloned()),
                    ),
                    ("source_type", text_owned(source_type)),
                    (
                        "image_url",
                        item.get("thumbnail")
                            .cloned()
                            .or_else(|| item.get("img_src").cloned()),
                    ),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_xquik(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let Some(items) = data.get("tweets").and_then(Value::as_array) else {
        return Normalized::empty();
    };
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let username = nested(item, &["author", "username"])
                .and_then(Value::as_str)
                .unwrap_or("");
            let author_name = nested(item, &["author", "name"])
                .and_then(Value::as_str)
                .unwrap_or("");
            let tweet_id = value_to_string(item.get("id")).unwrap_or_default();
            let url = if !username.is_empty() && !tweet_id.is_empty() {
                format!(
                    "https://x.com/{}/status/{}",
                    percent_encode(username),
                    percent_encode(&tweet_id)
                )
            } else if !tweet_id.is_empty() {
                format!("https://x.com/i/web/status/{}", percent_encode(&tweet_id))
            } else {
                String::new()
            };
            let author = if !username.is_empty() {
                Some(format!("@{username}"))
            } else if !author_name.is_empty() {
                Some(author_name.to_string())
            } else {
                None
            };
            let title = match author.as_deref() {
                Some(author) => format!("{author} on X"),
                None => "X post".to_string(),
            };
            let image_url = item
                .get("media")
                .and_then(Value::as_array)
                .and_then(|media| {
                    media.iter().find_map(|entry| {
                        entry
                            .get("mediaUrl")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                });
            let item_text = item.get("text").and_then(Value::as_str);
            mapped(
                "xquik",
                &source_item(vec![
                    ("title", Some(json!(title))),
                    ("url", Some(json!(url))),
                    ("snippet", Some(json!(item_text.unwrap_or_default()))),
                    (
                        "published_at",
                        item.get("createdAt")
                            .and_then(Value::as_str)
                            .map(|value| json!(value)),
                    ),
                    ("author", text_owned(author)),
                    ("image_url", text_owned(image_url)),
                    ("source_type", Some(json!("x_post"))),
                    ("full_text", item_text.map(|value| json!(value))),
                    ("text_format", Some(json!("text"))),
                ]),
                index,
                &now,
            )
        })
        .collect();
    let next_cursor = data
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| json!(cursor))
        .unwrap_or(Value::Null);
    Normalized {
        results,
        total_results: Value::Null,
        pagination: Some(json!({
            "has_more": data.get("has_next_page").and_then(Value::as_bool).unwrap_or(false),
            "next_cursor": next_cursor,
        })),
    }
}

fn normalize_ollama_search(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let items = data
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| data.as_array())
        .cloned()
        .unwrap_or_default();
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let snippet = item
                .get("content")
                .cloned()
                .or_else(|| item.get("snippet").cloned())
                .unwrap_or_else(|| json!(""));
            mapped(
                "ollama-search",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", item.get("url").cloned()),
                    ("snippet", Some(snippet)),
                    ("full_text", item.get("content").cloned()),
                    ("text_format", Some(json!("text"))),
                    ("published_at", item.get("published_at").cloned()),
                    ("source_type", item.get("source").cloned()),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn normalize_glm(data: &Value, _search_type: &str) -> Normalized {
    let now = now_iso();
    let mut payload = data.clone();
    if let Some(text_content) = nested(data, &["result", "content"])
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("text"))
        .and_then(Value::as_str)
    {
        payload = serde_json::from_str(text_content).unwrap_or_else(|_| json!({}));
    }
    let items = payload
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| payload.get("news").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    let results = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let url = item
                .get("link")
                .cloned()
                .or_else(|| item.get("url").cloned());
            let snippet = item.get("content").cloned().unwrap_or_else(|| json!(""));
            mapped(
                "glm",
                &source_item(vec![
                    ("title", item.get("title").cloned()),
                    ("url", url),
                    ("snippet", Some(snippet)),
                    (
                        "published_at",
                        item.get("publish_date")
                            .cloned()
                            .or_else(|| item.get("published_at").cloned()),
                    ),
                    ("favicon_url", item.get("icon").cloned()),
                    ("source_type", item.get("media").cloned()),
                ]),
                index,
                &now,
            )
        })
        .collect();
    Normalized::with_length(results)
}

fn percent_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Upstream `normalizeSearchResponse(providerId, data, query, searchType)`.
pub fn normalize_search_response(provider_id: &str, data: &Value, search_type: &str) -> Normalized {
    match provider_id {
        "serper" => normalize_serper(data, search_type),
        "brave-search" => normalize_brave(data, search_type),
        "exa" => normalize_exa(data, search_type),
        "tavily" => normalize_tavily(data, search_type),
        "google-pse" => normalize_google_pse(data, search_type),
        "linkup" => normalize_linkup(data, search_type),
        "searchapi" => normalize_searchapi(data, search_type),
        "youcom" => normalize_youcom(data, search_type),
        "searxng" => normalize_searxng(data, search_type),
        "xquik" => normalize_xquik(data, search_type),
        "ollama-search" => normalize_ollama_search(data, search_type),
        "glm" => normalize_glm(data, search_type),
        _ => Normalized::empty(),
    }
}

/// Credentials for a single attempt.
pub struct Credentials {
    pub api_key: Option<String>,
    pub provider_specific_data: Option<Value>,
    pub connection_id: String,
}

enum CoreOutcome {
    Success(Value),
    Failure { status: u16, error: String },
}

impl CoreOutcome {
    fn failure(status: u16, error: impl Into<String>) -> Self {
        CoreOutcome::Failure {
            status,
            error: error.into(),
        }
    }
}

fn request_timeout_ms(config: &Value, started: Instant) -> u64 {
    let configured = config
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let elapsed = started.elapsed().as_millis() as u64;
    let remaining = GLOBAL_TIMEOUT_MS.saturating_sub(elapsed);
    configured.min(remaining.max(1_000))
}

/// Upstream `tryDedicatedProvider`.
async fn try_dedicated_provider(
    state: &AppState,
    provider_id: &str,
    provider_config: &Value,
    body: &Value,
    credentials: Option<&Credentials>,
    started: Instant,
) -> CoreOutcome {
    let start_time = Instant::now();
    let token = credentials.and_then(|credentials| credentials.api_key.clone());
    let auth_type = provider_config
        .get("authType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if auth_type != "none" && token.is_none() {
        return CoreOutcome::failure(401, format!("No credentials for provider: {provider_id}"));
    }

    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    let search_type = body
        .get("search_type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            provider_config
                .get("searchTypes")
                .and_then(Value::as_array)
                .and_then(|types| types.first())
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "web".to_string());
    let default_max = provider_config
        .get("defaultMaxResults")
        .and_then(Value::as_u64)
        .unwrap_or(5);
    let max_cap = provider_config
        .get("maxMaxResults")
        .and_then(Value::as_u64)
        .unwrap_or(100);
    let requested_max = body
        .get("max_results")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(default_max);
    let params = SearchParams {
        query: query.to_string(),
        search_type,
        max_results: requested_max.min(max_cap),
        token,
        country: body
            .get("country")
            .and_then(Value::as_str)
            .map(str::to_string),
        language: body
            .get("language")
            .and_then(Value::as_str)
            .map(str::to_string),
        time_range: body
            .get("time_range")
            .and_then(Value::as_str)
            .map(str::to_string),
        offset: body.get("offset").and_then(Value::as_f64),
        domain_filter: body
            .get("domain_filter")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        content_options: body.get("content_options").cloned(),
        provider_options: body.get("provider_options").cloned(),
        provider_specific_data: credentials
            .and_then(|credentials| credentials.provider_specific_data.clone()),
    };

    let built = match build_search_request(provider_id, provider_config, &params) {
        Ok(built) => built,
        Err(message) => {
            let message = if message.is_empty() {
                format!("Invalid request for {provider_id}")
            } else {
                message
            };
            return CoreOutcome::failure(400, message);
        }
    };

    let timeout_ms = request_timeout_ms(provider_config, started);
    let mut headers = HeaderMap::new();
    for (name, value) in &built.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&sanitize_header_value(value)),
        ) {
            headers.insert(name, value);
        }
    }
    let request = PublicRequest {
        method: built.method.clone(),
        headers,
        body: built.body.clone(),
        timeout_ms: Some(timeout_ms),
    };

    let response = match ssrf_guard::fetch_public(&state.proxy_http, &built.url, &request).await {
        Ok(response) => response,
        Err(failure) => {
            let is_timeout = matches!(failure, FetchFailure::Timeout);
            return CoreOutcome::failure(
                if is_timeout { 504 } else { 502 },
                format!(
                    "{provider_id} {}: {}",
                    if is_timeout { "timeout" } else { "error" },
                    failure.message()
                ),
            );
        }
    };

    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return CoreOutcome::failure(
            status,
            format!(
                "{provider_id} returned {status}: {}",
                truncate_utf16(&text, Some(ERROR_TEXT_LIMIT))
            ),
        );
    }
    let data: Value = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(_) => {
            return CoreOutcome::failure(
                502,
                format!("{provider_id} error: could not parse response body as JSON"),
            )
        }
    };

    CoreOutcome::Success(success_payload(
        provider_id,
        provider_config,
        &params,
        &data,
        start_time.elapsed().as_millis() as i64,
    ))
}

/// Upstream success payload: `{ provider, query, results, answer, usage,
/// pagination?, metrics, errors }` (search/index.js lines 112-135).
fn success_payload(
    provider_id: &str,
    provider_config: &Value,
    params: &SearchParams,
    data: &Value,
    duration_ms: i64,
) -> Value {
    let normalized = normalize_search_response(provider_id, data, &params.search_type);
    let results: Vec<Value> = normalized
        .results
        .into_iter()
        .take(params.max_results as usize)
        .collect();
    let mut usage = Map::new();
    usage.insert("queries_used".into(), json!(1));
    usage.insert(
        "search_cost_usd".into(),
        provider_config
            .get("costPerQuery")
            .cloned()
            .unwrap_or(Value::Null),
    );
    if let Some(credits) = provider_config
        .get("creditsPerResult")
        .and_then(Value::as_f64)
    {
        if credits.is_finite() {
            usage.insert(
                "provider_credits_used".into(),
                json!(results.len() as f64 * credits),
            );
        }
    }
    let mut payload = Map::new();
    payload.insert("provider".into(), json!(provider_id));
    payload.insert("query".into(), json!(params.query));
    payload.insert("results".into(), json!(results));
    payload.insert("answer".into(), Value::Null);
    payload.insert("usage".into(), Value::Object(usage));
    if let Some(pagination) = normalized.pagination {
        payload.insert("pagination".into(), pagination);
    }
    payload.insert(
        "metrics".into(),
        json!({
            "response_time_ms": duration_ms,
            "upstream_latency_ms": duration_ms,
            "total_results_available": normalized.total_results,
        }),
    );
    payload.insert("errors".into(), json!([]));
    Value::Object(payload)
}

/// Connections available for a provider, most preferred first.
fn provider_connections(state: &AppState, provider_id: &str) -> Vec<Value> {
    state
        .db
        .provider_connections(Some(provider_id), Some(true))
        .unwrap_or_default()
}

fn pick_credentials(
    state: &AppState,
    provider_id: &str,
    excluded: &HashSet<String>,
) -> Option<Credentials> {
    provider_connections(state, provider_id)
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

/// Upstream `handleSingleProviderSearch`.
async fn handle_single_provider(
    state: &AppState,
    body: &Value,
    provider_input: &str,
) -> Result<Response<Body>, AppError> {
    let entry = providers::provider_entry(provider_input);
    let Some(entry) = entry else {
        return error_response(400, Some(&format!("Unknown provider: {provider_input}")));
    };
    let provider_id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(provider_input)
        .to_string();
    let provider_config = providers::media_config(&provider_id, "search");
    let has_config = provider_config
        .as_object()
        .map(|config| !config.is_empty())
        .unwrap_or(false);
    let has_chat_search = entry.get("searchViaChat").is_some();
    if !has_config && !has_chat_search {
        return error_response(
            400,
            Some(&format!(
                "Provider {provider_id} does not support web search"
            )),
        );
    }
    if !has_config {
        // Upstream would run a chat-based search (`searchViaChat`) here; the
        // native backend has no chat-search adapter yet.
        return provider_error_response(
            501,
            &format!(
                "Provider {provider_id} chat-based web search is not implemented in the native Rust backend"
            ),
        );
    }

    let query = body
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut core_body = Map::new();
    core_body.insert("query".into(), json!(query));
    core_body.insert("provider".into(), json!(provider_id));
    for key in [
        "max_results",
        "search_type",
        "country",
        "language",
        "time_range",
        "offset",
        "domain_filter",
        "content_options",
        "provider_options",
    ] {
        if let Some(value) = body.get(key) {
            if !value.is_null() {
                core_body.insert(key.to_string(), value.clone());
            }
        }
    }
    let core_body = Value::Object(core_body);
    let started = Instant::now();

    let no_auth = entry
        .get("noAuth")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if no_auth {
        let outcome = try_dedicated_provider(
            state,
            &provider_id,
            &provider_config,
            &core_body,
            None,
            started,
        )
        .await;
        return outcome_response(outcome);
    }

    let fallback_provider_id = entry
        .get("credentialFallback")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut excluded: HashSet<String> = HashSet::new();
    let mut last: Option<(u16, String)> = None;
    loop {
        let mut credentials = pick_credentials(state, &provider_id, &excluded);
        if credentials.is_none() {
            if let Some(fallback) = fallback_provider_id.as_deref() {
                credentials = pick_credentials(state, fallback, &excluded);
            }
        }
        let Some(credentials) = credentials else {
            if excluded.is_empty() {
                return error_response(
                    400,
                    Some(&format!("No credentials for provider: {provider_id}")),
                );
            }
            let (status, error) = last.unwrap_or((503, "All accounts unavailable".to_string()));
            return error_response(status, Some(&error));
        };

        let outcome = try_dedicated_provider(
            state,
            &provider_id,
            &provider_config,
            &core_body,
            Some(&credentials),
            started,
        )
        .await;
        match outcome {
            CoreOutcome::Success(data) => return ok_response(data),
            CoreOutcome::Failure { status, error } => {
                // Upstream always falls back to the next connection (the
                // default `checkFallbackError` rule is `shouldFallback: true`).
                excluded.insert(credentials.connection_id);
                last = Some((status, error));
            }
        }
    }
}

fn outcome_response(outcome: CoreOutcome) -> Result<Response<Body>, AppError> {
    match outcome {
        CoreOutcome::Success(data) => ok_response(data),
        CoreOutcome::Failure { status, error } => provider_error_response(status, &error),
    }
}

/// POST /v1/search entry point.
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
    let Some(query) = body.get("query").and_then(Value::as_str) else {
        return error_response(400, Some("Missing required field: query"));
    };
    if query.trim().is_empty() {
        return error_response(400, Some("Missing required field: query"));
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
    use serde_json::json;

    fn config(provider_id: &str) -> Value {
        providers::media_config(provider_id, "search")
    }

    fn params(_provider_id: &str, query: &str, max_results: u64) -> SearchParams {
        SearchParams {
            query: query.to_string(),
            search_type: "web".to_string(),
            max_results,
            token: Some("token-123".to_string()),
            country: None,
            language: None,
            time_range: None,
            offset: None,
            domain_filter: Vec::new(),
            content_options: None,
            provider_options: None,
            provider_specific_data: None,
        }
    }

    #[test]
    fn sanitize_query_matches_upstream_rules() {
        assert_eq!(sanitize_query("  hello   world ").unwrap(), "hello world");
        assert_eq!(
            sanitize_query("bad\u{0}query").unwrap_err(),
            "Query contains invalid control characters"
        );
        assert_eq!(
            sanitize_query("   ").unwrap_err(),
            "Query is empty after normalization"
        );
    }

    #[test]
    fn serper_request_matches_upstream_shape() {
        let built = build_search_request(
            "serper",
            &config("serper"),
            &params("serper", "rust lang", 5),
        )
        .unwrap();
        assert_eq!(built.url, "https://google.serper.dev/search");
        assert_eq!(built.method, Method::POST);
        assert_eq!(
            built.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-API-Key".to_string(), "token-123".to_string())
            ]
        );
        assert_eq!(
            serde_json::from_str::<Value>(built.body.as_deref().unwrap()).unwrap(),
            json!({ "q": "rust lang", "num": 5 })
        );
    }

    #[test]
    fn brave_request_uses_get_with_query_params() {
        let mut p = params("brave-search", "rust lang", 7);
        p.country = Some("US".to_string());
        p.language = Some("en".to_string());
        let built = build_search_request("brave-search", &config("brave-search"), &p).unwrap();
        assert_eq!(
            built.url,
            "https://api.search.brave.com/res/v1/web/search?q=rust+lang&count=7&country=US&search_lang=en"
        );
        assert_eq!(built.method, Method::GET);
        assert_eq!(
            built.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-Subscription-Token".to_string(), "token-123".to_string())
            ]
        );
        assert!(built.body.is_none());
    }

    #[test]
    fn exa_request_includes_domain_filters() {
        let mut p = params("exa", "rust lang", 3);
        p.domain_filter = vec!["rust-lang.org".to_string(), "-reddit.com".to_string()];
        let built = build_search_request("exa", &config("exa"), &p).unwrap();
        assert_eq!(built.url, "https://api.exa.ai/search");
        let body: Value = serde_json::from_str(built.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["numResults"], 3);
        assert_eq!(body["includeDomains"], json!(["rust-lang.org"]));
        assert_eq!(body["excludeDomains"], json!(["reddit.com"]));
        assert_eq!(body["type"], "auto");
    }

    #[test]
    fn tavily_request_uses_bearer_auth() {
        let built = build_search_request("tavily", &config("tavily"), &params("tavily", "rust", 2))
            .unwrap();
        assert_eq!(
            built.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer token-123".to_string())
            ]
        );
        let body: Value = serde_json::from_str(built.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["topic"], "general");
        assert_eq!(body["max_results"], 2);
    }

    #[test]
    fn google_pse_requires_cx_and_caps_num() {
        let mut p = params("google-pse", "rust", 25);
        assert_eq!(
            build_search_request("google-pse", &config("google-pse"), &p).unwrap_err(),
            "Google Programmable Search requires both apiKey and cx"
        );
        p.provider_options = Some(json!({ "cx": "cx-123" }));
        let built = build_search_request("google-pse", &config("google-pse"), &p).unwrap();
        assert_eq!(
            built.url,
            "https://www.googleapis.com/customsearch/v1?key=token-123&cx=cx-123&q=rust&num=10"
        );
        assert_eq!(built.method, Method::GET);
    }

    #[test]
    fn google_pse_maps_time_range_and_offset() {
        let mut p = params("google-pse", "rust", 10);
        p.provider_options = Some(json!({ "cx": "cx-123" }));
        p.time_range = Some("week".to_string());
        p.offset = Some(21.0);
        let built = build_search_request("google-pse", &config("google-pse"), &p).unwrap();
        assert!(built.url.contains("dateRestrict=w1"), "{}", built.url);
        assert!(built.url.contains("start=22"), "{}", built.url);
    }

    #[test]
    fn linkup_uses_depth_override_and_date_window() {
        let mut p = params("linkup", "rust", 4);
        p.provider_options = Some(json!({ "depth": "deep" }));
        p.time_range = Some("day".to_string());
        let built = build_search_request("linkup", &config("linkup"), &p).unwrap();
        assert_eq!(built.url, "https://api.linkup.so/v1/search");
        let body: Value = serde_json::from_str(built.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["depth"], "deep");
        assert_eq!(body["outputType"], "searchResults");
        assert_eq!(body["maxResults"], 4);
        assert!(body["fromDate"].as_str().unwrap().len() == 10);
        assert!(body["toDate"].as_str().unwrap().len() == 10);
    }

    #[test]
    fn searchapi_request_uses_page_number() {
        let mut p = params("searchapi", "rust", 10);
        p.offset = Some(30.0);
        p.country = Some("DE".to_string());
        let built = build_search_request("searchapi", &config("searchapi"), &p).unwrap();
        assert_eq!(
            built.url,
            "https://www.searchapi.io/api/v1/search?engine=google&q=rust&api_key=token-123&gl=de&page=4"
        );
    }

    #[test]
    fn youcom_request_supports_livecrawl_options() {
        let mut p = params("youcom", "rust", 5);
        p.content_options = Some(json!({ "full_page": true, "format": "markdown" }));
        let built = build_search_request("youcom", &config("youcom"), &p).unwrap();
        assert!(built.url.contains("livecrawl=web"), "{}", built.url);
        assert!(
            built.url.contains("livecrawl_formats=markdown"),
            "{}",
            built.url
        );
        assert_eq!(
            built.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-API-Key".to_string(), "token-123".to_string())
            ]
        );
    }

    #[test]
    fn searxng_request_is_unauthenticated_and_uses_search_path() {
        let mut p = params("searxng", "rust", 5);
        p.token = None;
        let built = build_search_request("searxng", &config("searxng"), &p).unwrap();
        assert_eq!(
            built.url,
            "http://localhost:8888/search?q=rust&format=json&categories=general"
        );
        assert_eq!(
            built.headers,
            vec![("Accept".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn xquik_request_validates_query_type() {
        let mut p = params("xquik", "rust", 5);
        p.provider_options = Some(json!({ "queryType": "Bogus" }));
        assert_eq!(
            build_search_request("xquik", &config("xquik"), &p).unwrap_err(),
            "Xquik queryType must be Latest or Top"
        );
        p.provider_options = Some(json!({ "queryType": "Latest", "cursor": "abc" }));
        let built = build_search_request("xquik", &config("xquik"), &p).unwrap();
        assert!(built.url.contains("queryType=Latest"), "{}", built.url);
        assert!(built.url.contains("cursor=abc"), "{}", built.url);
    }

    #[test]
    fn ollama_search_uses_credential_fallback_and_env_override() {
        let mut p = params("ollama-search", "rust", 5);
        p.token = None;
        let built = build_search_request("ollama-search", &config("ollama-search"), &p).unwrap();
        assert_eq!(built.url, "https://ollama.com/api/web_search");
        assert_eq!(
            built.headers,
            vec![("content-type".to_string(), "application/json".to_string())]
        );
        p.provider_options = Some(json!({ "baseUrl": "https://example.com/api/" }));
        let built = build_search_request("ollama-search", &config("ollama-search"), &p).unwrap();
        assert_eq!(built.url, "https://example.com/api");
    }

    #[test]
    fn glm_request_is_json_rpc() {
        let built = build_search_request("glm", &config("glm"), &params("glm", "rust", 5)).unwrap();
        assert_eq!(built.url, "https://api.z.ai/api/mcp/web_search_prime/mcp");
        let body: Value = serde_json::from_str(built.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "web_search_prime");
        assert_eq!(body["params"]["arguments"]["search_query"], "rust");
        assert_eq!(body["params"]["arguments"]["count"], 5);
    }

    #[test]
    fn base_url_override_must_be_public_http() {
        let mut p = params("tavily", "rust", 5);
        p.provider_options = Some(json!({ "baseUrl": "ftp://example.com/x" }));
        let error = build_search_request("tavily", &config("tavily"), &p).unwrap_err();
        assert_eq!(error, "Invalid baseUrl protocol: ftp:");
        p.provider_options = Some(json!({ "baseUrl": "http://127.0.0.1:9000/v1" }));
        let error = build_search_request("tavily", &config("tavily"), &p).unwrap_err();
        assert_eq!(error, "Blocked URL: internal host");
        p.provider_options = Some(json!({ "baseUrl": "not a url" }));
        let error = build_search_request("tavily", &config("tavily"), &p).unwrap_err();
        assert_eq!(error, "Invalid baseUrl: not a url");
    }

    #[test]
    fn page_number_matches_upstream_math() {
        assert_eq!(to_page_number(Some(30.0), 10), Some(4));
        assert_eq!(to_page_number(Some(0.0), 10), None);
        assert_eq!(to_page_number(None, 10), None);
        assert_eq!(to_page_number(Some(10.0), 0), None);
    }

    #[test]
    fn brave_fixture_normalizes_web_results() {
        let fixture = json!({
            "web": {
                "totalCount": 42,
                "results": [
                    {
                        "title": "Rust Programming Language",
                        "url": "https://www.rust-lang.org/?utm_source=brave",
                        "description": "A language empowering everyone",
                        "page_age": "2024-01-02T03:04:05",
                        "meta_url": { "favicon": "https://www.rust-lang.org/favicon.ico" }
                    }
                ]
            }
        });
        let normalized = normalize_search_response("brave-search", &fixture, "web");
        assert_eq!(normalized.total_results, json!(42));
        let first = &normalized.results[0];
        assert_eq!(first["title"], "Rust Programming Language");
        assert_eq!(first["url"], "https://www.rust-lang.org/?utm_source=brave");
        assert_eq!(first["display_url"], "rust-lang.org/");
        assert_eq!(first["snippet"], "A language empowering everyone");
        assert_eq!(first["position"], 1);
        assert_eq!(first["citation"]["provider"], "brave-search");
        assert_eq!(first["citation"]["rank"], 1);
        assert_eq!(
            first["favicon_url"],
            "https://www.rust-lang.org/favicon.ico"
        );
        assert_eq!(first["score"], Value::Null);
        assert_eq!(first["content"], Value::Null);
        assert!(first["citation"]["retrieved_at"].as_str().is_some());
    }

    #[test]
    fn brave_fixture_normalizes_news_results() {
        let fixture = json!({
            "news": {
                "results": [
                    { "title": "Release", "url": "https://example.com/news", "description": "shipped" }
                ]
            }
        });
        let normalized = normalize_search_response("brave-search", &fixture, "news");
        assert_eq!(normalized.results.len(), 1);
        assert_eq!(normalized.results[0]["title"], "Release");
        assert_eq!(normalized.total_results, Value::Null);
    }

    #[test]
    fn serper_fixture_reports_total_results() {
        let fixture = json!({
            "searchParameters": { "totalResults": 1234 },
            "organic": [
                { "title": "Docs", "link": "https://docs.rs/serde", "snippet": "serde", "date": "2024-05-06" }
            ]
        });
        let normalized = normalize_search_response("serper", &fixture, "web");
        assert_eq!(normalized.total_results, json!(1234));
        assert_eq!(normalized.results[0]["url"], "https://docs.rs/serde");
        assert_eq!(normalized.results[0]["published_at"], "2024-05-06");
    }

    #[test]
    fn exa_fixture_uses_highlights_and_scores() {
        let fixture = json!({
            "results": [
                {
                    "title": "Async Rust",
                    "url": "https://tokio.rs/",
                    "highlights": ["first highlight"],
                    "score": 0.75,
                    "publishedDate": "2023-09-01",
                    "text": "full body"
                }
            ]
        });
        let normalized = normalize_search_response("exa", &fixture, "web");
        let first = &normalized.results[0];
        assert_eq!(first["snippet"], "first highlight");
        assert_eq!(first["score"], 0.75);
        assert_eq!(first["published_at"], "2023-09-01");
        assert_eq!(first["content"]["format"], "text");
        assert_eq!(first["content"]["text"], "full body");
        assert_eq!(first["content"]["length"], 9);
        assert_eq!(normalized.total_results, json!(1));
    }

    #[test]
    fn tavily_fixture_normalizes_raw_content() {
        let fixture = json!({
            "results": [
                { "title": "T", "url": "https://example.com", "content": "snippet", "score": 1.4, "raw_content": "raw" }
            ]
        });
        let normalized = normalize_search_response("tavily", &fixture, "web");
        let first = &normalized.results[0];
        assert_eq!(first["snippet"], "snippet");
        assert_eq!(first["score"], 1.0);
        assert_eq!(first["content"]["text"], "raw");
    }

    #[test]
    fn google_pse_fixture_parses_string_total_results() {
        let fixture = json!({
            "searchInformation": { "totalResults": "42" },
            "items": [ { "title": "G", "link": "https://example.com/g", "snippet": "s" } ]
        });
        let normalized = normalize_search_response("google-pse", &fixture, "web");
        assert_eq!(normalized.total_results, json!(42));
        assert_eq!(normalized.results[0]["url"], "https://example.com/g");
    }

    #[test]
    fn youcom_fixture_uses_livecrawl_markdown() {
        let fixture = json!({
            "results": {
                "web": [
                    {
                        "title": "Y",
                        "url": "https://example.com/y",
                        "snippets": ["snip"],
                        "markdown": "# heading",
                        "page_age": "2d",
                        "thumbnail_url": "https://example.com/thumb.png"
                    }
                ]
            }
        });
        let normalized = normalize_search_response("youcom", &fixture, "web");
        let first = &normalized.results[0];
        assert_eq!(first["snippet"], "snip");
        assert_eq!(first["content"]["format"], "markdown");
        assert_eq!(first["content"]["text"], "# heading");
        assert_eq!(first["metadata"]["source_type"], "web");
        assert_eq!(
            first["metadata"]["image_url"],
            "https://example.com/thumb.png"
        );
    }

    #[test]
    fn searxng_fixture_joins_engines() {
        let fixture = json!({
            "results": [
                { "title": "S", "url": "https://example.com/s", "content": "c", "engines": ["google", "bing"] }
            ]
        });
        let normalized = normalize_search_response("searxng", &fixture, "web");
        assert_eq!(
            normalized.results[0]["metadata"]["source_type"],
            "google, bing"
        );
    }

    #[test]
    fn glm_fixture_unwraps_json_rpc_text() {
        let fixture = json!({
            "result": {
                "content": [
                    { "text": "{\"results\":[{\"title\":\"G\",\"link\":\"https://example.com/g\",\"content\":\"c\"}]}" }
                ]
            }
        });
        let normalized = normalize_search_response("glm", &fixture, "web");
        assert_eq!(normalized.results.len(), 1);
        assert_eq!(normalized.results[0]["url"], "https://example.com/g");
        assert_eq!(normalized.results[0]["snippet"], "c");
    }

    #[test]
    fn xquik_fixture_builds_tweet_urls_and_pagination() {
        let fixture = json!({
            "tweets": [
                {
                    "id": 1234567890,
                    "text": "hello world",
                    "createdAt": "2024-02-02T00:00:00Z",
                    "author": { "username": "rustlang", "name": "Rust" },
                    "media": [ { "mediaUrl": "https://pbs.twimg.com/media/x.jpg" } ]
                }
            ],
            "has_next_page": true,
            "next_cursor": "cursor-1"
        });
        let normalized = normalize_search_response("xquik", &fixture, "web");
        let first = &normalized.results[0];
        assert_eq!(first["title"], "@rustlang on X");
        assert_eq!(first["url"], "https://x.com/rustlang/status/1234567890");
        assert_eq!(first["snippet"], "hello world");
        assert_eq!(first["metadata"]["author"], "@rustlang");
        assert_eq!(first["metadata"]["source_type"], "x_post");
        assert_eq!(normalized.total_results, Value::Null);
        assert_eq!(
            normalized.pagination,
            Some(json!({ "has_more": true, "next_cursor": "cursor-1" }))
        );
    }

    #[test]
    fn ollama_search_fixture_accepts_array_payloads() {
        let fixture = json!([
            { "title": "O", "url": "https://example.com/o", "content": "body", "source": "web" }
        ]);
        let normalized = normalize_search_response("ollama-search", &fixture, "web");
        assert_eq!(normalized.results[0]["content"]["text"], "body");
        assert_eq!(normalized.results[0]["metadata"]["source_type"], "web");
    }

    #[test]
    fn unknown_provider_normalizes_to_empty_results() {
        let normalized = normalize_search_response("perplexity", &json!({ "results": [1] }), "web");
        assert!(normalized.results.is_empty());
        assert_eq!(normalized.total_results, Value::Null);
    }

    #[test]
    fn success_payload_matches_upstream_envelope() {
        let fixture = json!({
            "web": {
                "totalCount": 7,
                "results": [
                    { "title": "One", "url": "https://example.com/1", "description": "first" },
                    { "title": "Two", "url": "https://example.com/2", "description": "second" },
                    { "title": "Three", "url": "https://example.com/3", "description": "third" }
                ]
            }
        });
        let mut p = params("brave-search", "rust lang", 2);
        p.country = Some("US".to_string());
        let payload = success_payload("brave-search", &config("brave-search"), &p, &fixture, 42);
        assert_eq!(payload["provider"], "brave-search");
        assert_eq!(payload["query"], "rust lang");
        assert_eq!(payload["answer"], Value::Null);
        assert_eq!(payload["errors"], json!([]));
        // `max_results` truncates the normalised list.
        assert_eq!(payload["results"].as_array().unwrap().len(), 2);
        assert_eq!(payload["results"][0]["title"], "One");
        assert_eq!(payload["usage"]["queries_used"], 1);
        assert_eq!(payload["usage"]["search_cost_usd"], 0.005);
        assert!(payload["usage"].get("provider_credits_used").is_none());
        assert_eq!(payload["metrics"]["response_time_ms"], 42);
        assert_eq!(payload["metrics"]["upstream_latency_ms"], 42);
        assert_eq!(payload["metrics"]["total_results_available"], 7);
        assert!(payload.get("pagination").is_none());
    }

    #[test]
    fn success_payload_reports_credits_and_pagination_when_present() {
        let fixture = json!({
            "tweets": [
                { "id": "1", "text": "hi", "author": { "username": "u" } }
            ],
            "has_next_page": true,
            "next_cursor": "c1"
        });
        let p = params("xquik", "rust", 5);
        let payload = success_payload("xquik", &config("xquik"), &p, &fixture, 7);
        assert_eq!(payload["usage"]["provider_credits_used"], 1.0);
        assert_eq!(
            payload["pagination"],
            json!({ "has_more": true, "next_cursor": "c1" })
        );
        assert_eq!(payload["metrics"]["total_results_available"], Value::Null);
    }
}
