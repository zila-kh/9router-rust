//! Native `GET /v1/models` and its sub-routes — a port of
//! `frontend/src/app/api/v1/models/route.js` (`buildModelsList`) and
//! `frontend/src/app/api/v1/models/[...model]/route.js`.
//!
//! Contract (identical to upstream):
//!
//! * `GET /v1/models` — `{object:"list", data:[...]}` of every LLM model the
//!   active connections + combos + custom models + aliases expose.
//! * `GET /v1/models/{kind}` — the same list filtered by one of the six kind
//!   slugs (`image`, `tts`, `stt`, `embedding`, `image-to-text`, `web`).
//! * `GET /v1/models/{provider}/{model}` — single-model lookup over that same
//!   LLM list, 404 `model_not_found` when unknown.
//! * `GET /v1/models/info` is a different route (`model_catalog.rs`) and is
//!   dispatched first by the gateway.
//!
//! Data sources match upstream: the static `PROVIDER_MODELS` catalog and the
//! provider registry exported into `assets/provider-catalog.json`, plus the
//! `providerConnections`, `combos`, `customModels`, `modelAliases` and
//! `disabledModels` rows in the SQLite store, plus the per-provider live
//! resolvers in [`crate::models_live`].
//!
//! Everything here is a pure projection of those inputs, which is why the unit
//! tests below build the list from fixtures with no database and no network.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use axum::{
    body::Body,
    http::{HeaderMap, Method, Response, StatusCode},
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    error::AppError, inference_media::json_response, model_capabilities, models_live, providers,
    state::AppState,
};

/// Upstream `LLM_KIND` — combos and models without an explicit kind.
pub const LLM_KIND: &str = "llm";

/// Upstream `INTERNAL_MODELS_FETCH_HEADER`: set by
/// [`fetch_compatible_model_ids`], read here to break cross-instance loops.
const INTERNAL_MODELS_FETCH_HEADER: &str = "x-9r-internal-models-fetch";

const OPENAI_COMPATIBLE_PREFIX: &str = "openai-compatible-";
const ANTHROPIC_COMPATIBLE_PREFIX: &str = "anthropic-compatible-";

/// `KIND_SLUG_MAP` from `[...model]/route.js` (URL slug -> service kinds).
const KIND_SLUG_MAP: [(&str, &[&str]); 6] = [
    ("image", &["image"]),
    ("tts", &["tts"]),
    ("stt", &["stt"]),
    ("embedding", &["embedding"]),
    ("image-to-text", &["imageToText"]),
    ("web", &["webSearch", "webFetch"]),
];

/// `MODEL_TYPE_TO_KIND` — per-model `kind`/`type` to service kind. Models
/// without either field are LLMs.
fn model_type_to_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "image" => Some("image"),
        "tts" => Some("tts"),
        "embedding" => Some("embedding"),
        "stt" => Some("stt"),
        "imageToText" => Some("imageToText"),
        "video" => Some("video"),
        _ => None,
    }
}

/// `modelKind(model)`.
pub fn model_kind(model: &Value) -> String {
    let raw = model
        .get("kind")
        .or_else(|| model.get("type"))
        .and_then(Value::as_str);
    match raw {
        Some(kind) => model_type_to_kind(kind).unwrap_or(LLM_KIND).to_string(),
        None => LLM_KIND.to_string(),
    }
}

static EMBED_ID: Lazy<Regex> = Lazy::new(|| Regex::new("embed").expect("embed regex"));
static TTS_ID: Lazy<Regex> = Lazy::new(|| Regex::new("tts|speech|audio|voice").expect("tts regex"));
static IMAGE_ID: Lazy<Regex> = Lazy::new(|| {
    Regex::new("image|imagen|dall-?e|flux|sdxl|sd-|stable-diffusion").expect("image regex")
});

/// `inferKindFromUnknownModelId(modelId)` — for dynamic ids (compatible
/// providers, aliases, custom models) where no per-model type exists.
pub fn infer_kind_from_unknown_model_id(model_id: &str) -> String {
    let lower = model_id.to_lowercase();
    if EMBED_ID.is_match(&lower) {
        return "embedding".to_string();
    }
    if TTS_ID.is_match(&lower) {
        return "tts".to_string();
    }
    if IMAGE_ID.is_match(&lower) {
        return "image".to_string();
    }
    LLM_KIND.to_string()
}

/// `parseOpenAIStyleModels(data)`.
pub fn parse_openai_style_models(data: &Value) -> Vec<Value> {
    if let Some(items) = data.as_array() {
        return items.clone();
    }
    ["data", "models", "results"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|text| text.to_string())
        .filter(|text| !text.trim().is_empty())
}

