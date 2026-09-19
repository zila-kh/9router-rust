//! Native model metadata endpoints.
//!
//! - `GET /v1/models/info?id={alias}/{modelId}[&kind=k]` — single-model metadata
//!   (upstream `api/v1/models/info/route.js`).
//! - `GET /v1beta/models` — Gemini-format model list (upstream
//!   `api/v1beta/models/route.js`).
//!
//! Both are pure catalog projections: `PROVIDER_MODELS` and the provider
//! registry exported into `assets/provider-catalog.json` are the same data the
//! upstream routes read, so no database or network access is involved.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Body,
    http::{Method, Response, StatusCode},
};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};

use crate::{error::AppError, inference_media::json_response, providers, state::AppState};

/// `KIND_ENDPOINT` from upstream `api/v1/models/info/route.js`.
const KIND_ENDPOINTS: [(&str, &str); 8] = [
    ("llm", "/v1/chat/completions"),
    ("image", "/v1/images/generations"),
    ("tts", "/v1/audio/speech"),
    ("stt", "/v1/audio/transcriptions"),
    ("embedding", "/v1/embeddings"),
    ("imageToText", "/v1/chat/completions"),
    ("webSearch", "/v1/search"),
    ("webFetch", "/v1/fetch"),
];

/// Fields copied onto the info payload when the catalog model declares them.
const MODEL_INFO_FIELDS: [&str; 5] = [
    "params",
    "capabilities",
    "options",
    "dimensions",
    "contextWindow",
];

/// `ALIAS_TO_ID` from the dashboard provider constants (`alias = uiAlias || alias`).
static ALIAS_TO_ID: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut map = HashMap::new();
    if let Some(entries) = providers::catalog()
        .get("registry")
        .and_then(Value::as_array)
    {
        for entry in entries {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            let alias = entry
                .get("uiAlias")
                .and_then(Value::as_str)
                .filter(|alias| !alias.is_empty())
                .or_else(|| entry.get("alias").and_then(Value::as_str))
                .filter(|alias| !alias.is_empty());
            if let Some(alias) = alias {
                map.insert(alias, id);
            }
        }
    }
    map
});

/// `getModelKind(model, fallback)` from `shared/constants/models.js`.
fn model_kind(model: &Value, fallback: &str) -> String {
    model
        .get("kind")
        .or_else(|| model.get("type"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn provider_models(key: &str) -> Option<&'static Vec<Value>> {
    providers::catalog()
        .get("models")
        .and_then(|models| models.get(key))
        .and_then(Value::as_array)
}

/// `GET /v1/models/info?id={alias}/{modelId}[&kind=k]`
pub async fn handle_model_info(
    _state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let params = query_params(query);
    let id = params
        .get("id")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let Some(id) = id else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": {
                "message": "Missing required query param: id (e.g. ?id=openai/dall-e-3)",
                "type": "invalid_request_error",
            }}),
        );
    };
    let kind = params
        .get("kind")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    match lookup(id, kind) {
        Some(info) => json_response(StatusCode::OK, info),
        None => json_response(
            StatusCode::NOT_FOUND,
            json!({"error": {"message": format!("Model not found: {id}"), "type": "not_found"}}),
        ),
    }
}

/// `GET /v1beta/models` — Gemini-compatible list built from every catalog model.
pub async fn handle_v1beta_models(
    state: &AppState,
    method: &Method,
    consumer: bool,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let models = if !consumer {
        v1beta_models()
    } else if crate::free_tier::enabled(state) {
        vec![json!({
            "name": "models/combo-free",
            "displayName": "9Router Free Tier",
            "description": "Requests are routed to third-party free providers; do not send confidential or personal data.",
            "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            "inputTokenLimit": 128000,
            "outputTokenLimit": 8192,
        })]
    } else {
        let hidden = crate::free_tier::hidden_providers(state);
        v1beta_models_with_hidden(Some(&hidden))
    };
    json_response(StatusCode::OK, json!({"models": models}))
}

fn v1beta_models() -> Vec<Value> {
    v1beta_models_with_hidden(None)
}

