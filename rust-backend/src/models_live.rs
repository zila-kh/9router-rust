//! Per-provider live model resolvers, ported from upstream
//! `open-sse/services/{kiroModels,kimchiModels,clinepassModels,grokCliModels}.js`
//! and consumed by `GET /v1/models` exactly like upstream's
//! `LIVE_MODEL_RESOLVERS` map: a resolver returns the live catalog for one
//! active connection, or `None` so the caller keeps the static
//! `PROVIDER_MODELS` list.
//!
//! ## What is ported
//!
//! | provider | upstream source | notes |
//! | --- | --- | --- |
//! | `kiro` | `kiroModels.js` | `ListAvailableModels` + `-thinking`/`-agentic` variant expansion |
//! | `kimchi` | `kimchiModels.js` | `/v1/models/metadata?include_in_cli=true` + metadata normalisation |
//! | `clinepass` | `clinepassModels.js` | `cline-pass/` prefix filter |
//! | `cline` | `clinepassModels.js` | unfiltered Cline catalog |
//! | `grok-cli` | `grokCliModels.js` | catalog GET + entry normalisation |
//!
//! ## What is declared, not ported
//!
//! * `qoder`, `github` (Copilot), `cursor` and `zed` — see
//!   [`UNPORTED_LIVE_RESOLVERS`]. Each needs a provider-specific token
//!   exchange/refresh (`copilotToken`, Qoder PAT signing, Cursor HTTP/2
//!   protobuf, Zed auth) that the Rust backend does not implement; `/v1/models`
//!   falls back to the static catalog for them, exactly as upstream does when a
//!   resolver fails.
//! * Token refresh on `401`/`403` inside the ported resolvers (`kiro`,
//!   `grok-cli`). A valid stored token resolves live; an expired one falls back
//!   to the static catalog instead of refreshing first.
//! * Upstream's `.js` per-credential 5-minute catalog caches. Every call
//!   resolves live, which is the same result the TTL expiry produces (only the
//!   request count differs) — the resolvers stay stateless.
//! * Per-connection proxy routing (`resolveConnectionProxyConfig`): the ported
//!   resolvers use the shared HTTP client, so a connection-scoped proxy is not
//!   applied to model discovery.
//!
//! Every failure path returns `None` (upstream logs a warning and continues);
//! no resolver error can fail the `/v1/models` response.

use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::state::AppState;

/// Upstream resolvers that are intentionally not ported (see module docs).
pub const UNPORTED_LIVE_RESOLVERS: [&str; 4] = ["qoder", "github", "cursor", "zed"];

/// True when upstream resolves this provider live but the Rust backend does
/// not, so `/v1/models` serves the static catalog for it.
pub fn is_unported_live_resolver(provider_id: &str) -> bool {
    UNPORTED_LIVE_RESOLVERS.contains(&provider_id)
}

const KIRO_RUNTIME_SDK_VERSION: &str = "1.0.0";
const KIRO_AGENT_OS: &str = "windows";
const KIRO_AGENT_OS_VERSION: &str = "10.0.26200";
const KIRO_NODE_VERSION: &str = "22.21.1";
const KIRO_VERSION: &str = "0.10.32";
const KIRO_DEFAULT_REGION: &str = "us-east-1";
const KIRO_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

const KIMCHI_API: &str = "https://llm.kimchi.dev";
const KIMCHI_USER_AGENT: &str = "kimchi/0.1.40";
const KIMCHI_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

const CLINE_MODELS_ENDPOINT: &str = "https://api.cline.bot/api/v1/models";
const CLINE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

const GROK_CLI_VERSION: &str = "0.2.99";
const GROK_CLI_MODEL: &str = "grok-build";
const GROK_CLI_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const GROK_CLI_CLIENT_IDENTIFIER: &str = "grok-shell";

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