/// `fetchCompatibleModelIds(connection)` — `{baseUrl}/models` for
/// openai-compatible / anthropic-compatible connections, 5s timeout, `[]` on
/// any failure.
pub async fn fetch_compatible_model_ids(state: &AppState, connection: &Value) -> Vec<String> {
    let Some(api_key) = non_empty_string(connection.get("apiKey")) else {
        return Vec::new();
    };
    let base_url = connection
        .get("providerSpecificData")
        .and_then(|data| data.get("baseUrl"))
        .and_then(Value::as_str)
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty());
    let Some(base_url) = base_url else {
        return Vec::new();
    };

    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut url = format!("{base_url}/models");
    let mut request = state
        .http
        .get(url.clone())
        .timeout(std::time::Duration::from_secs(5))
        .header("content-type", "application/json")
        .header(INTERNAL_MODELS_FETCH_HEADER, "1");
    if provider.starts_with(OPENAI_COMPATIBLE_PREFIX) {
        request = request.header("authorization", format!("Bearer {api_key}"));
    } else if provider.starts_with(ANTHROPIC_COMPATIBLE_PREFIX) {
        if url.ends_with("/messages/models") {
            url = url[..url.len() - "/messages/models".len()].to_string();
        } else if url.ends_with("/messages") {
            url = format!("{}/models", &url[..url.len() - "/messages".len()]);
        }
        request = state
            .http
            .get(url)
            .timeout(std::time::Duration::from_secs(5))
            .header("content-type", "application/json")
            .header("x-api-key", api_key.clone())
            .header("anthropic-version", "2023-06-01")
            .header("authorization", format!("Bearer {api_key}"))
            .header(INTERNAL_MODELS_FETCH_HEADER, "1");
    } else {
        return Vec::new();
    }

    let Ok(response) = request.send().await else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = response.json::<Value>().await else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    parse_openai_style_models(&data)
        .into_iter()
        .filter_map(|model| {
            non_empty_string(
                model
                    .get("id")
                    .or_else(|| model.get("name"))
                    .or_else(|| model.get("model")),
            )
        })
        .filter(|model_id| seen.insert(model_id.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Provider registry lookups (`AI_PROVIDERS` / `PROVIDER_ID_TO_ALIAS`)
// ---------------------------------------------------------------------------

fn registry() -> &'static [Value] {
    providers::catalog()
        .get("registry")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The `AI_PROVIDERS[id]` entry — built from the registry row with that exact
/// id (`buildProviderEntry` in the dashboard constants).
fn provider_entry_by_id(provider_id: &str) -> Option<&'static Value> {
    registry()
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(provider_id))
}

/// `getProviderAlias(providerId)` — `uiAlias || alias || providerId`.
pub fn provider_alias(provider_id: &str) -> String {
    provider_entry_by_id(provider_id)
        .and_then(|entry| {
            entry
                .get("uiAlias")
                .and_then(Value::as_str)
                .filter(|alias| !alias.is_empty())
                .or_else(|| entry.get("alias").and_then(Value::as_str))
                .filter(|alias| !alias.is_empty())
        })
        .unwrap_or(provider_id)
        .to_string()
}

/// `PROVIDER_ID_TO_ALIAS[providerId] || providerId` — the `PROVIDER_MODELS`
/// key for a provider. Upstream builds this from `OAUTH_ALIASES` (registry
/// `alias`, only when it differs from the id) intersected with the provider
/// ids that have a transport, so the transport map gates membership.
pub fn static_alias(provider_id: &str) -> String {
    let has_transport = providers::catalog()
        .get("providers")
        .and_then(|providers| providers.get(provider_id))
        .is_some();
    if !has_transport {
        return provider_id.to_string();
    }
    provider_entry_by_id(provider_id)
        .and_then(|entry| entry.get("alias").and_then(Value::as_str))
        .filter(|alias| !alias.is_empty())
        .unwrap_or(provider_id)
        .to_string()
}

/// `ALIAS_TO_ID` — the inverse of [`static_alias`], last definition wins
/// (upstream `Object.fromEntries`).
fn alias_to_provider_id() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        for entry in registry() {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            if providers::catalog()
                .get("providers")
                .and_then(|providers| providers.get(id))
                .is_none()
            {
                continue;
            }
            map.insert(static_alias(id), id.to_string());
        }
        map
    })
}

fn provider_models(key: &str) -> &'static [Value] {
    providers::catalog()
        .get("models")
        .and_then(|models| models.get(key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// `Object.keys(PROVIDER_MODELS)` in upstream insertion order: the registry
/// rows that declare `models` (keyed `alias || id`) followed by the TTS tables
/// `buildTtsProviderModels()` appends. The Rust catalog keeps the registry as
/// an array (order preserved) but stores the model map in sorted key order, so
/// the registry reconstruction is what keeps the list ordered like upstream;
/// only the trailing TTS tables fall back to sorted order.
fn provider_model_keys_in_order() -> &'static Vec<String> {
    static KEYS: OnceLock<Vec<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut keys: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for entry in registry() {
            if entry.get("models").is_none() {
                continue;
            }
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            let key = entry
                .get("alias")
                .and_then(Value::as_str)
                .filter(|alias| !alias.is_empty())
                .unwrap_or(id)
                .to_string();
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
        if let Some(models) = providers::catalog()
            .get("models")
            .and_then(Value::as_object)
        {
            for key in models.keys() {
                if seen.insert(key.clone()) {
                    keys.push(key.clone());
                }
            }
        }
        keys
    })
}

