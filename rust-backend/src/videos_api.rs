//! Native port of the pinned upstream `/v1/videos/**` family.
//!
//! Upstream sources mirrored here (pinned snapshot
//! `17c4cc76877bd1755030a8414f8d0083f48dcccf`, `0.5.75`):
//!
//! * `src/app/api/v1/videos/{generations,edits,extensions}/route.js` — POST job
//!   creation, all three feeding `handleVideoCreate(request, action)`;
//! * `src/app/api/v1/videos/[id]/route.js` — GET job status via
//!   `handleVideoGet(request, id)`;
//! * `src/sse/handlers/videoGeneration.js` — API-key validation, byte-preserving
//!   body forwarding, provider resolution (`xai` default, `getModelInfo` prefix
//!   stripping), connection selection/rotation and the `x-9router-connection-id`
//!   echo header;
//! * `open-sse/handlers/videoCore.js` — the transparent async-job proxy plus the
//!   adapter dispatch and the sanitised error envelopes;
//! * `open-sse/handlers/videoProviders/{openrouter,vertex}.js` — the two
//!   non-xAI wire formats (xAI is the default shape);
//! * `open-sse/services/tokenRefresh.js` (`parseVertexSaJson`,
//!   `refreshVertexToken`) for the Vertex service-account token mint.
//!
//! ## Declared approximations
//!
//! * Provider/report bookkeeping that upstream persists next to a failure
//!   (`markAccountUnavailable` → `testStatus`, `lastError`, `errorCode`, model
//!   locks, `backoffLevel`, and therefore `allRateLimited` cooldown answers) is
//!   not persisted by the native backend. The request-level behaviour is
//!   identical — an error that upstream rotates on still rotates here — but the
//!   account is not parked for the following requests.
//! * `getProviderCredentials` strategy selection (`fallbackStrategy`
//!   `round-robin` with sticky counts, `fill-first` bookkeeping, proxy pools,
//!   the `noAuth` free-provider virtual connection) is not implemented; the
//!   native picker walks the active connections in `priority` order and honours
//!   `x-connection-id` pinning, exactly like the other native adapters.
//! * The `401`/`403` "refresh once, retry once" branch of `handleVideoProxyCore`
//!   needs `refreshTokenByProvider`; the native backend has no provider token
//!   refresh (`models_live.rs` documents the same gap), so the upstream response
//!   is returned as-is — the same outcome upstream produces when its refresh
//!   fails ("account needs re-auth"). The Vertex service-account mint, which is
//!   independent of that branch, *is* implemented.
//! * `checkAndRefreshToken` is expiry driven, and the only video credentials with
//!   an expiry are OAuth ones (xAI device-code tokens) — same gap as above.
//! * Re-serialising a prefix-stripped JSON body (`{...parsed, model}`) uses
//!   `serde_json`, so object keys are emitted in a different order than Node's
//!   insertion order. The forwarded payload is semantically identical.
//! * Outbound calls use the shared HTTP client and the catalog `baseUrl`
//!   (like upstream's plain `fetch`, there is no SSRF guard on this route): the
//!   only client-controlled URL segment is percent-encoded, and a Vertex job id
//!   must match the anchored operation-name pattern before it reaches a URL.

use std::{
    collections::HashSet,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode},
};
use base64::Engine as _;
use bytes::Bytes;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    api_errors::{error_response, method_not_allowed, truncate_utf16},
    error::AppError,
    providers,
    state::AppState,
};

/// Video generation is xAI-only today; requests without a provider prefix
/// (bare model id, or multipart bodies we deliberately don't parse) land here.
const DEFAULT_VIDEO_PROVIDER: &str = "xai";

/// Upstream fetch deadline for video job submission/polling (the job itself is
/// async upstream — this only bounds the HTTP round-trip).
const DEFAULT_VIDEO_FETCH_TIMEOUT_MS: u64 = 120_000;

/// `message.slice(0, 2000)` in `handleVideoProxyCore`.
const ERROR_TEXT_LIMIT: usize = 2000;

/// `OAUTH_ENDPOINTS.google.token`.
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Vertex Veo default location when the connection does not pin one.
const VERTEX_DEFAULT_LOCATION: &str = "us-central1";

/// `VIDEO_ACTIONS` — creation actions the xAI shape accepts.
const VIDEO_ACTIONS: [&str; 3] = ["generations", "edits", "extensions"];

/// `CREATE_ROTATION_STATUSES` — errors that upstream rejects *before* creating a
/// billable job, so the next account may be tried.
const CREATE_ROTATION_STATUSES: [u16; 3] = [401, 403, 429];

/// `LOCAL_PROVIDER_ALIASES` (`src/sse/services/model.js`).
const LOCAL_PROVIDER_ALIASES: [(&str, &str); 2] = [
    ("xmtp", "xiaomi-tokenplan"),
    ("xiaomi-tokenplan", "xiaomi-tokenplan"),
];

/// `BUILTIN_MODEL_ALIASES` (`open-sse/services/model.js`).
const BUILTIN_MODEL_ALIASES: [(&str, &str); 1] = [("grok-build", "gcli/grok-build")];

/// Provider-node types consulted for user-defined model prefixes.
const PROVIDER_NODE_TYPES: [&str; 3] = [
    "openai-compatible",
    "anthropic-compatible",
    "custom-embedding",
];

static OPERATION_NAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^projects/[^/]+/locations/[^/]+/publishers/[^/]+/models/[^/]+/operations/[^/]+$")
        .expect("valid operation name pattern")
});
static VERTEX_JOB_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("valid job id pattern"));
static VERTEX_MODEL_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").expect("valid model id pattern"));
static DATA_URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)^data:([^;]+);base64,(.*)$").expect("valid data url pattern"));
static BEARER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("valid bearer pattern"));

/// `vertexTokenCache` — `{ token, expiresAt }` keyed by service-account email.
static VERTEX_TOKEN_CACHE: Lazy<Mutex<Vec<(String, String, i64)>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

/// A creation action (`POST /v1/videos/{action}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoAction {
    Generations,
    Edits,
    Extensions,
}

impl VideoAction {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "generations" => Some(Self::Generations),
            "edits" => Some(Self::Edits),
            "extensions" => Some(Self::Extensions),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generations => "generations",
            Self::Edits => "edits",
            Self::Extensions => "extensions",
        }
    }
}

/// Upstream `parseModel` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModel {
    pub provider: Option<String>,
    pub model: String,
    pub is_alias: bool,
    pub provider_alias: Option<String>,
}

/// Upstream `getModelInfo` result; `provider: None` means "combo".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: Option<String>,
    pub model: String,
}

/// Upstream `resolveVideoProvider` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVideoProvider {
    pub provider: String,
    pub model: Option<String>,
}

/// The subset of `getProviderCredentials` credentials this route consumes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoCredentials {
    pub connection_id: String,
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub project_id: Option<String>,
    pub provider_specific_data: Option<Value>,
}

/// Which wire format the request must be built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAdapter {
    /// No adapter: raw body forwarded to `{baseUrl}/{action}`, polled at
    /// `{baseUrl}/{id}`, upstream JSON passed through verbatim.
    Default,
    OpenRouter,
    Vertex,
}

/// A fully built upstream request.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestPlan {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
    pub adapter: PlanAdapter,
}

impl RequestPlan {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Why an upstream request never produced a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkFailure {
    /// Node reports aborted/timed-out fetches as a timeout.
    Aborted,
    /// Transport-level failure; carries the underlying message.
    Other(String),
}

/// Terminal outcome of one proxied video request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreOutcome {
    Success {
        status: u16,
        content_type: String,
        body: String,
    },
    Failure {
        status: u16,
        error: String,
    },
}

impl CoreOutcome {
    pub fn failure(status: u16, error: impl Into<String>) -> Self {
        Self::Failure {
            status,
            error: error.into(),
        }
    }
}

/// Which `/v1/videos/**` sub-route a request addressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoTarget {
    Create(VideoAction),
    Status(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Small JS-semantics helpers
// ─────────────────────────────────────────────────────────────────────────────

/// JavaScript truthiness of a JSON value (`""`, `0`, `NaN`, `null`, `false` and
/// `undefined` are falsy; objects and arrays are truthy).
pub fn is_js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number
            .as_f64()
            .map(|n| n != 0.0 && !n.is_nan())
            .unwrap_or(false),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// First JavaScript-truthy string in order, mirroring `a || b || c`.
fn first_truthy_string<'a>(values: impl IntoIterator<Item = Option<&'a String>>) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .cloned()
}

/// `String(value)` for the model field, or `None` when the value is falsy
/// (upstream `if (!parsedBody?.model)`).
fn js_model_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(true) => Some("true".to_string()),
        Value::Bool(false) => None,
        Value::Number(number) => {
            let raw = number.to_string();
            if raw == "0" {
                None
            } else {
                Some(raw)
            }
        }
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::String(_) => None,
        // `String({})`/`String([])` never produce a usable model id; upstream
        // would forward them verbatim and let the provider reject the request.
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// `Number(value)` as JSON — non-numeric input becomes `NaN`, which
/// `JSON.stringify` renders as `null`.
fn js_number(value: &Value) -> Value {
    match value {
        Value::Number(number) => Value::Number(number.clone()),
        Value::Bool(true) => json!(1),
        Value::Bool(false) => json!(0),
        Value::String(text) => match text.trim().parse::<f64>() {
            // `JSON.stringify(2)` — integral values must not render as "2.0".
            Ok(number) if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 => {
                json!(number as i64)
            }
            Ok(number) => serde_json::Number::from_f64(number)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Err(_) => Value::Null,
        },
        Value::Null | Value::Array(_) | Value::Object(_) => Value::Null,
    }
}

/// `encodeURIComponent` — everything outside `A-Za-z0-9-_.!~*'()` is percent
/// encoded as UTF-8 (unlike `form_urlencoded`, a space becomes `%20`).
pub fn encode_uri_component(value: &str) -> String {
    const UNRESERVED: &[u8] = b"-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || UNRESERVED.contains(byte) {
            out.push(ch);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Host placeholder resolution for the catalog `videoConfig.baseUrl`.
fn has_video_base_url(config: &Value) -> bool {
    config
        .as_object()
        .map(|object| {
            object.contains_key("baseUrl")
                && object
                    .get("baseUrl")
                    .and_then(Value::as_str)
                    .map(|url| !url.is_empty())
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn has_video_config(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| has_video_base_url(value)).cloned()
}

/// `PROVIDER_MEDIA[provider]?.videoConfig` keyed exactly as upstream keys it
/// (canonical provider ids; aliases are *not* resolved by the lookup itself).
fn raw_video_config(provider: &str) -> Option<Value> {
    let catalog = providers::catalog();
    if let Some(config) = has_video_config(
        catalog
            .get("media")
            .and_then(|media| media.get(provider))
            .and_then(|entry| entry.get("videoConfig")),
    ) {
        return Some(config);
    }
    catalog
        .get("registry")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(provider))
        })
        .and_then(|entry| entry.get("videoConfig"))
        .and_then(|value| has_video_config(Some(value)))
}