/// JS `Number(value)` for the upstream coercions: numbers pass through,
/// numeric strings are parsed, everything else is `NaN`.
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// JSON numbers the way upstream serializes them: integral doubles have no
/// fractional part (`500000`, not `500000.0`).
fn number_json(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn psd<'a>(conn: &'a Value, key: &str) -> Option<&'a Value> {
    conn.get("providerSpecificData")?.get(key)
}

/// `resolve_live_models(providerId, conn)` — upstream `LIVE_MODEL_RESOLVERS`.
pub async fn resolve_live_models(
    state: &AppState,
    provider_id: &str,
    conn: &Value,
) -> Option<Vec<Value>> {
    match provider_id {
        "kiro" => resolve_kiro_models(state, conn).await,
        "kimchi" => resolve_kimchi_models(state, conn).await,
        "clinepass" => resolve_clinepass_models(state, conn).await,
        "cline" => resolve_cline_models(state, conn).await,
        "grok-cli" => resolve_grok_cli_models(state, conn).await,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Kiro — upstream `kiroModels.js`
// ---------------------------------------------------------------------------

/// `stripSyntheticSuffixes` — used for display naming only.
fn kiro_strip_synthetic_suffixes(id: &str) -> String {
    let mut out = id.to_string();
    if let Some(stripped) = out.clone().strip_suffix("-agentic") {
        out = stripped.to_string();
    }
    if let Some(stripped) = out.clone().strip_suffix("-thinking") {
        out = stripped.to_string();
    }
    out
}

/// `regionFromProfileArn` — `arn:aws:codewhisperer:us-east-1:...:profile/X`.
fn kiro_region_from_profile_arn(profile_arn: Option<&str>) -> String {
    let Some(profile_arn) = profile_arn.filter(|value| !value.is_empty()) else {
        return KIRO_DEFAULT_REGION.to_string();
    };
    let parts: Vec<&str> = profile_arn.split(':').collect();
    if parts.len() >= 4 && !parts[3].is_empty() {
        return parts[3].to_string();
    }
    KIRO_DEFAULT_REGION.to_string()
}

/// `buildKiroFingerprintHeaders` — the machine fingerprint Kiro upstream
/// validates; identical seeding to upstream so one account keeps one machineId.
fn kiro_fingerprint_headers(conn: &Value) -> Vec<(HeaderName, HeaderValue)> {
    let seed = psd(conn, "clientId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| text(conn.get("refreshToken")).filter(|value| !value.is_empty()))
        .or_else(|| {
            psd(conn, "profileArn")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| text(conn.get("accessToken")).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "kiro-anonymous".to_string());
    let machine_id = hex_sha256(&seed);

    let user_agent = format!(
        "aws-sdk-js/{KIRO_RUNTIME_SDK_VERSION} ua/2.1 os/{KIRO_AGENT_OS}#{KIRO_AGENT_OS_VERSION} \
         lang/js md/nodejs#{KIRO_NODE_VERSION} api/codewhispererruntime#{KIRO_RUNTIME_SDK_VERSION} \
         m/N,E KiroIDE-{KIRO_VERSION}-{machine_id}"
    );
    let amz_user_agent =
        format!("aws-sdk-js/{KIRO_RUNTIME_SDK_VERSION} KiroIDE-{KIRO_VERSION}-{machine_id}");

    [
        ("user-agent", user_agent),
        ("x-amz-user-agent", amz_user_agent),
        ("x-amzn-kiro-agent-mode", "vibe".to_string()),
        ("x-amzn-codewhisperer-optout", "true".to_string()),
        ("amz-sdk-request", "attempt=1; max=1".to_string()),
        ("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string()),
        ("accept", "application/json".to_string()),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        Some((
            HeaderName::from_bytes(name.as_bytes()).ok()?,
            HeaderValue::from_str(&value).ok()?,
        ))
    })
    .collect()
}

fn hex_sha256(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `buildVariants` — the synthetic `-thinking`/`-agentic` pairs. `auto` skips
/// the agentic variants because Kiro picks the model server-side.
fn kiro_build_variants(upstream: &str, display_name: &str) -> Vec<Value> {
    let safe_upstream = kiro_strip_synthetic_suffixes(upstream);
    let display = if display_name.is_empty() {
        format!("Kiro {safe_upstream}")
    } else {
        display_name.to_string()
    };
    let is_auto = safe_upstream == "auto";

    let mut variants = vec![
        json!({
            "id": safe_upstream,
            "name": display,
            "capabilities": {"thinking": false, "agentic": false},
        }),
        json!({
            "id": format!("{safe_upstream}-thinking"),
            "name": format!("{display} (Thinking)"),
            "capabilities": {"thinking": true, "agentic": false},
        }),
    ];
    if !is_auto {
        variants.push(json!({
            "id": format!("{safe_upstream}-agentic"),
            "name": format!("{display} (Agentic)"),
            "capabilities": {"thinking": false, "agentic": true},
        }));
        variants.push(json!({
            "id": format!("{safe_upstream}-thinking-agentic"),
            "name": format!("{display} (Thinking + Agentic)"),
            "capabilities": {"thinking": true, "agentic": true},
        }));
    }
    variants
}

/// `formatDisplayName` — `Kiro <name>` and a `(1.5x credit)` suffix when the
/// account's rate multiplier is not 1.0.
fn kiro_format_display_name(
    model_name: Option<&str>,
    model_id: &str,
    rate_multiplier: &Value,
) -> String {
    let base = model_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(model_id);
    let base = if base.trim().is_empty() {
        "Kiro".to_string()
    } else {
        base.trim().to_string()
    };
    let rate = js_number(rate_multiplier);
    match rate {
        Some(rate) if rate.is_finite() && (rate - 1.0).abs() >= 1e-9 && rate > 0.0 => {
            format!("Kiro {base} ({rate:.1}x credit)")
        }
        _ => format!("Kiro {base}"),
    }
}

/// `fetchKiroCatalogRaw` — `GET https://q.{region}.amazonaws.com/ListAvailableModels`.
fn kiro_catalog_url(profile_arn: Option<&str>) -> String {
    let region = kiro_region_from_profile_arn(profile_arn);
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("origin", "AI_EDITOR");
    if let Some(profile_arn) = profile_arn.filter(|value| !value.is_empty()) {
        serializer.append_pair("profileArn", profile_arn);
    }
    format!(
        "https://q.{region}.amazonaws.com/ListAvailableModels?{}",
        serializer.finish()
    )
}

/// `resolveKiroModels` — live Kiro catalog, expanded into 9router variants.
/// Upstream refreshes on 401 and retries; the ported version returns `None`
/// instead (declared in the module docs).
pub async fn resolve_kiro_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let access_token = text(conn.get("accessToken")).filter(|value| !value.is_empty())?;
    let profile_arn = psd(conn, "profileArn").and_then(Value::as_str);
    let url = kiro_catalog_url(profile_arn);

    let mut request = state
        .http
        .get(url)
        .timeout(KIRO_FETCH_TIMEOUT)
        .header("authorization", format!("Bearer {access_token}"));
    for (name, value) in kiro_fingerprint_headers(conn) {
        request = request.header(name, value);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = response.json().await.ok()?;
    let raw = data.get("models").and_then(Value::as_array)?;

    let mut expanded = Vec::new();
    for model in raw {
        let upstream_id = model
            .get("modelId")
            .or_else(|| model.get("id"))
            .and_then(Value::as_str);
        let Some(upstream_id) = upstream_id else {
            continue;
        };
        let model_name = model.get("modelName").and_then(Value::as_str);
        let rate_multiplier = model.get("rateMultiplier").cloned().unwrap_or(Value::Null);
        let display = kiro_format_display_name(model_name, upstream_id, &rate_multiplier);
        let context_length = model
            .get("tokenLimits")
            .and_then(|limits| limits.get("maxInputTokens"))
            .and_then(js_number)
            .filter(|value| *value != 0.0)
            .map(number_json)
            .unwrap_or_else(|| json!(200_000));
        let rate = js_number(&rate_multiplier)
            .filter(|value| value.is_finite())
            .map(number_json)
            .unwrap_or_else(|| json!(1.0));
        for variant in kiro_build_variants(upstream_id, &display) {
            let mut entry = variant.as_object().cloned().unwrap_or_default();
            entry.insert("contextLength".into(), context_length.clone());
            entry.insert("rateMultiplier".into(), rate.clone());
            entry.insert("upstreamModelId".into(), json!(upstream_id));
            entry.insert(
                "description".into(),
                json!(model
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")),
            );
            expanded.push(Value::Object(entry));
        }
    }

    (!expanded.is_empty()).then_some(expanded)
}

// ---------------------------------------------------------------------------
// Kimchi — upstream `kimchiModels.js`
// ---------------------------------------------------------------------------

fn kimchi_normalize_endpoint(endpoint: Option<&str>) -> String {
    let raw = endpoint.unwrap_or("").trim().to_string();
    let raw = if raw.is_empty() {
        KIMCHI_API.to_string()
    } else {
        raw
    };
    raw.trim_end_matches('/').to_string()
}

/// `buildKimchiModelsUrl`.
pub fn kimchi_models_url(endpoint: Option<&str>) -> String {
    format!(
        "{}/v1/models/metadata?include_in_cli=true",
        kimchi_normalize_endpoint(endpoint)
    )
}

/// `readToken` — access token, then API key from the row or nested data.
fn kimchi_token(conn: &Value) -> Option<String> {
    text(conn.get("accessToken"))
        .filter(|value| !value.is_empty())
        .or_else(|| text(conn.get("apiKey")).filter(|value| !value.is_empty()))
        .or_else(|| {
            psd(conn, "apiKey")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

/// `normalizeKimchiModel` — the subset `/v1/models` consumes (id, kind,
/// capabilities, token limits).
pub fn normalize_kimchi_model(item: &Value) -> Option<Value> {
    if !item.is_object() {
        return None;
    }
    let id = ["slug", "id", "model", "name"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();

    let input_modalities: Vec<&str> = item
        .get("input_modalities")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| *value == "text" || *value == "image")
                .collect()
        })
        .unwrap_or_default();
    let limits = item.get("limits").filter(|value| value.is_object());
    let context_length = limits
        .and_then(|limits| limits.get("context_window"))
        .or_else(|| item.get("contextLength"))
        .or_else(|| item.get("context_length"))
        .and_then(js_number)
        .filter(|value| *value != 0.0);
    let max_output_tokens = limits
        .and_then(|limits| limits.get("max_output_tokens"))
        .or_else(|| item.get("maxOutputTokens"))
        .or_else(|| item.get("max_output_tokens"))
        .and_then(js_number)
        .filter(|value| *value != 0.0);
    let upstream_provider = item
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reasoning = item
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let kind = if input_modalities.contains(&"image") {
        "imageToText"
    } else {
        "llm"
    };
    let name = ["display_name", "displayName", "name"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .unwrap_or(&id)
        .trim()
        .to_string();

    let mut capabilities = Map::new();
    capabilities.insert("vision".into(), json!(input_modalities.contains(&"image")));
    capabilities.insert("reasoning".into(), json!(reasoning));
    if let Some(context_length) = context_length {
        capabilities.insert("contextWindow".into(), number_json(context_length));
    }
    if let Some(max_output_tokens) = max_output_tokens {
        capabilities.insert("maxOutput".into(), number_json(max_output_tokens));
    }
    if !upstream_provider.is_empty() {
        capabilities.insert("upstreamProvider".into(), json!(upstream_provider));
    }

    let mut model = item.as_object().cloned().unwrap_or_default();
    model.insert("id".into(), json!(id));
    model.insert("name".into(), json!(name));
    model.insert("provider".into(), json!(upstream_provider));
    model.insert("upstreamProvider".into(), json!(upstream_provider));
    model.insert("reasoning".into(), json!(reasoning));
    model.insert("inputModalities".into(), json!(input_modalities));
    model.insert("kind".into(), json!(kind));
    model.insert("type".into(), json!(kind));
    model.insert("capabilities".into(), Value::Object(capabilities));
    if let Some(context_length) = context_length {
        model.insert("contextLength".into(), number_json(context_length));
    }
    if let Some(max_output_tokens) = max_output_tokens {
        model.insert("maxOutputTokens".into(), number_json(max_output_tokens));
    }
    if upstream_provider == "anthropic" {
        model.insert(
            "compat".into(),
            json!({"supportsReasoningEffort": false, "cacheControlFormat": "anthropic"}),
        );
    }
    Some(Value::Object(model))
}

/// `resolveKimchiModels`.
pub async fn resolve_kimchi_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let token = kimchi_token(conn)?;
    let endpoint = psd(conn, "kimchiEndpoint").and_then(Value::as_str);
    let url = kimchi_models_url(endpoint);
    let response = state
        .http
        .get(url)
        .timeout(KIMCHI_FETCH_TIMEOUT)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", KIMCHI_USER_AGENT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = response.json().await.ok()?;
    let raw = data.get("models").and_then(Value::as_array)?;
    let models: Vec<Value> = raw.iter().filter_map(normalize_kimchi_model).collect();
    (!models.is_empty()).then_some(models)
}

// ---------------------------------------------------------------------------
// Cline / ClinePass — upstream `clinepassModels.js`
// ---------------------------------------------------------------------------

/// `getClineAccessToken` — OAuth JWTs carry the WorkOS `workos:` prefix, API
/// keys (`clp_…`) are sent verbatim.
fn cline_access_token(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_lowercase().starts_with("workos:") {
        return trimmed.to_string();
    }
    if WORKOS_JWT.is_match(trimmed) {
        format!("workos:{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Upstream `/^eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/` — Cline OAuth access tokens
/// are WorkOS JWTs (`eyJ…` base64url header).
static WORKOS_JWT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").expect("jwt regex"));

/// `buildClineHeaders` with the `Accept: application/json` extra the model-list
/// caller passes.
fn cline_headers(token: &str, is_api_key: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let authorization = if is_api_key {
        format!("Bearer {}", token.trim())
    } else {
        format!("Bearer {}", cline_access_token(token))
    };
    let version = env!("CARGO_PKG_VERSION");
    let user_agent = format!("9Router/{version}");
    for (name, value) in [
        ("http-referer", "https://cline.bot".to_string()),
        ("x-title", "Cline".to_string()),
        ("user-agent", user_agent.clone()),
        ("x-platform", std::env::consts::OS.to_string()),
        ("x-platform-version", "rust".to_string()),
        ("x-client-type", "9router".to_string()),
        ("x-client-version", version.to_string()),
        ("x-core-version", version.to_string()),
        ("x-is-multiroot", "false".to_string()),
        ("accept", "application/json".to_string()),
        ("authorization", authorization),
    ] {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

async fn fetch_cline_raw_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let is_api_key = conn
        .get("apiKey")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let token = if is_api_key {
        text(conn.get("apiKey"))
    } else {
        text(conn.get("accessToken"))
    }
    .filter(|value| !value.is_empty())?;

    let response = state
        .http
        .get(CLINE_MODELS_ENDPOINT)
        .timeout(CLINE_FETCH_TIMEOUT)
        .headers(cline_headers(&token, is_api_key))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = response.json().await.ok()?;
    let raw = if data.is_array() {
        data.as_array().cloned()
    } else {
        data.get("data").and_then(Value::as_array).cloned()
    }?;
    Some(raw)
}

/// `resolveClinepassModels` — only `cline-pass/`-prefixed ids.
pub async fn resolve_clinepass_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let raw = fetch_cline_raw_models(state, conn).await?;
    let models: Vec<Value> = raw
        .iter()
        .filter(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("cline-pass/"))
        })
        .map(|model| {
            let id = model.get("id").and_then(Value::as_str).unwrap_or("");
            json!({"id": id, "name": model.get("name").and_then(Value::as_str).unwrap_or(id)})
        })
        .collect();
    (!models.is_empty()).then_some(models)
}

/// `resolveClineModels` — the unfiltered catalog.
pub async fn resolve_cline_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let raw = fetch_cline_raw_models(state, conn).await?;
    let models: Vec<Value> = raw
        .iter()
        .filter(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
        })
        .map(|model| {
            let id = model.get("id").and_then(Value::as_str).unwrap_or("");
            json!({"id": id, "name": model.get("name").and_then(Value::as_str).unwrap_or(id)})
        })
        .collect();
    (!models.is_empty()).then_some(models)
}

// ---------------------------------------------------------------------------
// Grok CLI — upstream `grokCliModels.js`
// ---------------------------------------------------------------------------

/// `parseGrokCliModels` — accepts arrays, `{data|models|results}` wrappers and
/// id-keyed objects; dedupes by id.
pub fn parse_grok_cli_models(data: &Value) -> Vec<Value> {
    let value = if data.is_array() {
        data.clone()
    } else {
        ["data", "models", "results"]
            .iter()
            .find_map(|key| data.get(*key).filter(|value| !value.is_null()))
            .cloned()
            .unwrap_or_else(|| json!([]))
    };
    let entries: Vec<(Option<String>, Value)> = if let Some(items) = value.as_array() {
        items.iter().map(|item| (None, item.clone())).collect()
    } else if let Some(map) = value.as_object() {
        map.iter()
            .map(|(key, item)| (Some(key.clone()), item.clone()))
            .collect()
    } else {
        Vec::new()
    };

    let mut seen = Vec::new();
    let mut models = Vec::new();
    for (key, raw) in entries {
        let item = if raw.is_string() {
            json!({"id": raw})
        } else if raw.is_object() {
            raw
        } else {
            continue;
        };
        let id = ["id", "model_id", "modelId", "model", "slug"]
            .iter()
            .find_map(|field| item.get(*field).and_then(Value::as_str))
            .map(str::to_string)
            .or(key)
            .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_string))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let Some(id) = id else { continue };
        if seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());

        let mut model = item.as_object().cloned().unwrap_or_default();
        model.insert("id".into(), json!(id));
        let name = ["display_name", "displayName", "name"]
            .iter()
            .find_map(|field| item.get(*field).and_then(Value::as_str))
            .unwrap_or(&id)
            .to_string();
        model.insert("name".into(), json!(name));
        let context_length = [
            "context_length",
            "contextLength",
            "context_window",
            "contextWindow",
        ]
        .iter()
        .find_map(|field| item.get(*field))
        .and_then(js_number)
        .filter(|value| value.is_finite() && *value > 0.0);
        let max_output = ["max_output_tokens", "maxOutputTokens"]
            .iter()
            .find_map(|field| item.get(*field))
            .and_then(js_number)
            .filter(|value| value.is_finite() && *value > 0.0);
        if let Some(context_length) = context_length {
            model.insert("contextLength".into(), number_json(context_length));
        }
        if let Some(max_output) = max_output {
            model.insert("maxOutputTokens".into(), number_json(max_output));
        }
        if id == GROK_CLI_MODEL {
            let context_length = context_length.unwrap_or(500_000.0);
            let max_output = max_output.unwrap_or(64_000.0);
            model.insert("contextLength".into(), number_json(context_length));
            model.insert("maxOutputTokens".into(), number_json(max_output));
        }
        models.push(Value::Object(model));
    }
    models
}

fn grok_cli_headers(access_token: &str, provider_specific_data: Option<&Value>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let user_agent = format!("grok-shell/{GROK_CLI_VERSION} (linux; x86_64)");
    let mut pairs = vec![
        ("authorization", format!("Bearer {access_token}")),
        ("accept", "application/json".to_string()),
        ("user-agent", user_agent),
        ("x-xai-token-auth", "xai-grok-cli".to_string()),
        ("x-grok-client-version", GROK_CLI_VERSION.to_string()),
        (
            "x-grok-client-identifier",
            GROK_CLI_CLIENT_IDENTIFIER.to_string(),
        ),
        ("x-grok-client-mode", "headless".to_string()),
    ];
    if let Some(email) = provider_specific_data
        .and_then(|data| data.get("email"))
        .and_then(Value::as_str)
    {
        pairs.push(("x-email", email.to_string()));
    }
    if let Some(user_id) = provider_specific_data
        .and_then(|data| data.get("userId").or_else(|| data.get("principalId")))
        .and_then(Value::as_str)
    {
        pairs.push(("x-userid", user_id.to_string()));
    }
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// `resolveGrokCliModels` — catalog GET. Upstream refreshes on 401/403 and
/// retries; the ported version returns `None` instead (declared in the module
/// docs).
pub async fn resolve_grok_cli_models(state: &AppState, conn: &Value) -> Option<Vec<Value>> {
    let access_token = text(conn.get("accessToken")).filter(|value| !value.is_empty())?;
    let response = state
        .http
        .get(format!("{GROK_CLI_BASE_URL}/models"))
        .headers(grok_cli_headers(
            &access_token,
            conn.get("providerSpecificData"),
        ))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = response.json().await.ok()?;
    let models = parse_grok_cli_models(&data);
    (!models.is_empty()).then_some(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cline_oauth_tokens_get_the_workos_prefix_only_for_jwts() {
        assert_eq!(cline_access_token("workos:abc"), "workos:abc");
        assert_eq!(
            cline_access_token("eyJhbGciOiJIUzI1NiJ9.payload.sig"),
            "workos:eyJhbGciOiJIUzI1NiJ9.payload.sig"
        );
        // ClinePass API keys must stay verbatim
        assert_eq!(cline_access_token("clp_12345"), "clp_12345");
    }

    #[test]
    fn kiro_variants_match_upstream_including_auto() {
        let variants = kiro_build_variants("claude-sonnet-4.5", "Kiro Claude Sonnet 4.5");
        let ids: Vec<&str> = variants
            .iter()
            .filter_map(|variant| variant["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "claude-sonnet-4.5",
                "claude-sonnet-4.5-thinking",
                "claude-sonnet-4.5-agentic",
                "claude-sonnet-4.5-thinking-agentic"
            ]
        );
        assert_eq!(variants[3]["capabilities"]["thinking"], true);
        assert_eq!(variants[3]["capabilities"]["agentic"], true);

        let auto = kiro_build_variants("auto", "Kiro Auto");
        assert_eq!(auto.len(), 2, "auto has no agentic variants");
    }

    #[test]
    fn kiro_display_names_carry_rate_multipliers() {
        assert_eq!(
            kiro_format_display_name(Some("Sonnet 4.5"), "claude-sonnet-4.5", &json!(1.0)),
            "Kiro Sonnet 4.5"
        );
        assert_eq!(
            kiro_format_display_name(Some("Sonnet 4.5"), "claude-sonnet-4.5", &json!(1.3)),
            "Kiro Sonnet 4.5 (1.3x credit)"
        );
        assert_eq!(
            kiro_format_display_name(None, "claude-sonnet-4.5", &Value::Null),
            "Kiro claude-sonnet-4.5"
        );
    }

    #[test]
    fn kiro_region_comes_from_the_profile_arn() {
        assert_eq!(kiro_region_from_profile_arn(None), "us-east-1");
        assert_eq!(
            kiro_region_from_profile_arn(Some("arn:aws:codewhisperer:eu-west-1:1:profile/A")),
            "eu-west-1"
        );
        assert_eq!(kiro_region_from_profile_arn(Some("garbage")), "us-east-1");
        assert_eq!(
            kiro_catalog_url(Some("arn:aws:codewhisperer:eu-west-1:1:profile/A")),
            "https://q.eu-west-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR&profileArn=arn%3Aaws%3Acodewhisperer%3Aeu-west-1%3A1%3Aprofile%2FA"
        );
    }

    #[test]
    fn kimchi_metadata_drives_kind_and_capabilities() {
        let vision = normalize_kimchi_model(&json!({
            "slug": "vision-model",
            "display_name": "Vision",
            "provider": "anthropic",
            "reasoning": true,
            "input_modalities": ["text", "image"],
            "limits": {"context_window": 128000, "max_output_tokens": 8192},
        }))
        .expect("normalized");
        assert_eq!(vision["kind"], "imageToText");
        assert_eq!(vision["capabilities"]["vision"], true);
        assert_eq!(vision["capabilities"]["reasoning"], true);
        assert_eq!(vision["capabilities"]["contextWindow"], 128_000);
        assert_eq!(vision["capabilities"]["maxOutput"], 8_192);
        assert_eq!(vision["compat"]["cacheControlFormat"], "anthropic");

        let plain =
            normalize_kimchi_model(&json!({"id": "kimchi-x", "input_modalities": ["text"]}))
                .expect("normalized");
        assert_eq!(plain["kind"], "llm");
        assert_eq!(plain["capabilities"]["vision"], false);

        assert!(normalize_kimchi_model(&json!({"slug": "   "})).is_none());
        assert!(normalize_kimchi_model(&json!("nope")).is_none());
    }

    #[test]
    fn kimchi_url_uses_the_stored_endpoint_over_the_default() {
        assert_eq!(
            kimchi_models_url(None),
            "https://llm.kimchi.dev/v1/models/metadata?include_in_cli=true"
        );
        assert_eq!(
            kimchi_models_url(Some("https://kimchi.internal//")),
            "https://kimchi.internal/v1/models/metadata?include_in_cli=true"
        );
    }

    #[test]
    fn grok_cli_parser_accepts_every_upstream_shape() {
        let array = parse_grok_cli_models(&json!([{"id": "grok-4.5", "display_name": "Grok 4.5"}]));
        assert_eq!(array.len(), 1);
        assert_eq!(array[0]["name"], "Grok 4.5");

        let wrapped = parse_grok_cli_models(&json!({"data": [{"model_id": "grok-3"}]}));
        assert_eq!(wrapped[0]["id"], "grok-3");

        let keyed = parse_grok_cli_models(&json!({"models": {"grok-2": {"name": "Grok 2"}}}));
        assert_eq!(keyed[0]["id"], "grok-2");
        assert_eq!(keyed[0]["name"], "Grok 2");

        let deduped =
            parse_grok_cli_models(&json!([{"id": "grok-1"}, {"id": "grok-1"}, {"id": "  "}]));
        assert_eq!(deduped.len(), 1);

        let grok_build = parse_grok_cli_models(&json!([{"id": GROK_CLI_MODEL}]));
        assert_eq!(grok_build[0]["contextLength"], 500_000);
        assert_eq!(grok_build[0]["maxOutputTokens"], 64_000);
    }

    #[test]
    fn unported_resolvers_are_declared() {
        assert!(is_unported_live_resolver("qoder"));
        assert!(is_unported_live_resolver("github"));
        assert!(is_unported_live_resolver("cursor"));
        assert!(is_unported_live_resolver("zed"));
        assert!(!is_unported_live_resolver("kiro"));
        assert!(!is_unported_live_resolver("openai-compatible-x"));
    }
}