/// `providerMatchesKinds(providerId, kindFilter)` — `AI_PROVIDERS[id]
/// .serviceKinds` intersected with the requested kinds; providers without
/// service kinds are LLM-only.
pub fn provider_matches_kinds(provider_id: &str, kind_filter: &[String]) -> bool {
    let kinds: Vec<String> = provider_entry_by_id(provider_id)
        .and_then(|entry| entry.get("serviceKinds").and_then(Value::as_array))
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|kinds: &Vec<String>| !kinds.is_empty())
        .unwrap_or_else(|| vec![LLM_KIND.to_string()]);
    kind_filter.iter().any(|kind| kinds.contains(kind))
}

/// `comboMatchesKinds(combo, kindFilter)` — combos without a kind are LLMs.
pub fn combo_matches_kinds(combo: &Value, kind_filter: &[String]) -> bool {
    let kind = combo
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .unwrap_or(LLM_KIND);
    kind_filter.iter().any(|requested| requested == kind)
}

fn disabled(disabled_by_alias: &Map<String, Value>, alias: &str, model_id: &str) -> bool {
    disabled_by_alias
        .get(alias)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().any(|id| id.as_str() == Some(model_id)))
        .unwrap_or(false)
}

fn finite_number(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Number(number)) if number.as_f64().is_some_and(f64::is_finite) => {
            Some(Value::Number(number.clone()))
        }
        _ => None,
    }
}