/// `getVideoConfig(provider)` for already-resolved provider ids (aliases are
/// canonicalised, which is how upstream reaches this check).
fn video_config(provider: &str) -> Option<Value> {
    has_video_config(Some(&providers::media_config(provider, "video")))
}

/// True when the provider has a `videoConfig` in the catalog.
pub fn supports_video(provider: &str) -> bool {
    video_config(provider).is_some()
}

/// `getVideoConfig(value)` without provider-id canonicalisation, mirroring the
/// raw lookups upstream performs on the `x-connection-id`/`provider` values.
fn supports_video_raw(provider: &str) -> bool {
    raw_video_config(provider).is_some()
}

/// `getVideoAdapter(provider)`.
pub fn plan_adapter(provider: &str) -> PlanAdapter {
    match provider {
        "openrouter" => PlanAdapter::OpenRouter,
        "vertex" => PlanAdapter::Vertex,
        _ => PlanAdapter::Default,
    }
}

fn video_fetch_timeout_ms() -> u64 {
    static TIMEOUT: Lazy<u64> = Lazy::new(|| {
        std::env::var("VIDEO_FETCH_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_VIDEO_FETCH_TIMEOUT_MS)
    });
    *TIMEOUT
}

/// `sanitizeSecrets` — strip bearer tokens and known credential values from text
/// destined for clients or logs.
pub fn sanitize_secrets(text: &str, credentials: Option<&VideoCredentials>) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut out = BEARER_RE
        .replace_all(text, "Bearer [redacted]")
        .into_owned();
    if let Some(credentials) = credentials {
        for secret in [
            credentials.access_token.as_ref(),
            credentials.refresh_token.as_ref(),
            credentials.api_key.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if secret.len() >= 8 {
                out = out.replace(secret.as_str(), "[redacted]");
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Provider resolution (`getModelInfo` + `resolveVideoProvider`)
// ─────────────────────────────────────────────────────────────────────────────

fn reserved_provider_prefixes() -> &'static HashSet<String> {
    static PREFIXES: Lazy<HashSet<String>> = Lazy::new(|| {
        let mut set: HashSet<String> = LOCAL_PROVIDER_ALIASES
            .iter()
            .map(|(alias, _)| (*alias).to_string())
            .collect();
        if let Some(entries) = providers::catalog()
            .get("registry")
            .and_then(Value::as_array)
        {
            for entry in entries {
                if let Some(id) = entry.get("id").and_then(Value::as_str) {
                    set.insert(id.to_string());
                }
                if let Some(alias) = entry.get("alias").and_then(Value::as_str) {
                    set.insert(alias.to_string());
                }
                if let Some(aliases) = entry.get("aliases").and_then(Value::as_array) {
                    for alias in aliases.iter().filter_map(Value::as_str) {
                        set.insert(alias.to_string());
                    }
                }
            }
        }
        set
    });
    &PREFIXES
}

/// `parseModel` — `"provider/model"`, `"alias/model"` or a bare alias.
pub fn parse_model(model_str: &str) -> ParsedModel {
    if let Some((provider_alias, model)) = model_str.split_once('/') {
        let provider = LOCAL_PROVIDER_ALIASES
            .iter()
            .find(|(alias, _)| *alias == provider_alias)
            .map(|(_, id)| (*id).to_string())
            .unwrap_or_else(|| providers::canonical_provider(provider_alias));
        return ParsedModel {
            provider: Some(provider),
            model: model.to_string(),
            is_alias: false,
            provider_alias: Some(provider_alias.to_string()),
        };
    }
    ParsedModel {
        provider: None,
        model: model_str.to_string(),
        is_alias: true,
        provider_alias: None,
    }
}

/// `resolveModelAliasFromMap` — accepts `"provider/model"` or `{provider, model}`.
pub fn resolve_model_alias_from_map(alias: &str, aliases: &Value) -> Option<ModelInfo> {
    let resolved = aliases.get(alias)?;
    if let Some(text) = resolved.as_str() {
        let (provider, model) = text.split_once('/')?;
        return Some(ModelInfo {
            provider: Some(providers::canonical_provider(provider)),
            model: model.to_string(),
        });
    }
    let provider = resolved.get("provider").and_then(Value::as_str)?;
    let model = resolved.get("model").and_then(Value::as_str)?;
    Some(ModelInfo {
        provider: Some(providers::canonical_provider(provider)),
        model: model.to_string(),
    })
}

/// `inferProviderFromModelName` — config-driven prefix table, `openai` fallback.
pub fn infer_provider_from_model_name(model_name: &str) -> String {
    let lowered = model_name.to_lowercase();
    let rules: [(&str, &str); 5] = [
        ("claude-", "anthropic"),
        ("gemini-", "gemini"),
        ("gpt-", "openai"),
        ("deepseek-", "openrouter"),
        ("", "openai"),
    ];
    for (prefix, provider) in rules {
        if prefix.is_empty() {
            return provider.to_string();
        }
        if lowered.starts_with(prefix) {
            return provider.to_string();
        }
    }
    "openai".to_string()
}

/// `o1`/`o3`/`o4` prefix inference (`/^o[134]/`).
fn infers_openai(model_name: &str) -> bool {
    let lowered = model_name.to_lowercase();
    let mut chars = lowered.chars();
    chars.next() == Some('o') && matches!(chars.next(), Some('1' | '3' | '4'))
}

/// `inferProviderFromModelName` including the `/^o[134]/` rule.
pub fn infer_provider_with_o_series(model_name: &str) -> String {
    if infers_openai(model_name) {
        return "openai".to_string();
    }
    infer_provider_from_model_name(model_name)
}

fn provider_node_by_prefix(state: &AppState, prefix: &str) -> Result<Option<String>, AppError> {
    let nodes = state.db.list_json_table("providerNodes")?;
    for node_type in PROVIDER_NODE_TYPES {
        let matched = nodes.iter().find(|node| {
            node.get("type").and_then(Value::as_str) == Some(node_type)
                && node.get("prefix").and_then(Value::as_str) == Some(prefix)
        });
        if let Some(node) = matched {
            if let Some(id) = node.get("id").and_then(Value::as_str) {
                return Ok(Some(id.to_string()));
            }
        }
    }
    Ok(None)
}

/// `getModelInfo(modelStr)` — parse, combo detection, alias resolution, provider
/// node prefixes and finally prefix inference.
pub fn model_info(state: &AppState, model_str: &str) -> Result<ModelInfo, AppError> {
    let parsed = parse_model(model_str);
    if !parsed.is_alias {
        let prefix = parsed.provider_alias.clone().unwrap_or_default();
        if !reserved_provider_prefixes().contains(&prefix) {
            if let Some(node_id) = provider_node_by_prefix(state, &prefix)? {
                return Ok(ModelInfo {
                    provider: Some(node_id),
                    model: parsed.model,
                });
            }
        }
        return Ok(ModelInfo {
            provider: parsed.provider,
            model: parsed.model,
        });
    }

    if state.db.combo_by_name(&parsed.model)?.is_some() {
        return Ok(ModelInfo {
            provider: None,
            model: parsed.model,
        });
    }

    let aliases = state.db.kv_all("modelAliases")?;
    if let Some(resolved) = resolve_model_alias_from_map(&parsed.model, &Value::Object(aliases)) {
        return Ok(resolved);
    }
    if let Some(resolved) = resolve_model_alias_from_map(
        &parsed.model,
        &Value::Object(
            BUILTIN_MODEL_ALIASES
                .iter()
                .map(|(alias, target)| ((*alias).to_string(), json!(target)))
                .collect::<Map<String, Value>>(),
        ),
    ) {
        return Ok(resolved);
    }

    Ok(ModelInfo {
        provider: Some(infer_provider_with_o_series(&parsed.model)),
        model: parsed.model,
    })
}

/// The `getVideoConfig(modelInfo.provider)` gate of `resolveVideoProvider`.
pub fn select_video_route(
    model_str: &str,
    info: &ModelInfo,
    supports: impl Fn(&str) -> bool,
) -> Result<ResolvedVideoProvider, String> {
    let Some(provider) = info.provider.clone() else {
        return Err("Combos are not supported for video generation".to_string());
    };
    if !supports(&provider) {
        // Bare model ids (no explicit "provider/" prefix) fall back to the
        // default video provider — the prefix-less inference targets chat
        // providers only.
        if !model_str.contains('/') {
            return Ok(ResolvedVideoProvider {
                provider: DEFAULT_VIDEO_PROVIDER.to_string(),
                model: Some(model_str.to_string()),
            });
        }
        return Err(format!(
            "Provider '{provider}' does not support video generation"
        ));
    }
    Ok(ResolvedVideoProvider {
        provider,
        model: Some(info.model.clone()),
    })
}

/// `resolveVideoProvider(parsedBody)`.
pub fn resolve_video_provider(
    state: &AppState,
    parsed_body: Option<&Value>,
) -> Result<ResolvedVideoProvider, String> {
    let Some(model_value) = parsed_body
        .and_then(|body| body.get("model"))
        .filter(|value| is_js_truthy(value))
    else {
        return Ok(ResolvedVideoProvider {
            provider: DEFAULT_VIDEO_PROVIDER.to_string(),
            model: None,
        });
    };
    let Some(model_str) = js_model_string(model_value) else {
        return Ok(ResolvedVideoProvider {
            provider: DEFAULT_VIDEO_PROVIDER.to_string(),
            model: None,
        });
    };
    let info = model_info(state, &model_str).map_err(|error| error.to_string())?;
    select_video_route(&model_str, &info, supports_video)
}

// ─────────────────────────────────────────────────────────────────────────────
// Request plans
// ─────────────────────────────────────────────────────────────────────────────

fn base_url(config: &Value) -> Option<String> {
    config
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(|url| url.trim_end_matches('/').to_string())
}

fn config_headers(config: &Value) -> Vec<(String, String)> {
    config
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some(entry) = headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        entry.1 = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

/// `buildUpstreamUrl(config, action, requestId)` — the default (xAI) shape.
fn build_upstream_url(
    config: &Value,
    action: Option<VideoAction>,
    request_id: Option<&str>,
) -> Option<String> {
    let base = base_url(config)?;
    match request_id {
        Some(request_id) => Some(format!("{base}/{}", encode_uri_component(request_id))),
        None => action.map(|action| format!("{base}/{}", action.as_str())),
    }
}

/// `buildHeaders({token, contentType, idempotencyKey})` — the default shape.
fn default_plan(
    config: &Value,
    action: Option<VideoAction>,
    request_id: Option<&str>,
    raw_body: Option<&Bytes>,
    content_type: Option<&str>,
    idempotency_key: Option<&str>,
    credentials: &VideoCredentials,
) -> Option<RequestPlan> {
    let url = build_upstream_url(config, action, request_id)?;
    let method = if request_id.is_some() {
        Method::GET
    } else {
        Method::POST
    };
    let mut headers: Vec<(String, String)> = vec![("Accept".into(), "application/json".into())];
    if let Some(token) = first_truthy_string([
        credentials.access_token.as_ref(),
        credentials.api_key.as_ref(),
    ]) {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    if method == Method::POST {
        if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
            headers.push(("Content-Type".into(), content_type.to_string()));
        }
        if let Some(key) = idempotency_key.filter(|value| !value.is_empty()) {
            headers.push(("Idempotency-Key".into(), key.to_string()));
        }
    }
    Some(RequestPlan {
        method,
        url,
        headers,
        body: if request_id.is_some() {
            None
        } else {
            raw_body.cloned()
        },
        adapter: PlanAdapter::Default,
    })
}

/// `open-sse/handlers/videoProviders/openrouter.js`.
fn openrouter_plan(
    config: &Value,
    action: Option<VideoAction>,
    request_id: Option<&str>,
    raw_body: Option<&Bytes>,
    content_type: Option<&str>,
    credentials: &VideoCredentials,
) -> Result<RequestPlan, String> {
    let base = base_url(config).ok_or_else(|| "OpenRouter video requires a baseUrl".to_string())?;
    let token = first_truthy_string([
        credentials.access_token.as_ref(),
        credentials.api_key.as_ref(),
    ]);
    let mut headers: Vec<(String, String)> = vec![("Accept".into(), "application/json".into())];
    headers.extend(config_headers(config));
    if let Some(token) = token {
        upsert_header(&mut headers, "Authorization", format!("Bearer {token}"));
    }

    if let Some(request_id) = request_id {
        return Ok(RequestPlan {
            method: Method::GET,
            url: format!("{base}/{}", encode_uri_component(request_id)),
            headers,
            body: None,
            adapter: PlanAdapter::OpenRouter,
        });
    }
    match action {
        Some(VideoAction::Generations) => {}
        other => {
            return Err(format!(
                "OpenRouter video supports 'generations' only (got '{}')",
                other.map(VideoAction::as_str).unwrap_or("null")
            ))
        }
    }
    if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
        if !content_type.contains("application/json") {
            return Err("OpenRouter video requires an application/json body".to_string());
        }
    }
    upsert_header(&mut headers, "Content-Type", "application/json");
    Ok(RequestPlan {
        method: Method::POST,
        url: base,
        headers,
        body: raw_body.cloned(),
        adapter: PlanAdapter::OpenRouter,
    })
}

/// `parseVertexSaJson(apiKey)`.
pub fn parse_vertex_sa_json(api_key: Option<&str>) -> Option<Value> {
    let api_key = api_key?;
    let parsed: Value = serde_json::from_str(api_key).ok()?;
    let is_service_account = parsed.get("type").and_then(Value::as_str) == Some("service_account")
        && parsed
            .get("client_email")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && parsed
            .get("private_key")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && parsed
            .get("project_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
    if is_service_account {
        Some(parsed)
    } else {
        None
    }
}

/// `encodeJobId(name)` — base64url without padding.
pub fn vertex_encode_job_id(name: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(name.as_bytes())
}

/// `decodeJobId(id)` — only ids that re-encode byte-for-byte and decode to an
/// anchored operation resource name are accepted.
pub fn vertex_decode_job_id(id: &str) -> Option<String> {
    if id.is_empty() || id.len() > 1024 || !VERTEX_JOB_ID_RE.is_match(id) {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    if vertex_encode_job_id(&decoded) != id {
        return None;
    }
    if OPERATION_NAME_RE.is_match(&decoded) {
        Some(decoded)
    } else {
        None
    }
}

/// `modelPathOf(operationName)`.
pub fn vertex_model_path_of(operation_name: &str) -> Option<&str> {
    operation_name
        .find("/operations/")
        .map(|index| &operation_name[..index])
}

/// `toVertexBody(body)` — OpenAI-ish video body → Vertex `predictLongRunning`.
pub fn to_vertex_body(body: &Value) -> Value {
    let mut instance = Map::new();
    if let Some(prompt) = body.get("prompt") {
        instance.insert("prompt".into(), prompt.clone());
    }
    let image = body.get("image").or_else(|| body.get("image_url"));
    if let Some(image) = image {
        match image {
            Value::Object(_) => {
                instance.insert("image".into(), image.clone());
            }
            Value::String(text) if !text.is_empty() => {
                let mirrored = DATA_URL_RE.captures(text).map(|captures| {
                    json!({
                        "bytesBase64Encoded": captures.get(2).map(|m| m.as_str()).unwrap_or_default(),
                        "mimeType": captures.get(1).map(|m| m.as_str()).unwrap_or_default(),
                    })
                });
                instance.insert(
                    "image".into(),
                    mirrored.unwrap_or_else(|| json!({ "gcsUri": text })),
                );
            }
            _ => {}
        }
    }
    if let Some(video) = body.get("video").filter(|value| value.is_object()) {
        instance.insert("video".into(), video.clone());
    }

    let mut parameters = Map::new();
    if let Some(value) = body.get("n").filter(|value| !value.is_null()) {
        parameters.insert("sampleCount".into(), js_number(value));
    }
    if let Some(value) = body.get("duration").filter(|value| !value.is_null()) {
        parameters.insert("durationSeconds".into(), js_number(value));
    }
    if let Some(value) = body.get("aspect_ratio").filter(|value| is_js_truthy(value)) {
        parameters.insert("aspectRatio".into(), value.clone());
    }
    if let Some(value) = body.get("resolution").filter(|value| is_js_truthy(value)) {
        parameters.insert("resolution".into(), value.clone());
    }
    if let Some(value) = body.get("seed").filter(|value| !value.is_null()) {
        parameters.insert("seed".into(), value.clone());
    }
    if let Some(value) = body
        .get("negative_prompt")
        .filter(|value| is_js_truthy(value))
    {
        parameters.insert("negativePrompt".into(), value.clone());
    }
    if let Some(value) = body.get("storage_uri").filter(|value| is_js_truthy(value)) {
        parameters.insert("storageUri".into(), value.clone());
    }
    if let Some(value) = body.get("generate_audio").filter(|value| !value.is_null()) {
        parameters.insert("generateAudio".into(), json!(is_js_truthy(value)));
    }

    let mut out = Map::new();
    out.insert(
        "instances".into(),
        Value::Array(vec![Value::Object(instance)]),
    );
    if !parameters.is_empty() {
        out.insert("parameters".into(), Value::Object(parameters));
    }
    Value::Object(out)
}

fn first_truthy<'a>(values: impl IntoIterator<Item = Option<&'a Value>>) -> Option<Value> {
    values
        .into_iter()
        .flatten()
        .find(|value| is_js_truthy(value))
        .cloned()
}

/// `fromVertexOperation(json)` — Vertex operation → the async-job shape.
pub fn from_vertex_operation(json: &Value) -> Value {
    let Some(name) = json.get("name").and_then(Value::as_str) else {
        return json.clone();
    };
    let id = vertex_encode_job_id(name);
    if let Some(error) = json.get("error").filter(|value| is_js_truthy(value)) {
        return json!({
            "id": id,
            "request_id": id,
            "status": "failed",
            "error": error,
        });
    }
    if !json.get("done").map(is_js_truthy).unwrap_or(false) {
        return json!({ "id": id, "request_id": id, "status": "pending" });
    }
    let samples = json
        .get("response")
        .and_then(|response| {
            first_truthy([
                response.get("videos"),
                response
                    .get("generateVideoResponse")
                    .and_then(|inner| inner.get("generatedSamples")),
            ])
        })
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let videos: Vec<Value> = samples
        .iter()
        .map(|sample| {
            let video = sample.get("video");
            let url = first_truthy([
                sample.get("gcsUri"),
                video.and_then(|value| value.get("uri")),
                sample.get("uri"),
            ]);
            let b64_json = first_truthy([
                sample.get("bytesBase64Encoded"),
                video.and_then(|value| value.get("bytesBase64Encoded")),
            ]);
            let mime_type = first_truthy([
                sample.get("mimeType"),
                video.and_then(|value| value.get("mimeType")),
            ])
            .unwrap_or_else(|| json!("video/mp4"));
            json!({ "url": url.unwrap_or(Value::Null), "b64_json": b64_json.unwrap_or(Value::Null), "mime_type": mime_type })
        })
        .collect();
    json!({
        "id": id,
        "request_id": id,
        "status": "completed",
        "video": videos.first().cloned().unwrap_or(Value::Null),
        "videos": videos,
    })
}

/// Service-account JSON → signed RS256 JWT assertion for the Google token
/// endpoint (`refreshVertexToken` minus the cache and the network hop).
pub fn build_sa_assertion(sa_json: &Value, issued_at_secs: i64) -> Result<String, String> {
    let client_email = sa_json
        .get("client_email")
        .and_then(Value::as_str)
        .ok_or_else(|| "service account JSON is missing client_email".to_string())?;
    let private_key = sa_json
        .get("private_key")
        .and_then(Value::as_str)
        .ok_or_else(|| "service account JSON is missing private_key".to_string())?
        .replace("\\n", "\n");
    let mut header = Header::new(Algorithm::RS256);
    header.typ = Some("JWT".to_string());
    let claims = json!({
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "iss": client_email,
        "aud": GOOGLE_TOKEN_ENDPOINT,
        "iat": issued_at_secs,
        "exp": issued_at_secs + 3600,
    });
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(private_key.as_bytes()).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// `refreshVertexToken(saJson, log)` — cached (5 minute lead) SA token mint.
async fn refresh_vertex_token(state: &AppState, sa_json: &Value) -> Option<String> {
    let cache_key = sa_json
        .get("client_email")
        .and_then(Value::as_str)?
        .to_string();
    let now = now_millis();
    if let Ok(cache) = VERTEX_TOKEN_CACHE.lock() {
        if let Some((_, token, _)) = cache
            .iter()
            .find(|(email, _, expires_at)| email == &cache_key && expires_at - now > 5 * 60 * 1000)
        {
            return Some(token.clone());
        }
    }

    let issued_at = now / 1000;
    let assertion = build_sa_assertion(sa_json, issued_at).ok()?;
    let response = state
        .http
        .post(GOOGLE_TOKEN_ENDPOINT)
        .timeout(Duration::from_millis(video_fetch_timeout_ms()))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}",
            encode_uri_component(&assertion)
        ))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)?
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    if let Ok(mut cache) = VERTEX_TOKEN_CACHE.lock() {
        cache.retain(|(email, _, _)| email != &cache_key);
        cache.push((cache_key, access_token.clone(), now + expires_in * 1000));
    }
    Some(access_token)
}

/// `resolveAuth(credentials)` for the Vertex adapter.
async fn vertex_resolve_auth(
    state: &AppState,
    credentials: &VideoCredentials,
) -> Result<(String, String, String), String> {
    let sa_json = parse_vertex_sa_json(credentials.api_key.as_deref());
    let from_sa = sa_json
        .as_ref()
        .and_then(|sa| sa.get("project_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let from_specific = credentials
        .provider_specific_data
        .as_ref()
        .and_then(|data| data.get("projectId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let project_id = first_truthy_string([
        from_sa.as_ref(),
        credentials.project_id.as_ref(),
        from_specific.as_ref(),
    ]);
    let location = credentials
        .provider_specific_data
        .as_ref()
        .and_then(|data| data.get("location"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(VERTEX_DEFAULT_LOCATION)
        .to_string();

    let Some(project_id) = project_id else {
        return Err("Vertex video requires a project_id — use Service Account JSON or set providerSpecificData.projectId".to_string());
    };

    let mut token = credentials
        .access_token
        .clone()
        .filter(|value| !value.is_empty());
    if let Some(sa_json) = sa_json.as_ref() {
        let minted = refresh_vertex_token(state, sa_json).await;
        match minted {
            Some(minted) => token = Some(minted),
            None => {
                return Err(
                    "Vertex video: failed to mint access token from service account JSON"
                        .to_string(),
                )
            }
        }
    }
    let Some(token) = token else {
        return Err("Vertex video requires Service Account JSON or an OAuth access token (raw API keys are not supported)".to_string());
    };

    Ok((token, project_id, location))
}

/// `open-sse/handlers/videoProviders/vertex.js` `buildRequest`.
async fn vertex_plan(
    state: &AppState,
    config: &Value,
    action: Option<VideoAction>,
    request_id: Option<&str>,
    raw_body: Option<&Bytes>,
    content_type: Option<&str>,
    credentials: &VideoCredentials,
) -> Result<RequestPlan, String> {
    if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
        if !content_type.contains("application/json") {
            return Err("Vertex video requires an application/json body".to_string());
        }
    }

    let (token, project_id, location) = vertex_resolve_auth(state, credentials).await?;
    let base = base_url(config).unwrap_or_else(|| "https://aiplatform.googleapis.com".to_string());
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
    ];

    if let Some(request_id) = request_id {
        let operation_name = vertex_decode_job_id(request_id)
            .ok_or_else(|| "Invalid Vertex video job id".to_string())?;
        let model_path = vertex_model_path_of(&operation_name)
            .ok_or_else(|| "Invalid Vertex video job id".to_string())?;
        return Ok(RequestPlan {
            method: Method::POST,
            url: format!("{base}/v1/{model_path}:fetchPredictOperation"),
            headers,
            body: Some(Bytes::from(
                serde_json::to_vec(&json!({ "operationName": operation_name }))
                    .map_err(|error| error.to_string())?,
            )),
            adapter: PlanAdapter::Vertex,
        });
    }

    match action {
        Some(VideoAction::Generations) => {}
        other => {
            return Err(format!(
                "Vertex video supports 'generations' only (got '{}')",
                other.map(VideoAction::as_str).unwrap_or("null")
            ))
        }
    }

    let raw = raw_body.cloned().unwrap_or_default();
    let body: Value = serde_json::from_slice(&raw).map_err(|_| "Invalid JSON body".to_string())?;
    let model = body
        .get("model")
        .filter(|value| is_js_truthy(value))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(model) = model else {
        return Err(
            "Vertex video requires a model (e.g. vertex/veo-3.1-generate-preview)".to_string(),
        );
    };
    if !VERTEX_MODEL_ID_RE.is_match(&model) {
        return Err("Invalid Vertex video model id".to_string());
    }
    let has_prompt = body.get("prompt").map(is_js_truthy).unwrap_or(false);
    let has_image = body.get("image").map(is_js_truthy).unwrap_or(false)
        || body.get("image_url").map(is_js_truthy).unwrap_or(false);
    if !has_prompt && !has_image {
        return Err("Vertex video requires a prompt or an image".to_string());
    }

    let vertex_body = to_vertex_body(&body);
    Ok(RequestPlan {
        method: Method::POST,
        url: format!(
            "{base}/v1/projects/{project_id}/locations/{location}/publishers/google/models/{model}:predictLongRunning"
        ),
        headers,
        body: Some(Bytes::from(
            serde_json::to_vec(&vertex_body).map_err(|error| error.to_string())?,
        )),
        adapter: PlanAdapter::Vertex,
    })
}

/// `adapter ? adapter.buildRequest(...) : defaultPlan()`.
#[allow(clippy::too_many_arguments)]
async fn build_plan(
    state: &AppState,
    provider: &str,
    config: &Value,
    action: Option<VideoAction>,
    request_id: Option<&str>,
    raw_body: Option<&Bytes>,
    content_type: Option<&str>,
    idempotency_key: Option<&str>,
    credentials: &VideoCredentials,
) -> Result<RequestPlan, String> {
    match plan_adapter(provider) {
        PlanAdapter::Default => default_plan(
            config,
            action,
            request_id,
            raw_body,
            content_type,
            idempotency_key,
            credentials,
        )
        .ok_or_else(|| "Unsupported video base URL".to_string()),
        PlanAdapter::OpenRouter => openrouter_plan(
            config,
            action,
            request_id,
            raw_body,
            content_type,
            credentials,
        ),
        PlanAdapter::Vertex => {
            vertex_plan(
                state,
                config,
                action,
                request_id,
                raw_body,
                content_type,
                credentials,
            )
            .await
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core proxy
// ─────────────────────────────────────────────────────────────────────────────

/// Upstream `handleVideoProxyCore`'s terminal mapping of an upstream response.
pub fn map_upstream_response(
    provider: &str,
    status: u16,
    content_type: Option<&str>,
    body_text: &str,
    adapter: PlanAdapter,
    credentials: &VideoCredentials,
) -> CoreOutcome {
    if !(200..300).contains(&status) {
        let fallback = format!("HTTP {status}");
        let raw = if body_text.is_empty() {
            fallback.as_str()
        } else {
            body_text
        };
        let message = sanitize_secrets(raw, Some(credentials));
        return CoreOutcome::failure(
            status,
            format!(
                "[{provider}] {}",
                truncate_utf16(&message, Some(ERROR_TEXT_LIMIT))
            ),
        );
    }

    let mut out_body = body_text.to_string();
    let mut out_type = content_type
        .filter(|value| !value.is_empty())
        .unwrap_or("application/json")
        .to_string();
    if adapter == PlanAdapter::Vertex {
        if let Ok(parsed) = serde_json::from_str::<Value>(body_text) {
            if let Ok(serialized) = serde_json::to_string(&from_vertex_operation(&parsed)) {
                out_body = serialized;
                out_type = "application/json".to_string();
            }
        }
    }
    CoreOutcome::Success {
        status,
        content_type: out_type,
        body: out_body,
    }
}

async fn send_plan(
    state: &AppState,
    plan: &RequestPlan,
    timeout_ms: u64,
) -> Result<reqwest::Response, NetworkFailure> {
    let mut request = state
        .http
        .request(plan.method.clone(), &plan.url)
        .timeout(Duration::from_millis(timeout_ms.max(1)));
    for (name, value) in &plan.headers {
        request = request.header(name, value);
    }
    if let Some(body) = plan.body.clone() {
        request = request.body(body);
    }
    match request.send().await {
        Ok(response) => Ok(response),
        Err(error) if error.is_timeout() => Err(NetworkFailure::Aborted),
        Err(error) => Err(NetworkFailure::Other(error.to_string())),
    }
}

/// Everything `proxy_core` needs to build and send one upstream call.
pub struct CoreRequest<'a> {
    pub provider: &'a str,
    pub action: Option<VideoAction>,
    pub request_id: Option<&'a str>,
    pub raw_body: Option<&'a Bytes>,
    pub content_type: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    pub credentials: &'a VideoCredentials,
}

/// `handleVideoProxyCore` — one upstream attempt, no retries.
pub async fn proxy_core(state: &AppState, request: CoreRequest<'_>) -> CoreOutcome {
    let CoreRequest {
        provider,
        action,
        request_id,
        raw_body,
        content_type,
        idempotency_key,
        credentials,
    } = request;

    let Some(config) = video_config(provider) else {
        return CoreOutcome::failure(
            400,
            format!("Provider '{provider}' does not support video generation"),
        );
    };
    let known_action = action
        .map(|action| VIDEO_ACTIONS.contains(&action.as_str()))
        .unwrap_or(false);
    if request_id.is_none() && !known_action {
        return CoreOutcome::failure(
            400,
            format!(
                "Unknown video action: {}",
                action.map(VideoAction::as_str).unwrap_or("null")
            ),
        );
    }

    let method = if request_id.is_some() { "GET" } else { "POST" };
    let plan = match build_plan(
        state,
        provider,
        &config,
        action,
        request_id,
        raw_body,
        content_type,
        idempotency_key,
        credentials,
    )
    .await
    {
        Ok(plan) => plan,
        Err(error) => return CoreOutcome::failure(400, format!("[{provider}] {error}")),
    };

    let response = match send_plan(state, &plan, video_fetch_timeout_ms()).await {
        Ok(response) => response,
        Err(NetworkFailure::Aborted) => {
            return CoreOutcome::failure(
                504,
                format!("[{provider}] video {method} aborted: This operation was aborted"),
            )
        }
        // Never re-send a creation POST on network error — the job may already
        // exist upstream.
        Err(NetworkFailure::Other(message)) => {
            return CoreOutcome::failure(
                502,
                sanitize_secrets(
                    &format!("[{provider}] video upstream fetch failed: {message}"),
                    Some(credentials),
                ),
            )
        }
    };

    read_upstream_response(response, provider, plan.adapter, credentials).await
}

async fn read_upstream_response(
    response: reqwest::Response,
    provider: &str,
    adapter: PlanAdapter,
    credentials: &VideoCredentials,
) -> CoreOutcome {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body_text = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            return CoreOutcome::failure(
                if error.is_timeout() { 504 } else { 502 },
                sanitize_secrets(
                    &format!("[{provider}] video upstream response body failed: {error}"),
                    Some(credentials),
                ),
            );
        }
    };
    map_upstream_response(
        provider,
        status,
        content_type.as_deref(),
        &body_text,
        adapter,
        credentials,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Connection selection
// ─────────────────────────────────────────────────────────────────────────────

/// Credentials carried into a request, mirroring the connection row
/// `getProviderCredentials` hands to the video handler.
pub fn credentials_from_connection(connection: &Value) -> VideoCredentials {
    VideoCredentials {
        connection_id: connection
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        api_key: connection
            .get("apiKey")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        access_token: connection
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        refresh_token: connection
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        project_id: connection
            .get("projectId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        provider_specific_data: connection.get("providerSpecificData").cloned(),
    }
}

/// Connections available for a provider, most preferred first.
fn provider_connections(state: &AppState, provider: &str) -> Vec<Value> {
    state
        .db
        .provider_connections(Some(provider), Some(true))
        .unwrap_or_default()
}

/// Pick the next usable connection: `x-connection-id` pinning first, then the
/// provider's preference order, skipping already-tried connections.
fn pick_credentials(
    state: &AppState,
    provider: &str,
    excluded: &HashSet<String>,
    preferred_connection_id: Option<&str>,
) -> Option<VideoCredentials> {
    let connections = provider_connections(state, provider);
    if let Some(preferred) = preferred_connection_id.filter(|value| !value.is_empty()) {
        let pinned = connections.iter().find(|connection| {
            connection.get("id").and_then(Value::as_str) == Some(preferred)
                && !excluded.contains(preferred)
        });
        if let Some(connection) = pinned {
            return Some(credentials_from_connection(connection));
        }
    }
    connections
        .into_iter()
        .find(|connection| {
            connection
                .get("id")
                .and_then(Value::as_str)
                .map(|id| !excluded.contains(id))
                .unwrap_or(false)
        })
        .map(|connection| credentials_from_connection(&connection))
}

/// The provider a poll request targets: the pinned connection's provider, an
/// explicit `?provider=`, or the historical xAI default.
fn resolve_get_provider(
    state: &AppState,
    query: Option<&str>,
    connection_id: Option<&str>,
) -> String {
    if let Some(connection_id) = connection_id.filter(|value| !value.is_empty()) {
        if let Ok(Some(connection)) = state.db.provider_connection(connection_id) {
            if let Some(provider) = connection.get("provider").and_then(Value::as_str) {
                if supports_video_raw(provider) {
                    return provider.to_string();
                }
            }
        }
    }
    let queried = query.and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "provider")
            .map(|(_, value)| value.into_owned())
    });
    if let Some(queried) = queried.filter(|value| supports_video_raw(value)) {
        return queried;
    }
    DEFAULT_VIDEO_PROVIDER.to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Request plumbing
// ─────────────────────────────────────────────────────────────────────────────

/// `readForwardableBody(request)`.
#[derive(Debug)]
struct ForwardableBody {
    raw: Bytes,
    parsed: Option<Value>,
    content_type: String,
}

fn read_forwardable_body(content_type: &str, raw: &Bytes) -> Result<ForwardableBody, String> {
    if content_type.contains("application/json") {
        let parsed: Value =
            serde_json::from_slice(raw).map_err(|_| "Invalid JSON body".to_string())?;
        return Ok(ForwardableBody {
            raw: raw.clone(),
            parsed: Some(parsed),
            content_type: content_type.to_string(),
        });
    }
    // Multipart (or any other content type): forward the exact bytes — parsing
    // and re-encoding would change the multipart boundary.
    Ok(ForwardableBody {
        raw: raw.clone(),
        parsed: None,
        content_type: content_type.to_string(),
    })
}

/// Split a public or internal video path into `/v1/videos/**` coordinates.
pub fn video_subpath(path: &str) -> Option<&str> {
    let path = path.strip_prefix("/api").unwrap_or(path);
    let path = path.strip_prefix("/v1").unwrap_or(path);
    let path = path.strip_prefix("/v1").unwrap_or(path);
    path.strip_prefix("/videos")
        .map(|rest| rest.trim_start_matches('/'))
}

/// `Err` carries the upstream `errorResponse(400, message)` text.
fn target_for(method: &Method, path: &str) -> Result<Option<VideoTarget>, String> {
    let Some(sub) = video_subpath(path) else {
        return Ok(None);
    };
    match *method {
        Method::POST => {
            let action =
                VideoAction::parse(sub).ok_or_else(|| format!("Unknown video action: {sub}"))?;
            Ok(Some(VideoTarget::Create(action)))
        }
        Method::GET => {
            if sub.is_empty() {
                return Err("Missing video request id".to_string());
            }
            Ok(Some(VideoTarget::Status(sub.to_string())))
        }
        _ => Ok(None),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn success_response(
    outcome: CoreOutcome,
    connection_id: Option<&str>,
) -> Result<Response<Body>, AppError> {
    match outcome {
        CoreOutcome::Success {
            status,
            content_type,
            body,
        } => {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
            );
            response
                .headers_mut()
                .insert("access-control-allow-origin", HeaderValue::from_static("*"));
            if let Some(connection_id) = connection_id.filter(|value| !value.is_empty()) {
                if let Ok(value) = HeaderValue::from_str(connection_id) {
                    // Video jobs are account-bound upstream — clients echo this
                    // back as `x-connection-id` on GET polls.
                    response
                        .headers_mut()
                        .insert("x-9router-connection-id", value);
                }
            }
            Ok(response)
        }
        CoreOutcome::Failure { status, error } => error_response(status, Some(&error)),
    }
}

/// `handleVideoCreate` — `POST /v1/videos/{generations,edits,extensions}`.
async fn handle_create(
    state: &AppState,
    action: VideoAction,
    headers: &HeaderMap,
    raw: &Bytes,
) -> Result<Response<Body>, AppError> {
    let content_type = header_string(headers, "content-type").unwrap_or_default();
    let body_info = match read_forwardable_body(&content_type, raw) {
        Ok(body_info) => body_info,
        Err(message) => return error_response(400, Some(&message)),
    };

    let resolved = match resolve_video_provider(state, body_info.parsed.as_ref()) {
        Ok(resolved) => resolved,
        Err(message) => return error_response(400, Some(&message)),
    };
    let provider = resolved.provider.clone();
    let model = resolved.model.clone();

    // Strip the provider prefix (e.g. "xai/grok-imagine-video") before
    // forwarding; otherwise forward the original bytes untouched.
    let mut forward_body = body_info.raw.clone();
    if let (Some(parsed), Some(model)) = (body_info.parsed.as_ref(), model.as_ref()) {
        let current = parsed
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if current != model {
            let mut mirrored = parsed.clone();
            if let Some(object) = mirrored.as_object_mut() {
                object.insert("model".into(), json!(model));
            }
            match serde_json::to_vec(&mirrored) {
                Ok(bytes) => forward_body = Bytes::from(bytes),
                Err(error) => {
                    return Err(AppError::Internal(error.into()));
                }
            }
        }
    }

    let preferred_connection_id = header_string(headers, "x-connection-id");
    let idempotency_key = header_string(headers, "idempotency-key");

    let mut excluded: HashSet<String> = HashSet::new();
    let mut last_error: Option<String> = None;
    let mut last_status: Option<u16> = None;

    loop {
        let credentials = pick_credentials(
            state,
            &provider,
            &excluded,
            preferred_connection_id.as_deref(),
        );
        let Some(credentials) = credentials else {
            if excluded.is_empty() {
                return error_response(
                    400,
                    Some(&format!("No credentials for provider: {provider}")),
                );
            }
            let status = last_status.unwrap_or(503);
            let error = last_error.unwrap_or_else(|| "All accounts unavailable".to_string());
            return error_response(status, Some(&error));
        };

        let outcome = proxy_core(
            state,
            CoreRequest {
                provider: &provider,
                action: Some(action),
                request_id: None,
                raw_body: Some(&forward_body),
                content_type: Some(body_info.content_type.as_str()),
                idempotency_key: idempotency_key.as_deref(),
                credentials: &credentials,
            },
        )
        .await;

        match outcome {
            CoreOutcome::Success { .. } => {
                return success_response(outcome, Some(&credentials.connection_id));
            }
            CoreOutcome::Failure { status, error } => {
                // Upstream records the failure on the connection and only
                // rotates for errors it rejects before creating a billable job.
                if CREATE_ROTATION_STATUSES.contains(&status) {
                    excluded.insert(credentials.connection_id.clone());
                    last_error = Some(error);
                    last_status = Some(status);
                    continue;
                }
                return error_response(status, Some(&error));
            }
        }
    }
}

/// `handleVideoGet` — `GET /v1/videos/{request_id}`.
async fn handle_status(
    state: &AppState,
    request_id: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Result<Response<Body>, AppError> {
    let preferred_connection_id = header_string(headers, "x-connection-id");
    // Poll requests carry no model, so the provider comes from the pinned
    // connection (`x-connection-id`, returned on create) or `?provider=`.
    let provider = resolve_get_provider(state, query, preferred_connection_id.as_deref());

    let credentials = pick_credentials(
        state,
        &provider,
        &HashSet::new(),
        preferred_connection_id.as_deref(),
    );
    let Some(credentials) = credentials else {
        return error_response(
            400,
            Some(&format!("No credentials for provider: {provider}")),
        );
    };

    let outcome = proxy_core(
        state,
        CoreRequest {
            provider: &provider,
            action: None,
            request_id: Some(request_id),
            raw_body: None,
            content_type: header_string(headers, "content-type").as_deref(),
            idempotency_key: None,
            credentials: &credentials,
        },
    )
    .await;

    match outcome {
        CoreOutcome::Success { .. } => success_response(outcome, Some(&credentials.connection_id)),
        CoreOutcome::Failure { status, error } => error_response(status, Some(&error)),
    }
}

/// Native `/v1/videos/**` entry point for both the public gateway route and the
/// internal `/api/v1/videos/**` compat path.
pub async fn handle(
    state: &AppState,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    raw: &Bytes,
) -> Result<Response<Body>, AppError> {
    let target = match target_for(method, path) {
        Ok(Some(target)) => target,
        // `Err` carries the upstream `errorResponse(400, message)` text.
        Err(message) => return error_response(400, Some(&message)),
        Ok(None) => {
            if video_subpath(path).is_some() {
                return method_not_allowed();
            }
            return Err(AppError::NotFound(format!(
                "no video route for {method} {path}"
            )));
        }
    };
    match target {
        VideoTarget::Create(action) => handle_create(state, action, headers, raw).await,
        VideoTarget::Status(request_id) => handle_status(state, &request_id, query, headers).await,
    }
}

/// `GET /api/v1/videos/{id}`-style internal calls that carry a parsed body.
pub async fn handle_parsed_body(
    state: &AppState,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let raw = if body.is_null() {
        Bytes::new()
    } else {
        Bytes::from(serde_json::to_vec(body)?)
    };
    handle(state, method, path, None, headers, &raw).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Db};
    use pretty_assertions::assert_eq;

    /// Test-only 2048-bit RSA key pair (no credential value).
    const TEST_RSA_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEA6b8ie2IGDf0d/UDs9CAp/Bt/BkAvo35BtkjPVcB9PfZO2AQ1
8afth249URuswfCvQIbkX0GFRS0/SLxjOYwK+QznbpmLfhoc1DldYqbg1AX8Jc1V
B3rrkAOkCSWD6zGWlwCopktTqO1L809Zx5BKzd719igoTrlNolLst5Ks864tVMBL
HtqHQxBfTV2fUQE9yc2maUp0XnN/PRIq5KbpGAZORK1uHqtQLXnu72AdqRJReNG+
eq4rO4W/l+/ozZKGZZJfPj7TRlIDqSjjq5WR6Qynhq7OSWfKf6vSjFGjdZR1Xz3q
hnZK5RTLj8hzj7hDu7UJNK2ZXvcm+iRUoTIvDQIDAQABAoIBAC0A+diblOLYmw+J
kpWmI69AdAJ2FTX7NxeriQ/Pkc1+QMvic6hlVpw+o1ucYnSsrHFWB143tTsObSLJ
8qi/x9UPoPdwZKUQzgAmU06NJrhrtpJoqDhaeEQwD0Mbj/yWfZHxNIdf9WmO1pKv
8m8z3tMoXF7aeHg/wSzBnoXxnY8E2c339U+USUTgOyaC0bk0m6jd47pThpTa6Gg0
8lQNrlRMkNkTfVk1KuXIfV/AQkFDn2C4LRoNx9nRaXmvKkXMJUSiUXIMBisYwpye
GbFEImSMjGI1lfOAXoht66Wd+eH9N2dFBhHdedsBH6cHqQ9VpB9x1RfTSBFOxANn
VshyzpkCgYEA9ykZVKIl1Z/0NDQRdsR3I3dY6LSOSMWEMWz9kzXn+WbJIGKVlclP
DOAUnuXaJDSbXKvvjgH2L6nHEKlyfXO+Dd+kzQpr3kBlGy/sPryVdsBddJji2ad2
SYjWI4wkV2qX3bMclBeLpC1Q5N0SlrdubVk9SgTqEc/C3A8cI/jFLW8CgYEA8hs5
LRzz0H4dLqePl7BSVSD2FpsQohMKi8qyYqs3KHW8KW1OSmvziBn/R1lOGY+c9r23
gYomHEhQxlJ/xg+6W7Kck/CZSvuj/0pbJt+0178qSl3zt76/G0McNG1WKw33ksCx
runfveClng/t+pFOG+lYsc2ZvMjOf7SM33e95UMCgYA3ztHnaE1+tQVhHDitRqNY
IMS0lsBh8idtOZzwNoXQrMLRSzFXhwMQdzBwyJm+/xntjO0kdZDvJjjKrFgrt4y8
eTkvCyFcJ9Isl1+SsuZU0A7KGxNt7gApjno7wJMcIfd0mdLkJYTkZ08SvlBKM9T9
X98U7ZMkvnLTWZ4TCUMMhQKBgGdlkvyeUc5oHeRv8VZSGkd7BT5QSUE+qpFbJuYW
wz7HUW3L3dTQ17f3iluZW051VA7YpUdwjagkhkK8tw8KZoeE93QDHCS25apAwj8O
6Tf+z3vlNhHyJ8Hn3mLRkyxeEa6eFwRho4l/KJwhp3wMlHQ9KwD8krzacb5+iG9j
vzjrAoGBAKjLTcMpKqLh8sH5NFFMAfIL6LVM3OIV96Tp2aGGVLPrR8r6IX53eqgt
H6kD5R7/bvdM35Ct6oFGHm8V99cwZd7jIgt5QewxEd8J2tSPWZamI35dLxjgIHqg
MlMIIG7IiSwxzUHU/myCdu3MgIS67lW22oELX4DNpP/83FlZlHdt
-----END RSA PRIVATE KEY-----";
    const TEST_RSA_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA6b8ie2IGDf0d/UDs9CAp
/Bt/BkAvo35BtkjPVcB9PfZO2AQ18afth249URuswfCvQIbkX0GFRS0/SLxjOYwK
+QznbpmLfhoc1DldYqbg1AX8Jc1VB3rrkAOkCSWD6zGWlwCopktTqO1L809Zx5BK
zd719igoTrlNolLst5Ks864tVMBLHtqHQxBfTV2fUQE9yc2maUp0XnN/PRIq5Kbp
GAZORK1uHqtQLXnu72AdqRJReNG+eq4rO4W/l+/ozZKGZZJfPj7TRlIDqSjjq5WR
6Qynhq7OSWfKf6vSjFGjdZR1Xz3qhnZK5RTLj8hzj7hDu7UJNK2ZXvcm+iRUoTIv
DQIDAQAB
-----END PUBLIC KEY-----";

    fn test_state() -> (tempfile::TempDir, AppState) {
        let temp = tempfile::tempdir().expect("temporary directory");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("create data directory");
        let db_path = data_dir.join("data.sqlite");
        let db = Db::open(&db_path).expect("open test database");
        let config = Config {
            listen: "127.0.0.1:0".parse().expect("test socket address"),
            ui_origin: "http://127.0.0.1:20129".into(),
            data_dir,
            db_path,
            upstream_timeout_secs: 5,
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = AppState::new(config, db).expect("test application state");
        (temp, state)
    }

    fn xai_config() -> Value {
        video_config("xai").expect("xai video config")
    }

    fn vertex_config() -> Value {
        video_config("vertex").expect("vertex video config")
    }

    fn openrouter_config() -> Value {
        video_config("openrouter").expect("openrouter video config")
    }

    fn credentials(api_key: Option<&str>, access_token: Option<&str>) -> VideoCredentials {
        VideoCredentials {
            connection_id: "conn-1".into(),
            api_key: api_key.map(str::to_string),
            access_token: access_token.map(str::to_string),
            ..VideoCredentials::default()
        }
    }

    #[tokio::test]
    async fn incomplete_video_body_is_not_reported_as_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
                assert!(request.len() <= 4096, "unexpected request size");
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/video"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        let outcome = read_upstream_response(
            response,
            "xai",
            PlanAdapter::Default,
            &credentials(None, None),
        )
        .await;
        assert!(
            matches!(outcome, CoreOutcome::Failure { status: 502, .. }),
            "{outcome:?}"
        );
        server.await.unwrap();
    }

    #[test]
    fn video_actions_match_upstream_route_table() {
        assert_eq!(
            VideoAction::parse("generations"),
            Some(VideoAction::Generations)
        );
        assert_eq!(VideoAction::parse("edits"), Some(VideoAction::Edits));
        assert_eq!(
            VideoAction::parse("extensions"),
            Some(VideoAction::Extensions)
        );
        assert_eq!(VideoAction::parse("cancel"), None);
        assert_eq!(VideoAction::parse("Generations"), None);
    }

    #[test]
    fn video_paths_cover_public_and_internal_forms() {
        assert_eq!(video_subpath("/v1/videos/generations"), Some("generations"));
        assert_eq!(
            video_subpath("/api/v1/videos/generations"),
            Some("generations")
        );
        assert_eq!(
            video_subpath("/api/v1/v1/videos/generations"),
            Some("generations")
        );
        assert_eq!(video_subpath("/v1/videos/abc-123"), Some("abc-123"));
        assert_eq!(video_subpath("/v1/videos"), Some(""));
        assert_eq!(video_subpath("/v1/chat/completions"), None);
    }

    #[test]
    fn unknown_actions_are_rejected_like_the_core() {
        assert_eq!(
            target_for(&Method::POST, "/v1/videos/cancel").unwrap_err(),
            "Unknown video action: cancel"
        );
        assert_eq!(
            target_for(&Method::POST, "/v1/videos/extensions").unwrap(),
            Some(VideoTarget::Create(VideoAction::Extensions))
        );
        assert_eq!(
            target_for(&Method::GET, "/v1/videos/req-1").unwrap(),
            Some(VideoTarget::Status("req-1".to_string()))
        );
        assert_eq!(
            target_for(&Method::GET, "/v1/videos").unwrap_err(),
            "Missing video request id"
        );
        assert_eq!(target_for(&Method::PUT, "/v1/videos/req-1").unwrap(), None);
    }

    #[test]
    fn parse_model_matches_upstream() {
        let parsed = parse_model("xai/grok-imagine-video");
        assert_eq!(parsed.provider.as_deref(), Some("xai"));
        assert_eq!(parsed.model, "grok-imagine-video");
        assert!(!parsed.is_alias);
        assert_eq!(parsed.provider_alias.as_deref(), Some("xai"));

        let parsed = parse_model("gcli/grok-build");
        assert_eq!(parsed.provider.as_deref(), Some("grok-cli"));
        assert_eq!(parsed.model, "grok-build");

        let parsed = parse_model("xmtp/some-model");
        assert_eq!(parsed.provider.as_deref(), Some("xiaomi-tokenplan"));

        let parsed = parse_model("grok-imagine-video");
        assert!(parsed.is_alias);
        assert_eq!(parsed.provider, None);
        assert_eq!(parsed.model, "grok-imagine-video");
    }

    #[test]
    fn provider_inference_matches_upstream_prefix_table() {
        assert_eq!(infer_provider_with_o_series("claude-opus"), "anthropic");
        assert_eq!(infer_provider_with_o_series("gemini-2.5-pro"), "gemini");
        assert_eq!(infer_provider_with_o_series("gpt-5"), "openai");
        assert_eq!(infer_provider_with_o_series("o3-mini"), "openai");
        assert_eq!(infer_provider_with_o_series("o4-mini"), "openai");
        assert_eq!(infer_provider_with_o_series("deepseek-v3"), "openrouter");
        assert_eq!(infer_provider_with_o_series("grok-imagine-video"), "openai");
        assert_eq!(infer_provider_with_o_series(""), "openai");
    }

    #[test]
    fn alias_map_resolution_accepts_strings_and_objects() {
        let aliases = json!({
            "fast": "xai/grok-imagine-video",
            "structured": {"provider": "openrouter", "model": "google/veo-3.1"},
            "broken": {"provider": "openrouter"},
        });
        assert_eq!(
            resolve_model_alias_from_map("fast", &aliases),
            Some(ModelInfo {
                provider: Some("xai".into()),
                model: "grok-imagine-video".into()
            })
        );
        assert_eq!(
            resolve_model_alias_from_map("structured", &aliases),
            Some(ModelInfo {
                provider: Some("openrouter".into()),
                model: "google/veo-3.1".into()
            })
        );
        assert_eq!(resolve_model_alias_from_map("broken", &aliases), None);
        assert_eq!(resolve_model_alias_from_map("missing", &aliases), None);
    }

    #[test]
    fn video_route_selection_matches_upstream_rules() {
        // Bare model ids fall back to xAI with the model forwarded verbatim.
        assert_eq!(
            select_video_route(
                "grok-imagine-video",
                &ModelInfo {
                    provider: Some("openai".into()),
                    model: "grok-imagine-video".into()
                },
                supports_video
            ),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: Some("grok-imagine-video".into())
            })
        );
        // Prefixed models must resolve to a video-capable provider.
        assert_eq!(
            select_video_route(
                "xai/grok-imagine-video",
                &ModelInfo {
                    provider: Some("xai".into()),
                    model: "grok-imagine-video".into()
                },
                supports_video
            ),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: Some("grok-imagine-video".into())
            })
        );
        assert_eq!(
            select_video_route(
                "openai/gpt-5.2",
                &ModelInfo {
                    provider: Some("openai".into()),
                    model: "gpt-5.2".into()
                },
                supports_video
            ),
            Err("Provider 'openai' does not support video generation".to_string())
        );
        // Combos are rejected with the upstream message.
        assert_eq!(
            select_video_route(
                "my-combo",
                &ModelInfo {
                    provider: None,
                    model: "my-combo".into()
                },
                supports_video
            ),
            Err("Combos are not supported for video generation".to_string())
        );
    }

    #[test]
    fn supported_video_providers_are_xai_openrouter_and_vertex() {
        assert!(supports_video("xai"));
        assert!(supports_video("openrouter"));
        assert!(supports_video("vertex"));
        // Registry aliases resolve through the catalog.
        assert!(supports_video("vx"));
        assert!(!supports_video("openai"));
        assert!(!supports_video("grok-cli"));
        // Upstream keys `PROVIDER_MEDIA` by canonical id only, so the raw
        // `provider`/`x-connection-id` lookups must reject aliases.
        assert!(supports_video_raw("vertex"));
        assert!(!supports_video_raw("vx"));
    }

    #[test]
    fn encode_uri_component_matches_javascript() {
        assert_eq!(encode_uri_component("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(encode_uri_component("a b"), "a%20b");
        assert_eq!(encode_uri_component("a+b"), "a%2Bb");
        assert_eq!(encode_uri_component("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
    }

    #[test]
    fn xai_plan_builds_creation_and_poll_requests() {
        let body = Bytes::from_static(br#"{"model":"grok-imagine-video","prompt":"hi"}"#);
        let plan = default_plan(
            &xai_config(),
            Some(VideoAction::Generations),
            None,
            Some(&body),
            Some("application/json"),
            Some("idem-1"),
            &credentials(None, Some("tok-12345678")),
        )
        .expect("xai plan");
        assert_eq!(plan.method, Method::POST);
        assert_eq!(plan.url, "https://api.x.ai/v1/videos/generations");
        assert_eq!(plan.header("Authorization"), Some("Bearer tok-12345678"));
        assert_eq!(plan.header("Content-Type"), Some("application/json"));
        assert_eq!(plan.header("Idempotency-Key"), Some("idem-1"));
        assert_eq!(plan.header("Accept"), Some("application/json"));
        assert_eq!(plan.body, Some(body));

        let plan = default_plan(
            &xai_config(),
            None,
            Some("req-1"),
            None,
            Some("application/json"),
            Some("idem-1"),
            &credentials(Some("key-12345678"), None),
        )
        .expect("xai poll plan");
        assert_eq!(plan.method, Method::GET);
        assert_eq!(plan.url, "https://api.x.ai/v1/videos/req-1");
        assert_eq!(plan.header("Authorization"), Some("Bearer key-12345678"));
        assert_eq!(plan.header("Content-Type"), None);
        assert_eq!(plan.header("Idempotency-Key"), None);
        assert_eq!(plan.body, None);
    }

    #[test]
    fn xai_plan_percent_encodes_poll_ids() {
        let plan = default_plan(
            &xai_config(),
            None,
            Some("a/b c"),
            None,
            None,
            None,
            &credentials(Some("key-12345678"), None),
        )
        .expect("xai poll plan");
        assert_eq!(plan.url, "https://api.x.ai/v1/videos/a%2Fb%20c");
    }

    #[test]
    fn openrouter_plan_is_generations_only_at_the_collection_root() {
        let body = Bytes::from_static(br#"{"model":"google/veo-3.1"}"#);
        let plan = openrouter_plan(
            &openrouter_config(),
            Some(VideoAction::Generations),
            None,
            Some(&body),
            Some("application/json"),
            &credentials(Some("sk-or-12345678"), None),
        )
        .expect("openrouter plan");
        assert_eq!(plan.method, Method::POST);
        assert_eq!(plan.url, "https://openrouter.ai/api/v1/videos");
        assert_eq!(plan.header("Authorization"), Some("Bearer sk-or-12345678"));
        assert_eq!(plan.header("Content-Type"), Some("application/json"));
        assert_eq!(
            plan.header("HTTP-Referer"),
            Some("https://endpoint-proxy.local")
        );
        assert_eq!(plan.header("X-Title"), Some("Endpoint Proxy"));

        let error = openrouter_plan(
            &openrouter_config(),
            Some(VideoAction::Edits),
            None,
            Some(&body),
            Some("application/json"),
            &credentials(Some("sk-or-12345678"), None),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "OpenRouter video supports 'generations' only (got 'edits')"
        );

        let error = openrouter_plan(
            &openrouter_config(),
            Some(VideoAction::Generations),
            None,
            Some(&body),
            Some("multipart/form-data; boundary=x"),
            &credentials(Some("sk-or-12345678"), None),
        )
        .unwrap_err();
        assert_eq!(error, "OpenRouter video requires an application/json body");

        let plan = openrouter_plan(
            &openrouter_config(),
            None,
            Some("job-42"),
            None,
            None,
            &credentials(Some("sk-or-12345678"), None),
        )
        .expect("openrouter poll plan");
        assert_eq!(plan.method, Method::GET);
        assert_eq!(plan.url, "https://openrouter.ai/api/v1/videos/job-42");
        assert_eq!(plan.header("Content-Type"), None);
    }

    #[test]
    fn vertex_job_ids_round_trip_and_reject_unsafe_values() {
        let name =
            "projects/p/locations/us-central1/publishers/google/models/veo-3.1/operations/abc";
        let encoded = vertex_encode_job_id(name);
        assert_eq!(vertex_decode_job_id(&encoded).as_deref(), Some(name));
        assert_eq!(vertex_encode_job_id(name).contains('='), false);

        // Reject non-canonical base64, invalid characters and non-resource names.
        assert_eq!(vertex_decode_job_id(""), None);
        assert_eq!(vertex_decode_job_id("not*allowed"), None);
        assert_eq!(
            vertex_decode_job_id(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("a/b")),
            None
        );
        assert_eq!(
            vertex_decode_job_id(&vertex_encode_job_id("projects/p/../../etc")),
            None
        );
        assert_eq!(vertex_decode_job_id(&"a".repeat(1025)), None);
        assert_eq!(
            vertex_model_path_of(name),
            Some("projects/p/locations/us-central1/publishers/google/models/veo-3.1")
        );
        assert_eq!(vertex_model_path_of("no-operations"), None);
    }

    #[test]
    fn vertex_body_translates_the_openai_video_shape() {
        let body = json!({
            "model": "veo-3.1-generate-preview",
            "prompt": "a cat",
            "n": "2",
            "duration": 8,
            "aspect_ratio": "16:9",
            "resolution": "1080p",
            "seed": 7,
            "negative_prompt": "blur",
            "storage_uri": "gs://bucket/out",
            "generate_audio": 1,
            "image": "data:image/png;base64,QUJD",
            "unknown": true,
        });
        assert_eq!(
            to_vertex_body(&body),
            json!({
                "instances": [{
                    "prompt": "a cat",
                    "image": {"bytesBase64Encoded": "QUJD", "mimeType": "image/png"},
                }],
                "parameters": {
                    "sampleCount": 2,
                    "durationSeconds": 8,
                    "aspectRatio": "16:9",
                    "resolution": "1080p",
                    "seed": 7,
                    "negativePrompt": "blur",
                    "storageUri": "gs://bucket/out",
                    "generateAudio": true,
                }
            })
        );

        // A bare string image is a GCS URI; `n` that is not numeric becomes null
        // (`JSON.stringify(NaN)`), and absent optional fields are omitted.
        assert_eq!(
            to_vertex_body(&json!({"prompt": "x", "image": "gs://b/i.png", "n": "abc"})),
            json!({
                "instances": [{"prompt": "x", "image": {"gcsUri": "gs://b/i.png"}}],
                "parameters": {"sampleCount": null}
            })
        );

        // No prompt/image keys at all → only the instance shell remains.
        assert_eq!(to_vertex_body(&json!({})), json!({"instances": [{}]}));
    }

    #[test]
    fn vertex_operation_normalisation_matches_the_async_job_shape() {
        let name =
            "projects/p/locations/us-central1/publishers/google/models/veo-3.1/operations/op-1";
        let id = vertex_encode_job_id(name);

        assert_eq!(
            from_vertex_operation(&json!({"name": name})),
            json!({"id": id, "request_id": id, "status": "pending"})
        );
        assert_eq!(
            from_vertex_operation(&json!({"name": name, "done": false})),
            json!({"id": id, "request_id": id, "status": "pending"})
        );
        assert_eq!(
            from_vertex_operation(&json!({"name": name, "error": {"code": 3, "message": "bad"}})),
            json!({
                "id": id,
                "request_id": id,
                "status": "failed",
                "error": {"code": 3, "message": "bad"}
            })
        );
        assert_eq!(
            from_vertex_operation(&json!({
                "name": name,
                "done": true,
                "response": {"videos": [{"gcsUri": "gs://b/out.mp4", "mimeType": "video/mp4"}]}
            })),
            json!({
                "id": id,
                "request_id": id,
                "status": "completed",
                "video": {"url": "gs://b/out.mp4", "b64_json": null, "mime_type": "video/mp4"},
                "videos": [{"url": "gs://b/out.mp4", "b64_json": null, "mime_type": "video/mp4"}]
            })
        );
        assert_eq!(
            from_vertex_operation(&json!({
                "name": name,
                "done": true,
                "response": {
                    "generateVideoResponse": {
                        "generatedSamples": [{
                            "video": {"uri": "gs://b/nested.mp4", "bytesBase64Encoded": "QUJD"}
                        }]
                    }
                }
            })),
            json!({
                "id": id,
                "request_id": id,
                "status": "completed",
                "video": {"url": "gs://b/nested.mp4", "b64_json": "QUJD", "mime_type": "video/mp4"},
                "videos": [{"url": "gs://b/nested.mp4", "b64_json": "QUJD", "mime_type": "video/mp4"}]
            })
        );
        assert_eq!(
            from_vertex_operation(&json!({
                "name": name,
                "done": true,
                "response": {"videos": [{"uri": "gs://b/one.mp4"}, {"uri": "gs://b/two.mp4"}]}
            }))["video"],
            json!({"url": "gs://b/one.mp4", "b64_json": null, "mime_type": "video/mp4"})
        );
        // No operation name → verbatim passthrough.
        assert_eq!(
            from_vertex_operation(&json!({"done": true})),
            json!({"done": true})
        );
    }

    #[test]
    fn vertex_auth_requires_service_account_json_or_access_token() {
        assert_eq!(
            parse_vertex_sa_json(Some(r#"{"type":"service_account"}"#)),
            None
        );
        assert_eq!(parse_vertex_sa_json(Some("not-json")), None);
        assert_eq!(parse_vertex_sa_json(None), None);
        let sa = json!({
            "type": "service_account",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n",
            "project_id": "p"
        });
        assert_eq!(
            parse_vertex_sa_json(Some(&sa.to_string())),
            Some(sa.clone())
        );
    }

    #[test]
    fn service_account_assertion_is_signed_with_rs256() {
        let sa = json!({
            "type": "service_account",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "private_key": TEST_RSA_KEY.replace('\n', "\\n"),
            "project_id": "p"
        });
        let assertion = build_sa_assertion(&sa, 1_700_000_000).expect("signed assertion");
        let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
        validation.set_audience(&[GOOGLE_TOKEN_ENDPOINT]);
        validation.set_issuer(&["sa@p.iam.gserviceaccount.com"]);
        // The assertion is minted for the caller-supplied timestamp, which is in
        // the past for a fixed test clock.
        validation.validate_exp = false;
        let decoded = jsonwebtoken::decode::<Value>(
            &assertion,
            &jsonwebtoken::DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_KEY.as_bytes())
                .expect("public key"),
            &validation,
        )
        .expect("verified assertion");
        assert_eq!(decoded.header.alg, Algorithm::RS256);
        assert_eq!(
            decoded.claims.get("scope").and_then(Value::as_str),
            Some("https://www.googleapis.com/auth/cloud-platform")
        );
        assert_eq!(
            decoded.claims.get("iat").and_then(Value::as_i64),
            Some(1_700_000_000)
        );
        assert_eq!(
            decoded.claims.get("exp").and_then(Value::as_i64),
            Some(1_700_003_600)
        );

        assert!(
            build_sa_assertion(&json!({"client_email": "x", "private_key": "broken"}), 0).is_err()
        );
    }

    #[test]
    fn upstream_success_bodies_are_passed_through_verbatim() {
        let credentials = credentials(Some("key-12345678"), None);
        let xai_job = r#"{"request_id":"11b1b6c1","status":"pending","progress":0}"#;
        assert_eq!(
            map_upstream_response(
                "xai",
                200,
                Some("application/json"),
                xai_job,
                PlanAdapter::Default,
                &credentials
            ),
            CoreOutcome::Success {
                status: 200,
                content_type: "application/json".into(),
                body: xai_job.into()
            }
        );
        // Missing upstream content type falls back to application/json.
        assert_eq!(
            map_upstream_response(
                "xai",
                202,
                None,
                r#"{"status":"processing"}"#,
                PlanAdapter::Default,
                &credentials
            ),
            CoreOutcome::Success {
                status: 202,
                content_type: "application/json".into(),
                body: r#"{"status":"processing"}"#.into()
            }
        );
    }

    #[test]
    fn vertex_success_bodies_are_normalised_from_fixtures() {
        let credentials = credentials(Some("key-12345678"), None);
        let name =
            "projects/p/locations/us-central1/publishers/google/models/veo-3.1/operations/op-1";
        let id = vertex_encode_job_id(name);
        let upstream = json!({
            "name": name,
            "done": true,
            "response": {"videos": [{"gcsUri": "gs://b/out.mp4"}]}
        })
        .to_string();
        let outcome = map_upstream_response(
            "vertex",
            200,
            Some("application/json; charset=utf-8"),
            &upstream,
            PlanAdapter::Vertex,
            &credentials,
        );
        assert_eq!(
            outcome,
            CoreOutcome::Success {
                status: 200,
                content_type: "application/json".into(),
                body: json!({
                    "id": id,
                    "request_id": id,
                    "status": "completed",
                    "video": {"url": "gs://b/out.mp4", "b64_json": null, "mime_type": "video/mp4"},
                    "videos": [{"url": "gs://b/out.mp4", "b64_json": null, "mime_type": "video/mp4"}]
                })
                .to_string()
            }
        );
        // A non-JSON success body falls back to the raw upstream text.
        assert_eq!(
            map_upstream_response(
                "vertex",
                200,
                Some("text/plain"),
                "not json",
                PlanAdapter::Vertex,
                &credentials
            ),
            CoreOutcome::Success {
                status: 200,
                content_type: "text/plain".into(),
                body: "not json".into()
            }
        );
    }

    #[test]
    fn upstream_errors_use_the_prefixed_sanitised_envelope() {
        let credentials = VideoCredentials {
            api_key: Some("sk-live-abcdefgh".into()),
            access_token: Some("access-token-12345678".into()),
            ..VideoCredentials::default()
        };
        assert_eq!(
            map_upstream_response(
                "xai",
                401,
                Some("application/json"),
                r#"{"error":"Invalid API key: sk-live-abcdefgh"}"#,
                PlanAdapter::Default,
                &credentials
            ),
            CoreOutcome::Failure {
                status: 401,
                error: r#"[xai] {"error":"Invalid API key: [redacted]"}"#.into()
            }
        );
        // An empty upstream body surfaces the status line, and a bearer token in
        // the body is redacted.
        assert_eq!(
            map_upstream_response("xai", 500, None, "", PlanAdapter::Default, &credentials),
            CoreOutcome::Failure {
                status: 500,
                error: "[xai] HTTP 500".into()
            }
        );
        assert_eq!(
            map_upstream_response(
                "openrouter",
                429,
                None,
                "slow down, Authorization: Bearer access-token-12345678",
                PlanAdapter::OpenRouter,
                &credentials
            ),
            CoreOutcome::Failure {
                status: 429,
                error: "[openrouter] slow down, Authorization: Bearer [redacted]".into()
            }
        );
    }

    #[test]
    fn upstream_error_bodies_are_truncated_to_2000_units() {
        let credentials = credentials(Some("k"), None);
        let long = "x".repeat(2500);
        let outcome =
            map_upstream_response("xai", 400, None, &long, PlanAdapter::Default, &credentials);
        assert_eq!(
            outcome,
            CoreOutcome::Failure {
                status: 400,
                error: format!("[xai] {}", "x".repeat(2000))
            }
        );
    }

    #[test]
    fn sanitize_secrets_matches_upstream_rules() {
        let credentials = VideoCredentials {
            api_key: Some("short".into()),
            access_token: Some("token-abcdefgh".into()),
            refresh_token: Some("refresh-abcdefgh".into()),
            ..VideoCredentials::default()
        };
        assert_eq!(sanitize_secrets("", Some(&credentials)), "");
        assert_eq!(
            sanitize_secrets(
                "Bearer abcdefghijkl and token-abcdefgh plus short plus refresh-abcdefgh",
                Some(&credentials)
            ),
            "Bearer [redacted] and [redacted] plus short plus [redacted]"
        );
        assert_eq!(sanitize_secrets("keep me", None), "keep me");
    }

    #[test]
    fn forwardable_bodies_preserve_bytes_and_reject_bad_json() {
        let json_body = Bytes::from_static(br#"{"model":"xai/grok-imagine-video"}"#);
        let body = read_forwardable_body("application/json", &json_body).expect("json body");
        assert_eq!(body.raw, json_body);
        assert_eq!(
            body.parsed,
            Some(json!({"model": "xai/grok-imagine-video"}))
        );

        let error =
            read_forwardable_body("application/json", &Bytes::from_static(b"{")).unwrap_err();
        assert_eq!(error, "Invalid JSON body");

        // Multipart is forwarded byte-for-byte without parsing.
        let multipart = Bytes::from_static(b"--boundary\r\ncontent\r\n--boundary--");
        let body = read_forwardable_body("multipart/form-data; boundary=boundary", &multipart)
            .expect("multipart body");
        assert_eq!(body.raw, multipart);
        assert_eq!(body.parsed, None);
        assert_eq!(body.content_type, "multipart/form-data; boundary=boundary");
    }

    #[tokio::test]
    async fn success_responses_echo_the_connection_header() {
        let response = success_response(
            CoreOutcome::Success {
                status: 200,
                content_type: "application/json".into(),
                body: r#"{"request_id":"r1","status":"pending"}"#.into(),
            },
            Some("conn-42"),
        )
        .expect("success response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(response.headers()["x-9router-connection-id"], "conn-42");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(&body[..], br#"{"request_id":"r1","status":"pending"}"#);

        // Without a connection id there is no echo header, and unpinned failure
        // payloads keep the OpenAI error envelope.
        let response = success_response(
            CoreOutcome::Success {
                status: 202,
                content_type: "application/json".into(),
                body: "{}".into(),
            },
            None,
        )
        .expect("success response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(response.headers().get("x-9router-connection-id").is_none());

        let response = success_response(
            CoreOutcome::failure(429, "[xai] rate limited"),
            Some("conn-42"),
        )
        .expect("failure response");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(
            String::from_utf8_lossy(&body),
            r#"{"error":{"code":"rate_limit_exceeded","message":"[xai] rate limited","type":"rate_limit_error"}}"#
        );
    }

    #[test]
    fn model_strings_follow_javascript_truthiness() {
        assert_eq!(js_model_string(&json!("x")), Some("x".into()));
        assert_eq!(js_model_string(&json!(8)), Some("8".into()));
        assert_eq!(js_model_string(&json!(true)), Some("true".into()));
        assert_eq!(js_model_string(&json!("")), None);
        assert_eq!(js_model_string(&json!(0)), None);
        assert_eq!(js_model_string(&json!(null)), None);
        assert_eq!(js_model_string(&json!(false)), None);
        assert_eq!(js_model_string(&json!({})), None);
    }

    #[test]
    fn provider_resolution_uses_the_database_state() {
        let (_temp, state) = test_state();

        // No model field / empty bodies → the xAI default with no model.
        assert_eq!(
            resolve_video_provider(&state, None),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: None
            })
        );
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"prompt": "hi"}))),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: None
            })
        );
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": ""}))),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: None
            })
        );

        // Prefixed model → provider + prefix-stripped model.
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": "xai/grok-imagine-video"}))),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: Some("grok-imagine-video".into())
            })
        );
        // Bare model id → default provider with the id forwarded verbatim.
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": "grok-imagine-video"}))),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: Some("grok-imagine-video".into())
            })
        );
        // Unsupported prefixed provider.
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": "openai/gpt-5.2"}))),
            Err("Provider 'openai' does not support video generation".to_string())
        );

        // A configured combo name is rejected before any provider lookup.
        state
            .db
            .upsert_combo(json!({"name": "cine", "models": ["xai/grok-imagine-video"]}))
            .expect("create combo");
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": "cine"}))),
            Err("Combos are not supported for video generation".to_string())
        );

        // Configured model aliases win over prefix inference.
        state
            .db
            .kv_set("modelAliases", "fast", &json!("xai/grok-imagine-video"))
            .expect("set alias");
        assert_eq!(
            resolve_video_provider(&state, Some(&json!({"model": "fast"}))),
            Ok(ResolvedVideoProvider {
                provider: "xai".into(),
                model: Some("grok-imagine-video".into())
            })
        );
    }

    #[test]
    fn credentials_come_from_the_connection_row() {
        let connection = json!({
            "id": "conn-9",
            "provider": "vertex",
            "apiKey": "sa-json",
            "accessToken": "",
            "refreshToken": "refresh-1",
            "projectId": "proj-1",
            "providerSpecificData": {"location": "europe-west1"}
        });
        let credentials = credentials_from_connection(&connection);
        assert_eq!(credentials.connection_id, "conn-9");
        assert_eq!(credentials.api_key.as_deref(), Some("sa-json"));
        assert_eq!(credentials.access_token, None);
        assert_eq!(credentials.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(credentials.project_id.as_deref(), Some("proj-1"));
        assert_eq!(
            credentials
                .provider_specific_data
                .as_ref()
                .and_then(|data| data.get("location"))
                .and_then(Value::as_str),
            Some("europe-west1")
        );
    }

    #[test]
    fn poll_provider_resolution_prefers_the_pinned_connection() {
        let (_temp, state) = test_state();
        state
            .db
            .create_connection(json!({
                "provider": "openrouter",
                "apiKey": "sk-or-12345678",
                "isActive": true
            }))
            .expect("create connection");

        // Pinned connection wins, `?provider=` is honoured, everything else
        // falls back to xAI.
        let connection_id = state
            .db
            .provider_connections(Some("openrouter"), Some(true))
            .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            resolve_get_provider(&state, None, Some(&connection_id)),
            "openrouter"
        );
        assert_eq!(
            resolve_get_provider(&state, Some("provider=vertex"), None),
            "vertex"
        );
        assert_eq!(
            resolve_get_provider(&state, Some("provider=openai"), None),
            "xai"
        );
        assert_eq!(
            resolve_get_provider(&state, Some("provider=vertex"), Some("missing")),
            "vertex"
        );
        assert_eq!(resolve_get_provider(&state, None, None), "xai");
    }
}