fn v1beta_models_with_hidden(hidden: Option<&HashSet<String>>) -> Vec<Value> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |name: String, display_name: String, description: String, methods: Vec<&str>| {
        if !seen.insert(name.clone()) {
            return;
        }
        models.push(json!({
            "name": name,
            "displayName": display_name,
            "description": description,
            "supportedGenerationMethods": methods,
            "inputTokenLimit": 128000,
            "outputTokenLimit": 8192,
        }));
    };
    if let Some(entries) = providers::catalog()
        .get("models")
        .and_then(Value::as_object)
    {
        for (provider, provider_models) in entries {
            if hidden
                .is_some_and(|hidden| hidden.contains(&providers::canonical_provider(provider)))
            {
                continue;
            }
            let Some(items) = provider_models.as_array() else {
                continue;
            };
            for model in items {
                let Some(id) = model.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let display_name = model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string();
                add(
                    format!("models/{provider}/{id}"),
                    display_name.clone(),
                    format!("{provider} model: {display_name}"),
                    vec!["generateContent"],
                );
                if provider == "gemini" {
                    add(
                        format!("models/{id}"),
                        display_name.clone(),
                        format!("Gemini model: {display_name}"),
                        vec!["generateContent", "streamGenerateContent"],
                    );
                }
            }
        }
    }
    models
}

/// Upstream `lookup()`: `{alias}/{modelId}` where the alias may be a provider id.
fn lookup(full_id: &str, requested_kind: Option<&str>) -> Option<Value> {
    let (alias, model_id) = full_id.split_once('/')?;
    if alias.is_empty() || model_id.is_empty() {
        return None;
    }
    let provider_id = ALIAS_TO_ID.get(alias).copied().unwrap_or(alias);
    let provider_info = providers::provider_entry(provider_id);
    let list = provider_models(alias).or_else(|| provider_models(provider_id));
    let matched = list.and_then(|models| {
        models.iter().find(|model| {
            model.get("id").and_then(Value::as_str) == Some(model_id)
                && requested_kind.is_none_or(|kind| model_kind(model, "llm") == kind)
        })
    });
    if let Some(model) = matched {
        let kind = model_kind(model, "llm");
        return Some(build_info(alias, provider_id, model, &kind, provider_info));
    }
    let provider_info = provider_info?;
    if model_id == "search" && provider_info.get("searchConfig").is_some() {
        let name = provider_name(provider_info);
        return Some(build_info(
            alias,
            provider_id,
            &json!({
                "id": "search",
                "name": format!("{name} Search"),
                "params": ["query", "max_results", "country", "language", "time_range", "domain_filter", "search_type"],
            }),
            "webSearch",
            Some(provider_info),
        ));
    }
    if model_id == "fetch" && provider_info.get("fetchConfig").is_some() {
        let name = provider_name(provider_info);
        return Some(build_info(
            alias,
            provider_id,
            &json!({
                "id": "fetch",
                "name": format!("{name} Fetch"),
                "params": ["url", "format", "max_characters"],
            }),
            "webFetch",
            Some(provider_info),
        ));
    }
    None
}