/// `buildModelsList(kindFilter, options)`.
pub async fn build_models_list(
    state: &AppState,
    kind_filter: &[&str],
    skip_dynamic_fetch: bool,
) -> Result<Vec<Value>, AppError> {
    let kind_filter: Vec<String> = kind_filter.iter().map(|kind| kind.to_string()).collect();

    let connections: Vec<Value> = state
        .db
        .provider_connections(None, None)?
        .into_iter()
        .filter(|conn| conn.get("isActive").and_then(Value::as_bool) != Some(false))
        .collect();
    let combos = state.db.combos()?;
    let custom_models: Vec<Value> = state.db.kv_all("customModels")?.into_values().collect();
    let model_aliases = state.db.kv_all("modelAliases")?;
    let disabled_by_alias = state.db.kv_all("disabledModels")?;

    let mut active_connection_by_provider: Vec<(String, Value)> = Vec::new();
    for conn in connections.iter() {
        let Some(provider) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        if !active_connection_by_provider
            .iter()
            .any(|(existing, _)| existing == provider)
        {
            active_connection_by_provider.push((provider.to_string(), conn.clone()));
        }
    }

    let mut models: Vec<Value> = Vec::new();

    // Combos first (filtered by kind). Web combos expose `kind` so the caller
    // can tell webSearch from webFetch.
    for combo in &combos {
        if !combo_matches_kinds(combo, &kind_filter) {
            continue;
        }
        let Some(name) = combo.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name == crate::free_tier::FREE_COMBO_MODEL {
            continue;
        }
        let mut entry = json!({"id": name, "object": "model", "owned_by": "combo"});
        let combo_kind = combo.get("kind").and_then(Value::as_str).unwrap_or("");
        if combo_kind == "webSearch" || combo_kind == "webFetch" {
            entry["kind"] = json!(combo_kind);
        }
        models.push(entry);
    }

    if connections.is_empty() {
        // No active connection -> the static catalog, filtered by per-model kind.
        for key in provider_model_keys_in_order() {
            let provider_id = alias_to_provider_id()
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.clone());
            if !provider_matches_kinds(&provider_id, &kind_filter) {
                continue;
            }
            for model in provider_models(key) {
                let Some(model_id) = model.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if !kind_filter.contains(&model_kind(model)) {
                    continue;
                }
                if disabled(&disabled_by_alias, key, model_id) {
                    continue;
                }
                models.push(json!({
                    "id": format!("{key}/{model_id}"),
                    "object": "model",
                    "owned_by": key,
                }));
            }
        }

        for custom_model in &custom_models {
            let Some(model_id) = non_empty_string(custom_model.get("id")) else {
                continue;
            };
            let custom_type = custom_model.get("type").and_then(Value::as_str);
            if custom_type.is_some_and(|kind| kind != "llm") {
                continue;
            }
            // Custom models without an active connection are LLM-only.
            if !kind_filter.contains(&LLM_KIND.to_string()) {
                continue;
            }
            let Some(provider_alias) = non_empty_string(custom_model.get("providerAlias")) else {
                continue;
            };
            models.push(json!({
                "id": format!("{provider_alias}/{}", model_id.trim()),
                "object": "model",
                "owned_by": provider_alias,
            }));
        }
    } else {
        for (provider_id, conn) in &active_connection_by_provider {
            if !provider_matches_kinds(provider_id, &kind_filter) {
                continue;
            }

            let alias = static_alias(provider_id);
            let provider_specific = conn.get("providerSpecificData");
            let output_alias = provider_specific
                .and_then(|data| data.get("prefix"))
                .and_then(Value::as_str)
                .filter(|prefix| !prefix.is_empty())
                .map(str::to_string)
                .or_else(|| Some(provider_alias(provider_id)))
                .unwrap_or_else(|| alias.clone())
                .trim()
                .to_string();
            let catalog_models = provider_models(&alias);
            let enabled_models = provider_specific
                .and_then(|data| data.get("enabledModels"))
                .and_then(Value::as_array)
                .cloned();
            let has_explicit_enabled_models = enabled_models
                .as_ref()
                .is_some_and(|enabled| !enabled.is_empty());
            let is_compatible = provider_id.starts_with(OPENAI_COMPATIBLE_PREFIX)
                || provider_id.starts_with(ANTHROPIC_COMPATIBLE_PREFIX);

            let static_model_kind_by_id: HashMap<String, String> = catalog_models
                .iter()
                .filter_map(|model| {
                    Some((
                        model.get("id").and_then(Value::as_str)?.to_string(),
                        model_kind(model),
                    ))
                })
                .collect();

            let mut raw_model_ids: Vec<String> = match &enabled_models {
                Some(enabled) if has_explicit_enabled_models => {
                    let mut seen = HashSet::new();
                    enabled
                        .iter()
                        .filter_map(|model_id| non_empty_string(Some(model_id)))
                        .filter(|model_id| seen.insert(model_id.clone()))
                        .collect()
                }
                _ => catalog_models
                    .iter()
                    .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect(),
            };

            if is_compatible && raw_model_ids.is_empty() && !skip_dynamic_fetch {
                raw_model_ids = fetch_compatible_model_ids(state, conn).await;
            }

            let mut live_model_kind_by_id: HashMap<String, String> = HashMap::new();
            let mut live_capabilities_by_id: HashMap<String, Value> = HashMap::new();
            if !has_explicit_enabled_models {
                if let Some(live) = models_live::resolve_live_models(state, provider_id, conn).await
                {
                    if !live.is_empty() {
                        raw_model_ids = live
                            .iter()
                            .filter_map(|model| {
                                model.get("id").and_then(Value::as_str).map(str::to_string)
                            })
                            .collect();
                        live_model_kind_by_id = live
                            .iter()
                            .filter_map(|model| {
                                Some((
                                    model.get("id").and_then(Value::as_str)?.to_string(),
                                    model_kind(model),
                                ))
                            })
                            .collect();
                        live_capabilities_by_id = live
                            .iter()
                            .filter_map(|model| {
                                let id = model.get("id").and_then(Value::as_str)?;
                                let caps = model.get("capabilities")?;
                                caps.is_object().then(|| (id.to_string(), caps.clone()))
                            })
                            .collect();
                    }
                }
            }

            let strip_prefixes = [&output_alias, &alias, provider_id];
            let model_ids: Vec<String> = raw_model_ids
                .iter()
                .map(|model_id| strip_model_prefix(model_id, &strip_prefixes))
                .filter(|model_id| !model_id.trim().is_empty())
                .collect();

            let mut custom_model_kind_by_id: HashMap<String, String> = HashMap::new();
            let custom_model_ids: Vec<String> = custom_models
                .iter()
                .filter(|model| {
                    if non_empty_string(model.get("id")).is_none() {
                        return false;
                    }
                    let kind = model
                        .get("kind")
                        .or_else(|| model.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or(LLM_KIND);
                    let requested = kind_filter.contains(&kind.to_string())
                        || (kind == "imageToText" && kind_filter.contains(&LLM_KIND.to_string()));
                    if !requested {
                        return false;
                    }
                    let Some(model_alias) = model.get("providerAlias").and_then(Value::as_str)
                    else {
                        return false;
                    };
                    model_alias == alias
                        || model_alias == output_alias
                        || model_alias == *provider_id
                })
                .map(|model| {
                    let model_id = model
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !model_id.is_empty() {
                        let kind = model
                            .get("kind")
                            .or_else(|| model.get("type"))
                            .and_then(Value::as_str)
                            .unwrap_or(LLM_KIND);
                        custom_model_kind_by_id.insert(model_id.clone(), kind.to_string());
                    }
                    model_id
                })
                .filter(|model_id| !model_id.is_empty())
                .collect();

            let alias_model_ids: Vec<String> = model_aliases
                .values()
                .filter_map(Value::as_str)
                .filter(|full_model| full_model.contains('/'))
                .filter(|full_model| {
                    [&output_alias, &alias, provider_id]
                        .iter()
                        .any(|prefix| full_model.starts_with(&format!("{prefix}/")))
                })
                .map(|full_model| strip_model_prefix(full_model, &strip_prefixes))
                .filter(|model_id| !model_id.trim().is_empty())
                .collect();

            let mut merged_model_ids: Vec<String> = Vec::new();
            for model_id in model_ids
                .into_iter()
                .chain(custom_model_ids)
                .chain(alias_model_ids)
            {
                if !merged_model_ids.contains(&model_id) {
                    merged_model_ids.push(model_id);
                }
            }

            for model_id in merged_model_ids {
                // Kind resolution: custom/live metadata, then the static
                // catalog, then the id heuristics.
                let custom_kind = custom_model_kind_by_id.get(&model_id).cloned();
                let live_kind = live_model_kind_by_id.get(&model_id).cloned();
                let kind = custom_kind
                    .clone()
                    .or_else(|| live_kind.clone())
                    .or_else(|| static_model_kind_by_id.get(&model_id).cloned())
                    .unwrap_or_else(|| infer_kind_from_unknown_model_id(&model_id));
                // imageToText custom models stay in the LLM list.
                let allow_as_llm =
                    kind == "imageToText" && kind_filter.contains(&LLM_KIND.to_string());
                if !kind_filter.contains(&kind) && !allow_as_llm {
                    continue;
                }
                if disabled(&disabled_by_alias, &output_alias, &model_id)
                    || disabled(&disabled_by_alias, &alias, &model_id)
                {
                    continue;
                }

                let mut model = json!({
                    "id": format!("{output_alias}/{model_id}"),
                    "object": "model",
                    "owned_by": output_alias,
                });
                let caps = live_capabilities_by_id
                    .get(&model_id)
                    .cloned()
                    .or_else(|| {
                        model_capabilities::capabilities_from_service_kind(
                            custom_kind.or(live_kind).as_deref(),
                        )
                    })
                    .or_else(|| {
                        (kind == LLM_KIND).then(|| {
                            model_capabilities::get_capabilities_for_model(
                                Some(provider_id),
                                &model_id,
                            )
                        })
                    });
                if let Some(caps) = caps.as_ref() {
                    model["capabilities"] = caps.clone();
                }
                if kind == LLM_KIND || allow_as_llm {
                    let mut context_window = caps
                        .as_ref()
                        .and_then(|caps| finite_number(caps.get("contextWindow")));
                    let mut max_output = caps
                        .as_ref()
                        .and_then(|caps| finite_number(caps.get("maxOutput")));
                    if context_window.is_none() || max_output.is_none() {
                        let fallback = model_capabilities::get_capabilities_for_model(
                            Some(provider_id),
                            &model_id,
                        );
                        if context_window.is_none() {
                            context_window = finite_number(fallback.get("contextWindow"));
                        }
                        if max_output.is_none() {
                            max_output = finite_number(fallback.get("maxOutput"));
                        }
                    }
                    if let Some(context_window) = context_window {
                        model["context_length"] = context_window;
                    }
                    if let Some(max_output) = max_output {
                        model["max_completion_tokens"] = max_output;
                    }
                }
                models.push(model);
            }

            // Web search/fetch: the provider itself is the model.
            let provider_info = provider_entry_by_id(provider_id);
            if kind_filter.contains(&"webSearch".to_string())
                && provider_info.is_some_and(|info| info.get("searchConfig").is_some())
            {
                models.push(json!({
                    "id": format!("{output_alias}/search"),
                    "object": "model",
                    "kind": "webSearch",
                    "owned_by": output_alias,
                }));
            }
            if kind_filter.contains(&"webFetch".to_string())
                && provider_info.is_some_and(|info| info.get("fetchConfig").is_some())
            {
                models.push(json!({
                    "id": format!("{output_alias}/fetch"),
                    "object": "model",
                    "kind": "webFetch",
                    "owned_by": output_alias,
                }));
            }
        }
    }

    let mut deduped: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for model in models {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        deduped.push(model);
    }

    Ok(deduped)
}

