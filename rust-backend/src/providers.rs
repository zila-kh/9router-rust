use crate::{error::AppError, state::AppState};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};

fn resolve_env_placeholders_with<F>(value: &mut Value, lookup: &F)
where
    F: Fn(&str) -> Option<String>,
{
    match value {
        Value::String(text) => {
            if let Some(name) = text.strip_prefix("env:") {
                let replacement = lookup(name).unwrap_or_default();
                *text = replacement;
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_env_placeholders_with(item, lookup);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                resolve_env_placeholders_with(item, lookup);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn load_catalog() -> Value {
    let mut catalog: Value = serde_json::from_str(include_str!("../assets/provider-catalog.json"))
        .unwrap_or_else(
            |_| json!({"registry":[],"providers":{},"models":{},"oauth":{},"media":{}}),
        );
    resolve_env_placeholders_with(&mut catalog, &|name| std::env::var(name).ok());
    catalog
}

static CATALOG: Lazy<Value> = Lazy::new(load_catalog);

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub requested: String,
    pub provider: String,
    pub model: String,
    pub connection: Value,
}

pub fn catalog() -> &'static Value {
    &CATALOG
}
pub fn provider_entry(id: &str) -> Option<&'static Value> {
    CATALOG.get("registry")?.as_array()?.iter().find(|e| {
        e.get("id").and_then(Value::as_str) == Some(id)
            || e.get("alias").and_then(Value::as_str) == Some(id)
            || e.get("aliases")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|v| v.as_str() == Some(id)))
                .unwrap_or(false)
    })
}
pub fn transport(id: &str) -> Value {
    if id.starts_with("anthropic-compatible-") {
        return json!({"format":"claude"});
    }
    if id.starts_with("openai-compatible-") {
        return json!({"format":"openai"});
    }
    let canonical = provider_entry(id)
        .and_then(|e| e.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(id);
    CATALOG
        .get("providers")
        .and_then(|p| p.get(canonical))
        .cloned()
        .unwrap_or_else(|| {
            provider_entry(id)
                .and_then(|e| e.get("transport"))
                .cloned()
                .unwrap_or_else(|| json!({}))
        })
}
pub fn media_config(id: &str, kind: &str) -> Value {
    let canonical = canonical_provider(id);
    let key = match kind {
        "embedding" => "embeddingConfig",
        "tts" => "ttsConfig",
        "stt" => "sttConfig",
        "image" => "imageConfig",
        "search" => "searchConfig",
        "fetch" => "fetchConfig",
        _ => "",
    };
    if key.is_empty() {
        return json!({});
    }
    CATALOG
        .get("media")
        .and_then(|m| m.get(&canonical))
        .and_then(|v| v.get(key))
        .cloned()
        .or_else(|| provider_entry(&canonical).and_then(|e| e.get(key)).cloned())
        .unwrap_or_else(|| json!({}))
}
pub fn models_for(id: &str) -> Vec<Value> {
    let alias = provider_entry(id)
        .and_then(|e| e.get("alias"))
        .and_then(Value::as_str)
        .unwrap_or(id);
    CATALOG
        .get("models")
        .and_then(|m| m.get(alias))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
pub fn all_models_openai() -> Value {
    let mut data = Vec::new();
    if let Some(entries) = CATALOG.get("registry").and_then(Value::as_array) {
        for e in entries {
            let id = e.get("id").and_then(Value::as_str).unwrap_or_default();
            let alias = e.get("alias").and_then(Value::as_str).unwrap_or(id);
            for m in models_for(id) {
                if let Some(mid) = m.get("id").and_then(Value::as_str) {
                    data.push(json!({"id":format!("{alias}/{mid}"),"object":"model","owned_by":id,"root":mid,"capabilities":m.get("capabilities").cloned().unwrap_or(Value::Null),"kind":m.get("kind").cloned().unwrap_or(json!("llm"))}))
                }
            }
        }
    }
    json!({"object":"list","data":data})
}

pub fn split_model(requested: &str) -> (Option<&str>, &str) {
    if let Some((p, m)) = requested.split_once('/') {
        (Some(p), m)
    } else {
        (None, requested)
    }
}
pub fn canonical_provider(id: &str) -> String {
    provider_entry(id)
        .and_then(|e| e.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string()
}

pub fn resolve_model(state: &AppState, requested: &str) -> Result<ResolvedModel, AppError> {
    let aliases = state.db.kv_all("modelAliases")?;
    let mut queue = VecDeque::from([requested.to_string()]);
    let mut visited = HashSet::new();
    let mut traversed_alias = false;

    while let Some(candidate) = queue.pop_front() {
        if !visited.insert(candidate.clone()) {
            continue;
        }
        if visited.len() > 64 {
            return Err(AppError::BadRequest(
                "model alias graph exceeds the maximum depth".into(),
            ));
        }

        match resolve_unaliased(state, &candidate) {
            Ok(mut resolved) => {
                resolved.requested = requested.to_string();
                return Ok(resolved);
            }
            Err(AppError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        if let Some(target) = aliases
            .get(&candidate)
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            traversed_alias = true;
            queue.push_back(target);
        }
        for (key, value) in &aliases {
            if value.as_str() == Some(candidate.as_str()) {
                traversed_alias = true;
                queue.push_back(key.clone());
            }
        }
    }

    if traversed_alias {
        Err(AppError::BadRequest(format!(
            "model alias graph for {requested} is cyclic or does not resolve to an active model"
        )))
    } else {
        Err(AppError::NotFound(format!(
            "no active provider for model {requested}"
        )))
    }
}

fn resolve_unaliased(state: &AppState, requested: &str) -> Result<ResolvedModel, AppError> {
    let (explicit, model) = split_model(requested);
    if let Some(provider) = explicit {
        return resolve_for_provider(state, provider, requested, model);
    }
    let inferred = if requested.starts_with("claude-") {
        Some("anthropic")
    } else if requested.starts_with("gemini-") {
        Some("gemini")
    } else if requested.starts_with("gpt-")
        || requested.starts_with("o1")
        || requested.starts_with("o3")
        || requested.starts_with("o4")
    {
        Some("openai")
    } else if requested.starts_with("deepseek-") {
        Some("openrouter")
    } else {
        None
    };
    if let Some(provider) = inferred {
        if let Ok(resolved) = resolve_for_provider(state, provider, requested, model) {
            return Ok(resolved);
        }
    }
    let active = state.db.provider_connections(None, Some(true))?;
    for connection in &active {
        if connection.get("defaultModel").and_then(Value::as_str) == Some(requested) {
            let provider = connection
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(ResolvedModel {
                requested: requested.into(),
                provider: provider.into(),
                model: model.into(),
                connection: connection.clone(),
            });
        }
    }
    let mut candidates = Vec::new();
    if let Some(entries) = CATALOG.get("registry").and_then(Value::as_array) {
        for entry in entries {
            let provider = entry.get("id").and_then(Value::as_str).unwrap_or_default();
            if models_for(provider)
                .iter()
                .any(|candidate| candidate.get("id").and_then(Value::as_str) == Some(model))
            {
                candidates.push(provider);
            }
        }
    }
    for provider in candidates {
        if let Ok(resolved) = resolve_for_provider(state, provider, requested, model) {
            return Ok(resolved);
        }
    }
    Err(AppError::NotFound(format!(
        "no active provider for model {requested}"
    )))
}

fn resolve_for_provider(
    state: &AppState,
    p: &str,
    requested: &str,
    model: &str,
) -> Result<ResolvedModel, AppError> {
    let provider = canonical_provider(p);
    let conns = state.db.provider_connections(Some(&provider), Some(true))?;
    let connection = conns.into_iter().next().ok_or_else(|| {
        AppError::NotFound(format!("no active connection for provider {provider}"))
    })?;
    let upstream = model_upstream_id(&provider, model).unwrap_or_else(|| model.to_string());
    Ok(ResolvedModel {
        requested: requested.into(),
        provider,
        model: upstream,
        connection,
    })
}

pub fn model_upstream_id(provider: &str, model: &str) -> Option<String> {
    models_for(provider)
        .into_iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
        .and_then(|m| {
            m.get("upstreamModelId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub fn auth_header(connection: &Value, t: &Value) -> Option<(String, String)> {
    let auth = t.get("auth");
    let api = connection.get("apiKey").and_then(Value::as_str);
    let access = connection.get("accessToken").and_then(Value::as_str);
    let format = t.get("format").and_then(Value::as_str).unwrap_or("openai");
    let spec = if let Some(a) = auth {
        if a.get("combined").and_then(Value::as_bool).unwrap_or(false) || a.get("header").is_some()
        {
            Some(a)
        } else if api.is_some() {
            a.get("apiKey")
        } else {
            a.get("oauth")
        }
    } else {
        None
    };
    let (header, scheme, token) = if let Some(spec) = spec {
        (
            spec.get("header")
                .and_then(Value::as_str)
                .unwrap_or("Authorization"),
            spec.get("scheme")
                .and_then(Value::as_str)
                .unwrap_or("bearer"),
            api.or(access)?,
        )
    } else if format == "claude" {
        ("x-api-key", "raw", api.or(access)?)
    } else {
        ("Authorization", "bearer", api.or(access)?)
    };
    let value = if scheme == "bearer" {
        format!("Bearer {token}")
    } else {
        token.to_string()
    };
    Some((header.into(), value))
}

pub fn endpoint(
    provider: &str,
    connection: &Value,
    kind: &str,
    model: &str,
) -> Result<(String, String), AppError> {
    let t = transport(provider);
    let format = t
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai")
        .to_string();
    let custom = connection
        .pointer("/providerSpecificData/baseUrl")
        .and_then(Value::as_str);
    let mut url = custom
        .or_else(|| t.get("baseUrl").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    if url.is_empty() {
        return Err(AppError::BadRequest(format!(
            "provider {provider} has no transport baseUrl"
        )));
    }
    if custom.is_some() {
        url = url.trim_end_matches('/').to_string();
        if kind == "chat"
            && provider.starts_with("anthropic-compatible-")
            && !url.ends_with("/messages")
        {
            url.push_str("/messages");
        } else if kind == "chat"
            && !provider.starts_with("anthropic-compatible-")
            && !url.ends_with("/chat/completions")
            && !url.ends_with("/responses")
        {
            url.push_str("/chat/completions");
        }
    }
    if format == "gemini" {
        url = url.replace("{model}", model);
    }
    if kind != "chat" {
        if let Some(media) = CATALOG.get("media").and_then(|m| m.get(provider)) {
            let key = match kind {
                "embedding" => "embeddingConfig",
                "tts" => "ttsConfig",
                "stt" => "sttConfig",
                "image" => "imageConfig",
                "search" => "searchConfig",
                "fetch" => "fetchConfig",
                _ => "",
            };
            if let Some(base) = media
                .get(key)
                .and_then(|c| c.get("baseUrl"))
                .and_then(Value::as_str)
            {
                url = base.into()
            }
        }
    }
    Ok((url, format))
}

#[cfg(test)]
mod tests {
    use super::{resolve_env_placeholders_with, resolve_model};
    use crate::{config::Config, db::Db, error::AppError, state::AppState};
    use serde_json::json;

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

    #[test]
    fn resolves_nested_environment_placeholders() {
        let mut value = json!({
            "clientId": "env:CLIENT_ID",
            "nested": ["keep", {"secret": "env:CLIENT_SECRET"}],
            "number": 7
        });
        resolve_env_placeholders_with(&mut value, &|name| match name {
            "CLIENT_ID" => Some("client-value".into()),
            "CLIENT_SECRET" => Some("secret-value".into()),
            _ => None,
        });
        assert_eq!(value["clientId"], "client-value");
        assert_eq!(value["nested"][0], "keep");
        assert_eq!(value["nested"][1]["secret"], "secret-value");
        assert_eq!(value["number"], 7);
    }

    #[test]
    fn missing_environment_placeholder_becomes_empty() {
        let mut value = json!({"token": "env:MISSING_TOKEN"});
        resolve_env_placeholders_with(&mut value, &|_| None);
        assert_eq!(value["token"], "");
    }

    #[test]
    fn model_alias_cycles_return_an_error_without_recursing() {
        let (_temp, state) = test_state();
        state
            .db
            .kv_set("modelAliases", "alias-a", &json!("alias-b"))
            .expect("store first alias");
        state
            .db
            .kv_set("modelAliases", "alias-b", &json!("alias-a"))
            .expect("store second alias");

        match resolve_model(&state, "alias-a") {
            Err(AppError::BadRequest(message)) => assert!(message.contains("cyclic")),
            other => panic!("expected cyclic alias error, got {other:?}"),
        }
    }

    #[test]
    fn model_aliases_support_current_and_legacy_orientation() {
        for (key, value, requested) in [
            ("friendly", "openai/model-x", "friendly"),
            ("openai/model-x", "legacy-friendly", "legacy-friendly"),
        ] {
            let (_temp, state) = test_state();
            state
                .db
                .create_connection(json!({"provider":"openai","apiKey":"test-key"}))
                .expect("create provider connection");
            state
                .db
                .kv_set("modelAliases", key, &json!(value))
                .expect("store model alias");

            let resolved = resolve_model(&state, requested).expect("resolve model alias");
            assert_eq!(resolved.requested, requested);
            assert_eq!(resolved.provider, "openai");
            assert_eq!(resolved.model, "model-x");
        }
    }
}