/// `AI_PROVIDERS[id].name`, which the dashboard constants take from `display.name`.
fn provider_name(provider_info: &Value) -> String {
    provider_info
        .pointer("/display/name")
        .and_then(Value::as_str)
        .or_else(|| provider_info.get("name").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn build_info(
    alias: &str,
    provider_id: &str,
    model: &Value,
    kind: &str,
    provider_info: Option<&Value>,
) -> Value {
    let model_id = model.get("id").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    out.insert("id".into(), json!(format!("{alias}/{model_id}")));
    out.insert(
        "name".into(),
        json!(model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(model_id)),
    );
    out.insert("kind".into(), json!(kind));
    out.insert("owned_by".into(), json!(alias));
    out.insert(
        "endpoint".into(),
        KIND_ENDPOINTS
            .iter()
            .find(|(name, _)| *name == kind)
            .map(|(_, endpoint)| json!(endpoint))
            .unwrap_or(Value::Null),
    );
    for field in MODEL_INFO_FIELDS {
        if let Some(value) = model.get(field) {
            out.insert(field.into(), value.clone());
        }
    }
    if kind == "tts" && crate::voice_catalog::PUBLIC_VOICE_PROVIDERS.contains(&provider_id) {
        out.insert(
            "voicesUrl".into(),
            json!(format!("/v1/audio/voices?provider={provider_id}")),
        );
    }
    if kind == "webSearch" {
        if let Some(config) = provider_info.and_then(|info| info.get("searchConfig")) {
            if let Some(value) = config.get("searchTypes") {
                out.insert("searchTypes".into(), value.clone());
            }
            if let Some(value) = config.get("maxMaxResults") {
                out.insert("maxResults".into(), value.clone());
            }
            if let Some(value) = config.get("requiredOptions") {
                out.insert("required".into(), value.clone());
            }
        }
    }
    Value::Object(out)
}

fn query_params(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn catalog_model(provider: &str, index: usize) -> Value {
        provider_models(provider)
            .unwrap_or_else(|| panic!("missing catalog models for {provider}"))
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("missing model #{index} for {provider}"))
    }

    #[test]
    fn chat_model_info_uses_alias_id_and_endpoint() {
        let model = catalog_model("gemini", 0);
        let id = format!("gemini/{}", model["id"].as_str().unwrap());
        let info = lookup(&id, None).expect("info");
        assert_eq!(info["id"], id);
        assert_eq!(info["owned_by"], "gemini");
        assert_eq!(info["endpoint"], "/v1/chat/completions");
        assert_eq!(info["kind"], "llm");
        assert_eq!(info["name"], model["name"].clone());
    }

    #[test]
    fn alias_resolves_to_provider_id_for_voices_and_kind() {
        let model = catalog_model("deepgram", 0);
        let model_id = model["id"].as_str().unwrap();
        let info = lookup(&format!("dg/{model_id}"), None).expect("info");
        assert_eq!(info["owned_by"], "dg");
        assert_eq!(info["id"], format!("dg/{model_id}"));
        assert_eq!(info["kind"], model_kind(&model, "llm"));
    }

    #[test]
    fn tts_models_expose_the_native_voices_url() {
        let model = catalog_model("edge-tts", 0);
        let model_id = model["id"].as_str().unwrap();
        let info = lookup(&format!("edge-tts/{model_id}"), None).expect("info");
        assert_eq!(info["kind"], "tts");
        assert_eq!(info["endpoint"], "/v1/audio/speech");
        assert_eq!(info["voicesUrl"], "/v1/audio/voices?provider=edge-tts");
    }

    #[test]
    fn kind_filter_rejects_models_of_other_kinds() {
        let model = catalog_model("edge-tts", 0);
        let model_id = model["id"].as_str().unwrap();
        assert!(lookup(&format!("edge-tts/{model_id}"), Some("llm")).is_none());
    }

    #[test]
    fn virtual_search_model_follows_the_search_config() {
        let info = lookup("tavily/search", None).expect("info");
        assert_eq!(info["id"], "tavily/search");
        assert_eq!(info["name"], "Tavily Search");
        assert_eq!(info["kind"], "webSearch");
        assert_eq!(info["endpoint"], "/v1/search");
        assert_eq!(info["params"][0], "query");
        assert_eq!(info["searchTypes"][0], "web");
        assert_eq!(info["maxResults"], 20);
    }

    #[test]
    fn virtual_fetch_model_requires_fetch_config() {
        let info = lookup("tavily/fetch", None).expect("info");
        assert_eq!(info["kind"], "webFetch");
        assert_eq!(info["endpoint"], "/v1/fetch");
        let unsupported = lookup("brave-search/fetch", None);
        assert!(unsupported.is_none(), "brave-search has no fetchConfig");
    }

    #[test]
    fn unknown_models_and_malformed_ids_are_missing() {
        assert!(lookup("nope/model", None).is_none());
        assert!(lookup("gemini/definitely-not-a-model", None).is_none());
        assert!(lookup("no-slash", None).is_none());
        assert!(lookup("/leading-slash", None).is_none());
    }

    #[test]
    fn v1beta_list_covers_namespaced_and_bare_gemini_models() {
        let models = v1beta_models();
        let names: Vec<&str> = models
            .iter()
            .filter_map(|model| model["name"].as_str())
            .collect();
        let gemini = catalog_model("gemini", 0);
        let gemini_id = gemini["id"].as_str().unwrap();
        assert!(names.contains(&format!("models/gemini/{gemini_id}").as_str()));
        assert!(names.contains(&format!("models/{gemini_id}").as_str()));
        let bare = models
            .iter()
            .find(|model| model["name"] == format!("models/{gemini_id}"))
            .expect("bare gemini model");
        assert_eq!(
            bare["description"],
            format!("Gemini model: {}", gemini["name"].as_str().unwrap())
        );
        assert_eq!(bare["inputTokenLimit"], 128000);
        assert_eq!(bare["outputTokenLimit"], 8192);
        assert_eq!(
            bare["supportedGenerationMethods"],
            json!(["generateContent", "streamGenerateContent"])
        );
        let mut unique = std::collections::HashSet::new();
        for name in &names {
            assert!(unique.insert(*name), "duplicate model name {name}");
        }
    }
}