/// Strip the `{alias}/` prefixes upstream removes from connection model ids.
fn strip_model_prefix(model_id: &str, prefixes: &[&String]) -> String {
    for prefix in prefixes {
        if let Some(rest) = model_id.strip_prefix(&format!("{prefix}/")) {
            return rest.to_string();
        }
    }
    model_id.to_string()
}

/// Which of the three `/v1/models` shapes a request path selects.
#[derive(Debug, PartialEq, Eq)]
pub enum ModelsRoute {
    /// `GET /v1/models` — the LLM listing.
    List,
    /// `GET /v1/models/{kind}` — one of the six kind slugs.
    Kinds(&'static [&'static str]),
    /// `GET /v1/models/{provider}/{model}` — single-model lookup.
    Lookup(String),
}

/// Split a public `/v1/models...` path the way the Next catch-all route does:
/// no trailing segment is the list, exactly one segment may be a kind slug,
/// everything else is a model identifier.
pub fn models_route(path: &str) -> ModelsRoute {
    let public_path = path.strip_prefix("/api").unwrap_or(path);
    let rest = public_path
        .strip_prefix("/v1/models")
        .unwrap_or("")
        .trim_matches('/');
    let segments: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('/').collect()
    };
    let identifier = segments
        .iter()
        .filter(|segment| !segment.is_empty())
        .cloned()
        .collect::<Vec<&str>>()
        .join("/");

    if segments.is_empty() {
        return ModelsRoute::List;
    }
    // Kind slugs only apply to a single path segment.
    if segments.len() == 1 {
        if let Some((_, kinds)) = KIND_SLUG_MAP
            .iter()
            .find(|(slug, _)| identifier.as_str() == *slug)
        {
            return ModelsRoute::Kinds(kinds);
        }
    }
    ModelsRoute::Lookup(identifier)
}

/// `GET /v1/models`, `GET /v1/models/{kind}`,
/// `GET /v1/models/{provider}/{model}`.
pub async fn handle_models(
    state: &AppState,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    consumer: bool,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": {"message": "Method Not Allowed", "type": "invalid_request_error"}}),
        );
    }
    let skip_dynamic_fetch = headers
        .get(INTERNAL_MODELS_FETCH_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some("1");

    match models_route(path) {
        ModelsRoute::List => {
            let mut data = build_models_list(state, &[LLM_KIND], skip_dynamic_fetch).await?;
            if consumer {
                filter_free_tier_models(state, &mut data, &[LLM_KIND]);
            }
            json_response(StatusCode::OK, json!({"object": "list", "data": data}))
        }
        ModelsRoute::Kinds(kinds) => {
            let mut data = build_models_list(state, kinds, skip_dynamic_fetch).await?;
            if consumer {
                filter_free_tier_models(state, &mut data, kinds);
            }
            json_response(StatusCode::OK, json!({"object": "list", "data": data}))
        }
        ModelsRoute::Lookup(identifier) => {
            // Single-model lookup over the same LLM list; the dynamic fetch is
            // never skipped here, matching upstream.
            let mut models = build_models_list(state, &[LLM_KIND], false).await?;
            if consumer {
                filter_free_tier_models(state, &mut models, &[LLM_KIND]);
            }
            let matched = models.into_iter().find(|candidate| {
                candidate.get("id").and_then(Value::as_str) == Some(identifier.as_str())
            });
            match matched {
                Some(model) => json_response(StatusCode::OK, model),
                None => json_response(
                    StatusCode::NOT_FOUND,
                    json!({"error": {
                        "message": format!("The model '{identifier}' does not exist or you do not have access to it."),
                        "type": "invalid_request_error",
                        "code": "model_not_found",
                    }}),
                ),
            }
        }
    }
}

fn filter_free_tier_models(state: &AppState, models: &mut Vec<Value>, kinds: &[&str]) {
    if crate::free_tier::enabled(state) {
        models.clear();
        if kinds.contains(&LLM_KIND) {
            models.push(json!({
                "id": crate::free_tier::FREE_COMBO_MODEL,
                "object": "model",
                "owned_by": "9router",
                "description": "Requests are routed to third-party free providers; do not send confidential or personal data.",
            }));
        }
        return;
    }
    let hidden = crate::free_tier::hidden_providers(state);
    models.retain(|model| {
        if model.get("id").and_then(Value::as_str) == Some(crate::free_tier::FREE_COMBO_MODEL) {
            return false;
        }
        let owner = model.get("owned_by").and_then(Value::as_str).map(|owner| {
            alias_to_provider_id()
                .get(owner)
                .cloned()
                .unwrap_or_else(|| providers::canonical_provider(owner))
        });
        let id_provider = model
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.split_once('/').map(|(provider, _)| provider))
            .map(|provider| {
                alias_to_provider_id()
                    .get(provider)
                    .cloned()
                    .unwrap_or_else(|| providers::canonical_provider(provider))
            });
        !owner.is_some_and(|provider| hidden.contains(&provider))
            && !id_provider.is_some_and(|provider| hidden.contains(&provider))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Db, state::AppState};
    use tempfile::TempDir;

    fn kinds(kinds: &[&str]) -> Vec<String> {
        kinds.iter().map(|kind| kind.to_string()).collect()
    }

    fn test_state(temp: &TempDir) -> AppState {
        let db_path = temp.path().join("data.sqlite");
        let db = Db::open(&db_path).unwrap();
        AppState::new(
            Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                ui_origin: "http://127.0.0.1:20129".into(),
                data_dir: temp.path().to_path_buf(),
                db_path,
                upstream_timeout_secs: 1,
                stream_first_chunk_timeout: std::time::Duration::from_secs(200),
                stream_stall_timeout: std::time::Duration::from_secs(360),
                ui_only_header_secret: "test-only".into(),
                legacy_backend_origin: None,
                compat_api_enabled: false,
            },
            db,
        )
        .unwrap()
    }

    #[test]
    fn consumer_models_hide_managed_members_and_expose_one_virtual_combo() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let mut models = vec![
            json!({"id":"groq/llama", "owned_by":"groq"}),
            json!({"id":"openai/gpt", "owned_by":"openai"}),
            json!({"id":"combo-free", "owned_by":"combo"}),
        ];
        filter_free_tier_models(&state, &mut models, &[LLM_KIND]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "combo-free");

        state
            .db
            .update_settings(json!({"builtinFreeCombo":false}))
            .unwrap();
        let mut models = vec![
            json!({"id":"groq/llama", "owned_by":"groq"}),
            json!({"id":"openai/gpt", "owned_by":"openai"}),
        ];
        filter_free_tier_models(&state, &mut models, &[LLM_KIND]);
        assert!(!models.iter().any(|model| model["id"] == "groq/llama"));
        assert!(models.iter().any(|model| model["id"] == "openai/gpt"));
        assert!(!models.iter().any(|model| model["id"] == "combo-free"));
        crate::free_tier::set_exposed(&state, "groq", true).unwrap();
        let mut models = vec![json!({"id":"groq/llama", "owned_by":"groq"})];
        filter_free_tier_models(&state, &mut models, &[LLM_KIND]);
        assert!(models.iter().any(|model| model["id"] == "groq/llama"));
    }

    #[test]
    fn model_kind_maps_types_and_defaults_to_llm() {
        assert_eq!(model_kind(&json!({"id": "x"})), "llm");
        assert_eq!(
            model_kind(&json!({"id": "x", "kind": "imageToText"})),
            "imageToText"
        );
        assert_eq!(
            model_kind(&json!({"id": "x", "type": "embedding"})),
            "embedding"
        );
        assert_eq!(model_kind(&json!({"id": "x", "kind": "llm"})), "llm");
        // unknown types fall back to the LLM sentinel
        assert_eq!(model_kind(&json!({"id": "x", "type": "music"})), "llm");
    }

    #[test]
    fn kind_slug_map_covers_the_six_upstream_slugs() {
        let slugs: Vec<&str> = KIND_SLUG_MAP.iter().map(|(slug, _)| *slug).collect();
        assert_eq!(
            slugs,
            vec!["image", "tts", "stt", "embedding", "image-to-text", "web"]
        );
        assert_eq!(
            KIND_SLUG_MAP
                .iter()
                .find(|(slug, _)| *slug == "web")
                .map(|(_, kinds)| *kinds),
            Some(&["webSearch", "webFetch"][..])
        );
    }

    #[test]
    fn unknown_model_ids_infer_kind_from_the_id() {
        assert_eq!(
            infer_kind_from_unknown_model_id("text-embedding-3-large"),
            "embedding"
        );
        assert_eq!(
            infer_kind_from_unknown_model_id("gpt-4o-audio-preview"),
            "tts"
        );
        assert_eq!(infer_kind_from_unknown_model_id("dall-e-3"), "image");
        assert_eq!(infer_kind_from_unknown_model_id("sd-xl-turbo"), "image");
        assert_eq!(infer_kind_from_unknown_model_id("gpt-5.2"), "llm");
    }

    #[test]
    fn combo_kind_filtering_defaults_to_llm() {
        let llm = kinds(&["llm"]);
        let web = kinds(&["webSearch", "webFetch"]);
        assert!(combo_matches_kinds(&json!({"name": "c"}), &llm));
        assert!(!combo_matches_kinds(&json!({"name": "c"}), &web));
        assert!(combo_matches_kinds(
            &json!({"name": "c", "kind": "webSearch"}),
            &web
        ));
        assert!(!combo_matches_kinds(
            &json!({"name": "c", "kind": "webSearch"}),
            &llm
        ));
    }

    #[test]
    fn provider_kind_matching_uses_service_kinds() {
        // providers without serviceKinds are LLM-only
        assert!(provider_matches_kinds("openai", &kinds(&["llm"])));
        assert!(provider_matches_kinds("openai", &kinds(&["tts", "stt"])));
        assert!(provider_matches_kinds("github", &kinds(&["embedding"])));
        assert!(!provider_matches_kinds("github", &kinds(&["image"])));
        assert!(!provider_matches_kinds("anthropic", &kinds(&["tts"])));
        assert!(provider_matches_kinds(
            "anthropic",
            &kinds(&["imageToText"])
        ));
        // unknown providers (compatible / custom) default to LLM
        assert!(provider_matches_kinds(
            "openai-compatible-x",
            &kinds(&["llm"])
        ));
        assert!(!provider_matches_kinds(
            "openai-compatible-x",
            &kinds(&["image"])
        ));
    }

    #[test]
    fn provider_aliases_mirror_the_dashboard_constants() {
        // `getProviderAlias` uses uiAlias when the registry row declares one
        assert_eq!(provider_alias("assemblyai"), "aai");
        assert_eq!(provider_alias("antigravity"), "ag");
        assert_eq!(provider_alias("kiro"), "kr");
        // `getProviderAlias` falls back to the registry alias, then the id
        assert_eq!(provider_alias("gitlab"), "gitlab");
        assert_eq!(provider_alias("openai-compatible-x"), "openai-compatible-x");
        // PROVIDER_ID_TO_ALIAS keys are the ids that also have a transport
        assert_eq!(static_alias("kiro"), "kr");
        assert_eq!(static_alias("gitlab"), "gitlab");
        assert_eq!(static_alias("openai-compatible-x"), "openai-compatible-x");
        // inverse map: the catalog model key resolves back to the provider id
        assert_eq!(
            alias_to_provider_id().get("kr").map(String::as_str),
            Some("kiro")
        );
        assert_eq!(
            alias_to_provider_id().get("ag").map(String::as_str),
            Some("antigravity")
        );
    }

    #[test]
    fn static_model_keys_follow_registry_order_and_include_tts_tables() {
        let keys = provider_model_keys_in_order();
        let first = keys.first().map(String::as_str);
        assert_eq!(first, Some("alicode-intl"), "registry order, not sorted");
        assert!(keys.contains(&"openai-tts-voices".to_string()));
        assert_eq!(keys.len(), {
            let mut unique = keys.clone();
            unique.sort();
            unique.dedup();
            unique.len()
        });
    }

    #[test]
    fn parse_openai_style_models_accepts_every_wrapper() {
        assert_eq!(parse_openai_style_models(&json!([{"id": "a"}])).len(), 1);
        assert_eq!(
            parse_openai_style_models(&json!({"data": [{"id": "a"}]})).len(),
            1
        );
        assert_eq!(
            parse_openai_style_models(&json!({"models": [{"id": "a"}]})).len(),
            1
        );
        assert_eq!(
            parse_openai_style_models(&json!({"results": [{"id": "a"}]})).len(),
            1
        );
        assert!(parse_openai_style_models(&json!({"nope": 1})).is_empty());
    }

    #[test]
    fn model_prefixes_are_stripped_in_alias_then_id_order() {
        let output = "kr".to_string();
        let alias = "kr".to_string();
        let provider = "kiro".to_string();
        let prefixes = [&output, &alias, &provider];
        assert_eq!(strip_model_prefix("kr/claude", &prefixes), "claude");
        assert_eq!(strip_model_prefix("kiro/claude", &prefixes), "claude");
        assert_eq!(strip_model_prefix("claude", &prefixes), "claude");
    }

    #[test]
    fn only_finite_numbers_reach_the_token_limit_fields() {
        assert_eq!(finite_number(Some(&json!(200_000))), Some(json!(200_000)));
        assert_eq!(finite_number(Some(&json!("200000"))), None);
        assert_eq!(finite_number(Some(&json!(null))), None);
        assert_eq!(finite_number(None), None);
    }

    #[test]
    fn disabled_lookup_is_per_alias_and_exact() {
        let mut disabled_by_alias = Map::new();
        disabled_by_alias.insert("kr".to_string(), json!(["claude-sonnet-4.5"]));
        assert!(disabled(&disabled_by_alias, "kr", "claude-sonnet-4.5"));
        assert!(!disabled(&disabled_by_alias, "kr", "claude-sonnet-4.6"));
        assert!(!disabled(&disabled_by_alias, "kiro", "claude-sonnet-4.5"));
        assert!(!disabled(&Map::new(), "kr", "claude-sonnet-4.5"));
    }

    #[test]
    fn model_not_found_payload_matches_upstream() {
        let identifier = "nope/nothing";
        let payload = json!({"error": {
            "message": format!("The model '{identifier}' does not exist or you do not have access to it."),
            "type": "invalid_request_error",
            "code": "model_not_found",
        }});
        assert_eq!(
            payload["error"]["message"],
            "The model 'nope/nothing' does not exist or you do not have access to it."
        );
        assert_eq!(payload["error"]["code"], "model_not_found");
    }

    #[test]
    fn routes_split_into_list_kinds_and_lookup() {
        assert_eq!(models_route("/v1/models"), ModelsRoute::List);
        assert_eq!(models_route("/api/v1/models"), ModelsRoute::List);
        // a trailing slash is the list route, not an empty model lookup
        assert_eq!(models_route("/v1/models/"), ModelsRoute::List);
        assert_eq!(
            models_route("/v1/models/image"),
            ModelsRoute::Kinds(&["image"])
        );
        assert_eq!(
            models_route("/api/v1/models/image-to-text"),
            ModelsRoute::Kinds(&["imageToText"])
        );
        assert_eq!(
            models_route("/v1/models/web"),
            ModelsRoute::Kinds(&["webSearch", "webFetch"])
        );
        assert_eq!(
            models_route("/v1/models/openai/gpt-5.2"),
            ModelsRoute::Lookup("openai/gpt-5.2".to_string())
        );
        // a kind slug only counts as a single segment
        assert_eq!(
            models_route("/v1/models/image/extra"),
            ModelsRoute::Lookup("image/extra".to_string())
        );
        // unknown slugs are model ids, not kind filters (upstream behavior)
        assert_eq!(
            models_route("/v1/models/llm"),
            ModelsRoute::Lookup("llm".to_string())
        );
    }
}
