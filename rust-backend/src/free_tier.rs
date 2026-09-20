//! Built-in free tier (`combo-free`).
//!
//! The tier is a computed combo assembled at request time from two member
//! kinds: anonymous providers synced from the online free-registry (served as
//! virtual connections that are never written to the database) and active
//! connections of registry-keyed providers (or connections explicitly tagged
//! into the pool). The admin manages the tier through `/api/free-tier`;
//! consumers see only the single `combo-free` model.
//!
//! Member definitions from the registry are read-only locally: exclusions and
//! the master switch are the only local controls. See
//! `docs/FREE_COMBO_PLAN.md`.

use crate::{error::AppError, state::AppState};
use axum::{
    body::Body,
    http::{Method, StatusCode},
    response::Response,
};
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

pub const FREE_COMBO_MODEL: &str = "combo-free";
/// Reserved provider prefix for registry-served virtual providers. The
/// `openai-compatible-` prefix makes the transport layer serve a generic
/// OpenAI format, with the base URL taken from the synthetic connection.
pub const VIRTUAL_PROVIDER_PREFIX: &str = "openai-compatible-free-";

const MEMBER_COOLDOWN: Duration = Duration::from_secs(600);
const KV_SCOPE: &str = "freeTier";
const MAX_MODELS_PER_KEYED_PROVIDER: usize = 10;
const REGISTRY_URL: &str = "https://raw.githubusercontent.com/zila-kh/9router-rust/main/rust-backend/assets/free-registry.json";

struct RegistryState {
    doc: Value,
    source: &'static str,
    synced_at: Option<String>,
}

static REGISTRY: OnceLock<RwLock<RegistryState>> = OnceLock::new();
static COOLDOWNS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn registry_cell() -> &'static RwLock<RegistryState> {
    REGISTRY.get_or_init(|| {
        RwLock::new(RegistryState {
            doc: bundled_doc(),
            source: "bundled",
            synced_at: None,
        })
    })
}

fn cooldowns() -> &'static Mutex<HashMap<String, Instant>> {
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bundled_doc() -> Value {
    serde_json::from_str(include_str!("../assets/free-registry.json"))
        .expect("bundled free-registry.json must parse")
}

fn validate_registry_doc(doc: &Value) -> Result<(), AppError> {
    if doc.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(AppError::BadRequest(
            "free-registry version must be 1".into(),
        ));
    }
    let providers = doc
        .get("providers")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::BadRequest("free-registry must carry a providers array".into()))?;
    let mut ids = HashSet::new();
    for entry in providers {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| AppError::BadRequest("free-registry provider id is invalid".into()))?;
        if !ids.insert(id) {
            return Err(AppError::BadRequest(format!(
                "free-registry contains duplicate provider id {id}"
            )));
        }
        if entry.get("class").and_then(Value::as_str) != Some("api") {
            return Err(AppError::BadRequest(format!(
                "free-registry provider {id} must have class api"
            )));
        }
        let access = entry.get("access").and_then(Value::as_str).unwrap_or("");
        if !matches!(access, "anonymous" | "key") {
            return Err(AppError::BadRequest(format!(
                "free-registry provider {id} has an invalid access mode"
            )));
        }
        if !matches!(
            entry.get("status").and_then(Value::as_str),
            Some("active" | "deprecated")
        ) {
            return Err(AppError::BadRequest(format!(
                "free-registry provider {id} has an invalid status"
            )));
        }
        if entry
            .get("limits")
            .is_some_and(|limits| !limits.is_object())
        {
            return Err(AppError::BadRequest(format!(
                "free-registry provider {id} limits must be an object"
            )));
        }
        if access == "anonymous" {
            let base_url = entry
                .get("baseUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::BadRequest(format!("provider {id} needs baseUrl")))?;
            let parsed = url::Url::parse(base_url)
                .map_err(|e| AppError::BadRequest(format!("provider {id} baseUrl: {e}")))?;
            if parsed.scheme() != "https"
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(AppError::BadRequest(format!(
                    "provider {id} baseUrl must be credential-free HTTPS"
                )));
            }
            crate::ssrf_guard::assert_public_url(base_url).map_err(AppError::BadRequest)?;
            if !entry
                .get("models")
                .and_then(Value::as_array)
                .is_some_and(|models| {
                    !models.is_empty()
                        && models.iter().all(|model| {
                            model.as_str().is_some_and(|model| !model.trim().is_empty())
                        })
                })
            {
                return Err(AppError::BadRequest(format!(
                    "provider {id} needs a non-empty models array"
                )));
            }
        } else {
            let provider = entry
                .get("provider")
                .and_then(Value::as_str)
                .filter(|provider| !provider.trim().is_empty())
                .ok_or_else(|| AppError::BadRequest(format!("provider {id} needs provider")))?;
            if crate::providers::provider_entry(provider).is_none() {
                return Err(AppError::BadRequest(format!(
                    "provider {id} references unknown provider {provider}"
                )));
            }
            if entry.get("models").is_some_and(|models| {
                !models.as_array().is_some_and(|models| {
                    models
                        .iter()
                        .all(|model| model.as_str().is_some_and(|model| !model.trim().is_empty()))
                })
            }) {
                return Err(AppError::BadRequest(format!(
                    "provider {id} models must be an array of strings"
                )));
            }
        }
    }
    Ok(())
}

/// Load the cached registry over the bundled copy at startup. Network sync is
/// admin-triggered; startup must not make network calls.
pub fn init(state: &AppState) {
    let path = state.config.data_dir.join("free-registry.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<Value>(&text) {
            Ok(doc) if validate_registry_doc(&doc).is_ok() => {
                let mut cell = registry_cell().write().expect("registry lock");
                cell.doc = doc;
                cell.source = "cache";
                let synced_at = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339());
                cell.synced_at = synced_at;
            }
            Ok(_) => {
                tracing::warn!(
                    "cached free-registry.json failed schema validation; using bundled copy"
                );
            }
            Err(error) => {
                tracing::warn!(error=%error, "cached free-registry.json invalid; using bundled copy");
            }
        }
    }
}

pub fn start_sync_loop(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = sync(&state).await {
                tracing::warn!(error=%error, "periodic free-registry sync failed; keeping last valid registry");
            }
        }
    });
}

fn registry_url() -> &'static str {
    REGISTRY_URL
}

/// Fetch the online registry, validate it, cache it, and serve it from memory.
pub async fn sync(state: &AppState) -> Result<Value, AppError> {
    let url = registry_url();
    let parsed_url = url::Url::parse(url)
        .map_err(|e| AppError::BadRequest(format!("invalid free-registry URL: {e}")))?;
    if parsed_url.scheme() != "https"
        || !parsed_url.username().is_empty()
        || parsed_url.password().is_some()
    {
        return Err(AppError::BadRequest(
            "free-registry URL must be credential-free HTTPS".into(),
        ));
    }
    let request = crate::ssrf_guard::PublicRequest {
        method: reqwest::Method::GET,
        headers: HeaderMap::new(),
        body: None,
        timeout_ms: Some(20_000),
    };
    let response = crate::ssrf_guard::fetch_public(&url, &request)
        .await
        .map_err(|e| AppError::Upstream(format!("free-registry fetch failed: {}", e.message())))?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "free-registry fetch returned HTTP {}",
            response.status()
        )));
    }
    let doc: Value = response
        .json()
        .await
        .map_err(|e| AppError::BadRequest(format!("free-registry is not valid JSON: {e}")))?;
    validate_registry_doc(&doc)?;
    let synced_at = chrono::Utc::now().to_rfc3339();
    let path = state.config.data_dir.join("free-registry.json");
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        if let Err(e) = std::fs::write(&path, text) {
            tracing::warn!(error=%e, "could not cache free-registry.json");
        }
    }
    {
        let mut cell = registry_cell().write().expect("registry lock");
        cell.doc = doc;
        cell.source = "remote";
        cell.synced_at = Some(synced_at.clone());
    }
    Ok(json!({"synced": true, "source": "remote", "syncedAt": synced_at}))
}

pub fn doc(_state: &AppState) -> Value {
    registry_cell().read().expect("registry lock").doc.clone()
}

pub fn sync_info(_state: &AppState) -> Value {
    let cell = registry_cell().read().expect("registry lock");
    json!({
        "source": cell.source,
        "syncedAt": cell.synced_at,
        "url": registry_url(),
    })
}

/// The tier is on unless explicitly disabled.
pub fn enabled(state: &AppState) -> bool {
    state
        .db
        .settings()
        .ok()
        .and_then(|settings| settings.get("builtinFreeCombo").and_then(Value::as_bool))
        != Some(false)
}

pub fn is_virtual_provider(provider: &str) -> bool {
    provider.starts_with(VIRTUAL_PROVIDER_PREFIX)
}

fn registry_providers(state: &AppState) -> Vec<Value> {
    doc(state)
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn excluded(state: &AppState) -> HashSet<String> {
    state
        .db
        .kv_all(KV_SCOPE)
        .ok()
        .and_then(|kv| {
            kv.get("excluded").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<HashSet<_>>()
            })
        })
        .unwrap_or_default()
}

fn exposed(state: &AppState) -> HashSet<String> {
    state
        .db
        .kv_all(KV_SCOPE)
        .ok()
        .and_then(|kv| {
            kv.get("exposed").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<HashSet<_>>()
            })
        })
        .unwrap_or_default()
}

pub fn set_excluded(state: &AppState, member: &str, value: bool) -> Result<(), AppError> {
    kv_toggle(state, "excluded", member, value)
}

pub fn set_exposed(state: &AppState, member: &str, value: bool) -> Result<(), AppError> {
    kv_toggle(state, "exposed", member, value)
}

fn kv_toggle(state: &AppState, key: &str, member: &str, value: bool) -> Result<(), AppError> {
    let mut current: Vec<Value> = state
        .db
        .kv_all(KV_SCOPE)
        .ok()
        .and_then(|kv| kv.get(key).and_then(Value::as_array).cloned())
        .unwrap_or_default();
    if value {
        if !current.iter().any(|item| item.as_str() == Some(member)) {
            current.push(Value::String(member.to_string()));
        }
    } else {
        current.retain(|item| item.as_str() != Some(member));
    }
    state.db.kv_set(KV_SCOPE, key, &Value::Array(current))
}

/// Synthetic connection for a registry anonymous provider. No `apiKey` field:
/// `auth_header` then sends no Authorization header, which is what keyless
/// endpoints expect.
pub fn virtual_connection(state: &AppState, provider: &str) -> Option<Value> {
    if !enabled(state) {
        return None;
    }
    let id = provider.strip_prefix(VIRTUAL_PROVIDER_PREFIX)?;
    let cell = registry_cell().read().expect("registry lock");
    let entry = cell
        .doc
        .get("providers")?
        .as_array()?
        .iter()
        .find(|entry| {
            entry.get("id").and_then(Value::as_str) == Some(id)
                && entry.get("access").and_then(Value::as_str) == Some("anonymous")
        })?;
    let status = entry.get("status").and_then(Value::as_str);
    if status.is_some_and(|status| status != "active") {
        return None;
    }
    let base_url = entry.get("baseUrl").and_then(Value::as_str)?;
    Some(json!({
        "id": format!("free:{id}"),
        "provider": provider,
        "authType": "apikey",
        "name": entry.get("name").cloned().unwrap_or(json!(id)),
        "isActive": true,
        "providerSpecificData": {"baseUrl": base_url},
    }))
}

fn registry_member_id(state: &AppState, provider: &str) -> Option<String> {
    registry_providers(state).into_iter().find_map(|entry| {
        (entry.get("access").and_then(Value::as_str) == Some("key")
            && entry.get("provider").and_then(Value::as_str) == Some(provider))
        .then(|| entry.get("id").and_then(Value::as_str).map(str::to_string))
        .flatten()
    })
}

fn is_exposed(
    state: &AppState,
    provider: &str,
    member_id: Option<&str>,
    conn: Option<&Value>,
) -> bool {
    let exposed = exposed(state);
    conn.and_then(|conn| {
        conn.pointer("/providerSpecificData/freeTierExpose")
            .and_then(Value::as_bool)
    }) == Some(true)
        || exposed.contains(provider)
        || member_id.is_some_and(|member| exposed.contains(member))
}

/// Provider ids hidden from API-key model discovery while the tier is on.
pub fn hidden_providers(state: &AppState) -> HashSet<String> {
    let connections = state
        .db
        .provider_connections(None, None)
        .unwrap_or_default();
    let mut hidden = HashSet::new();

    for conn in connections
        .iter()
        .filter(|conn| conn.get("isActive").and_then(Value::as_bool) != Some(false))
    {
        let Some(provider) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        let in_pool = conn
            .pointer("/providerSpecificData/freePool")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || auto_join(state, provider);
        let member_id = registry_member_id(state, provider);
        if in_pool && !is_exposed(state, provider, member_id.as_deref(), Some(conn)) {
            hidden.insert(crate::providers::canonical_provider(provider));
        }
    }

    // Exclusion means "do not use in combo-free", not "make this core
    // provider directly callable". Direct visibility is controlled solely by
    // the explicit expose-directly escape hatch.
    for entry in registry_providers(state) {
        if entry.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let Some(member) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let access = entry.get("access").and_then(Value::as_str).unwrap_or("");
        let provider = if access == "anonymous" {
            format!("{VIRTUAL_PROVIDER_PREFIX}{member}")
        } else {
            let Some(provider) = entry.get("provider").and_then(Value::as_str) else {
                continue;
            };
            provider.to_string()
        };
        if !is_exposed(state, &provider, Some(member), None) {
            hidden.insert(crate::providers::canonical_provider(&provider));
        }
    }
    hidden
}

pub fn is_hidden_model(state: &AppState, requested: &str) -> bool {
    let aliases = state.db.kv_all("modelAliases").unwrap_or_default();
    let mut candidate = requested.to_string();
    let mut visited = HashSet::new();
    while visited.insert(candidate.clone()) && visited.len() <= 64 {
        let (provider, _) = crate::providers::split_model(&candidate);
        if let Some(provider) = provider {
            let hidden = hidden_providers(state);
            return hidden.contains(provider)
                || hidden.contains(&crate::providers::canonical_provider(provider));
        }
        let Some(next) = aliases.get(&candidate).and_then(Value::as_str) else {
            break;
        };
        candidate = next.to_string();
    }
    let hidden = hidden_providers(state);
    hidden.iter().any(|provider| {
        if let Some(member) = provider.strip_prefix(VIRTUAL_PROVIDER_PREFIX) {
            return registry_providers(state).iter().any(|entry| {
                entry.get("id").and_then(Value::as_str) == Some(member)
                    && entry
                        .get("models")
                        .and_then(Value::as_array)
                        .is_some_and(|models| {
                            models
                                .iter()
                                .any(|model| model.as_str() == Some(candidate.as_str()))
                        })
            });
        }
        crate::providers::models_for(provider)
            .iter()
            .any(|model| model.get("id").and_then(Value::as_str) == Some(candidate.as_str()))
            || state
                .db
                .provider_connections(Some(provider), None)
                .unwrap_or_default()
                .iter()
                .any(|connection| {
                    [
                        "/providerSpecificData/freeTierModels",
                        "/providerSpecificData/enabledModels",
                    ]
                    .iter()
                    .filter_map(|path| connection.pointer(path).and_then(Value::as_array))
                    .flatten()
                    .any(|model| model.as_str() == Some(candidate.as_str()))
                })
    })
}

/// Whether a member model is explicitly allowed to bypass the opaque combo.
/// This is independent of the master switch: exposed members stay callable
/// directly while `combo-free` itself is disabled.
pub fn is_exposed_model(state: &AppState, requested: &str) -> bool {
    let aliases = state.db.kv_all("modelAliases").unwrap_or_default();
    let mut candidate = requested.to_string();
    let mut visited = HashSet::new();
    while visited.insert(candidate.clone()) && visited.len() <= 64 {
        let (provider, _) = crate::providers::split_model(&candidate);
        if let Some(provider) = provider {
            return is_exposed_provider(state, provider);
        }
        let Some(next) = aliases.get(&candidate).and_then(Value::as_str) else {
            return false;
        };
        candidate = next.to_string();
    }
    false
}

fn is_exposed_provider(state: &AppState, provider: &str) -> bool {
    let virtual_member = provider
        .strip_prefix(VIRTUAL_PROVIDER_PREFIX)
        .filter(|member| {
            registry_providers(state).iter().any(|entry| {
                entry.get("id").and_then(Value::as_str) == Some(*member)
                    && entry.get("access").and_then(Value::as_str) == Some("anonymous")
            })
        });
    let member_id = virtual_member
        .map(str::to_string)
        .or_else(|| registry_member_id(state, provider));
    let connections = state
        .db
        .provider_connections(Some(provider), None)
        .unwrap_or_default();
    let is_local_member = connections.iter().any(|connection| {
        connection
            .pointer("/providerSpecificData/freePool")
            .and_then(Value::as_bool)
            == Some(true)
    });
    if member_id.is_none() && !is_local_member {
        return false;
    }
    is_exposed(state, provider, member_id.as_deref(), None)
        || connections
            .iter()
            .any(|connection| is_exposed(state, provider, member_id.as_deref(), Some(connection)))
}

/// Metadata for an explicitly exposed registry virtual model, which is not in
/// the ordinary static provider catalog.
pub fn exposed_virtual_model_info(state: &AppState, requested: &str) -> Option<Value> {
    if !is_exposed_model(state, requested) {
        return None;
    }
    let (provider, model) = crate::providers::split_model(requested);
    let member = provider?.strip_prefix(VIRTUAL_PROVIDER_PREFIX)?;
    let entry = registry_providers(state).into_iter().find(|entry| {
        entry.get("id").and_then(Value::as_str) == Some(member)
            && entry.get("access").and_then(Value::as_str) == Some("anonymous")
            && entry
                .get("models")
                .and_then(Value::as_array)
                .is_some_and(|models| {
                    models
                        .iter()
                        .any(|candidate| candidate.as_str() == Some(model))
                })
    })?;
    Some(json!({
        "id": requested,
        "object": "model",
        "name": entry.get("name").and_then(Value::as_str).unwrap_or(member),
        "owned_by": "9router",
        "kind": "llm",
        "description": "Directly exposed member of the 9Router free tier.",
    }))
}

fn auto_join(state: &AppState, provider: &str) -> bool {
    if is_virtual_provider(provider) {
        return true;
    }
    let member_id = registry_member_id(state, provider);
    !member_id
        .as_deref()
        .is_some_and(|member| excluded(state).contains(member))
        && registry_providers(state).iter().any(|entry| {
            entry.get("access").and_then(Value::as_str) == Some("key")
                && entry.get("provider").and_then(Value::as_str) == Some(provider)
                && entry.get("status").and_then(Value::as_str) == Some("active")
        })
}

/// Ordered fallback targets for `combo-free`: anonymous registry members
/// first (they work with zero configuration), then keyed members in registry
/// order. Members in cooldown are skipped unless every member is cooling down.
pub fn pool_targets(state: &AppState) -> Result<Vec<String>, AppError> {
    if !enabled(state) {
        return Ok(Vec::new());
    }
    let excluded = excluded(state);
    let mut fresh: Vec<String> = Vec::new();
    let mut cooling: Vec<String> = Vec::new();
    let cooling_now = cooling_members();

    for entry in registry_providers(state) {
        if entry.get("status").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let access = entry.get("access").and_then(Value::as_str).unwrap_or("");
        let member = entry.get("id").and_then(Value::as_str).unwrap_or("");
        if member.is_empty() || excluded.contains(member) {
            continue;
        }
        match access {
            "anonymous" => {
                let provider = format!("{VIRTUAL_PROVIDER_PREFIX}{member}");
                let models = entry
                    .get("models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str);
                for model in models {
                    let target = format!("{provider}/{model}");
                    if cooling_now.contains(&provider) {
                        cooling.push(target);
                    } else {
                        fresh.push(target);
                    }
                }
            }
            "key" => {
                let Some(provider) = entry.get("provider").and_then(Value::as_str) else {
                    continue;
                };
                let connections: Vec<Value> = state
                    .db
                    .provider_connections(Some(provider), Some(true))?
                    .into_iter()
                    .filter(|conn| {
                        connection_in_pool(state, conn, provider) && connection_passed_test(conn)
                    })
                    .filter(|conn| has_api_key(conn))
                    .collect();
                if connections.is_empty() {
                    continue;
                }
                for conn in &connections {
                    for model in keyed_models(provider, conn, &entry) {
                        let target = format!("{provider}/{model}");
                        if cooling_now.contains(provider) {
                            cooling.push(target);
                        } else {
                            fresh.push(target);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Operator-added free endpoints use ordinary provider connections tagged
    // `freePool: true`; they are not registry-owned and are never rewritten by
    // an online sync.
    let registry_provider_ids: HashSet<String> = registry_providers(state)
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("active"))
        .filter_map(|entry| {
            entry
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    for conn in state.db.provider_connections(None, Some(true))? {
        let Some(provider) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        if registry_provider_ids.contains(provider)
            || !conn
                .pointer("/providerSpecificData/freePool")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || !connection_passed_test(&conn)
            || conn
                .pointer("/providerSpecificData/freeTierExcluded")
                .and_then(Value::as_bool)
                == Some(true)
        {
            continue;
        }
        for model in keyed_models(provider, &conn, &Value::Null) {
            let target = format!("{provider}/{model}");
            if cooling_now.contains(provider) {
                cooling.push(target);
            } else {
                fresh.push(target);
            }
        }
    }

    if fresh.is_empty() && !cooling.is_empty() {
        // Availability over purity: when every member is cooling down, still
        // offer them rather than hard-failing the model.
        return Ok(cooling);
    }
    Ok(fresh)
}

fn connection_in_pool(state: &AppState, conn: &Value, provider: &str) -> bool {
    if conn
        .pointer("/providerSpecificData/freeTierExcluded")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return false;
    }
    conn.pointer("/providerSpecificData/freePool")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || auto_join(state, provider)
}

fn connection_passed_test(conn: &Value) -> bool {
    matches!(
        conn.get("testStatus").and_then(Value::as_str),
        Some("success" | "active")
    )
}

fn has_api_key(conn: &Value) -> bool {
    conn.get("apiKey")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.trim().is_empty())
}

fn keyed_models(provider: &str, conn: &Value, registry_entry: &Value) -> Vec<String> {
    if let Some(models) = registry_entry.get("models").and_then(Value::as_array) {
        let models: Vec<String> = models
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !models.is_empty() {
            return models;
        }
    }
    if let Some(enabled) = conn
        .pointer("/providerSpecificData/freeTierModels")
        .or_else(|| conn.pointer("/providerSpecificData/enabledModels"))
        .and_then(Value::as_array)
    {
        let models: Vec<String> = enabled
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !models.is_empty() {
            return models;
        }
    }
    crate::providers::models_for(provider)
        .into_iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .take(MAX_MODELS_PER_KEYED_PROVIDER)
        .collect()
}

fn cooling_members() -> HashSet<String> {
    let now = Instant::now();
    cooldowns()
        .lock()
        .map(|map| {
            map.iter()
                .filter(|(_, until)| **until > now)
                .map(|(member, _)| member.clone())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default()
}

fn member_provider(state: &AppState, target: &str) -> Option<String> {
    let (provider, _) = crate::providers::split_model(target);
    let provider = provider?.to_string();
    (is_virtual_provider(&provider)
        || registry_member_provider(&provider)
        || state
            .db
            .provider_connections(Some(&provider), None)
            .ok()
            .is_some_and(|connections| {
                connections.iter().any(|conn| {
                    conn.pointer("/providerSpecificData/freePool")
                        .and_then(Value::as_bool)
                        == Some(true)
                })
            }))
    .then_some(provider)
}

fn registry_member_provider(provider: &str) -> bool {
    let cell = registry_cell().read().expect("registry lock");
    cell.doc
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| {
            entry.get("access").and_then(Value::as_str) == Some("key")
                && entry.get("provider").and_then(Value::as_str) == Some(provider)
        })
}

/// Record a member outcome. Called from the gateway fallback loops so a dead
/// member stops consuming attempts for [`MEMBER_COOLDOWN`].
pub fn note_result(state: &AppState, target: &str, ok: bool) {
    let Some(provider) = member_provider(state, target) else {
        return;
    };
    let mut map = match cooldowns().lock() {
        Ok(map) => map,
        Err(_) => return,
    };
    if ok {
        map.remove(&provider);
    } else {
        map.insert(provider, Instant::now() + MEMBER_COOLDOWN);
    }
}

/// Admin view for `GET /api/free-tier`: the tier switch, sync state, every
/// registry member with its local controls, and the current pool targets.
pub fn admin_view(state: &AppState) -> Result<Value, AppError> {
    let excluded = excluded(state);
    let exposed = exposed(state);
    let cooling = cooling_members();
    let mut members = Vec::new();
    for entry in registry_providers(state) {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let access = entry.get("access").and_then(Value::as_str).unwrap_or("");
        let mut member = entry.clone();
        member["excluded"] = json!(excluded.contains(id));
        member["exposed"] = json!(exposed.contains(id));
        member["kind"] = json!(if access == "anonymous" {
            "virtual"
        } else {
            "keyed"
        });
        if access == "key" {
            let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");
            let connections: Vec<Value> = state
                .db
                .provider_connections(Some(provider), None)?
                .into_iter()
                .map(|conn| {
                    json!({
                        "id": conn.get("id"),
                        "name": conn.get("name"),
                        "isActive": conn.get("isActive"),
                        "testStatus": conn.get("testStatus"),
                        "hasKey": conn.get("apiKey").and_then(Value::as_str).is_some_and(|k| !k.is_empty()),
                        "inPool": connection_in_pool(state, &conn, provider),
                        "managed": conn.pointer("/providerSpecificData/freeTierMemberId").and_then(Value::as_str) == Some(id),
                    })
                })
                .collect();
            member["connections"] = json!(connections);
        }
        if access == "anonymous" {
            member["cooling"] = json!(cooling.contains(&format!("{VIRTUAL_PROVIDER_PREFIX}{id}")));
        }
        members.push(member);
    }
    // Local additions: connections tagged into the pool whose provider is not
    // a registry-keyed provider.
    let registry_providers: HashSet<String> = registry_providers(state)
        .iter()
        .filter_map(|entry| {
            entry
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let additions: Vec<Value> = state
        .db
        .provider_connections(None, None)?
        .into_iter()
        .filter(|conn| {
            conn.pointer("/providerSpecificData/freePool")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter(|conn| {
            !conn
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|provider| registry_providers.contains(provider))
        })
        .map(|conn| {
            json!({
                "id": conn.get("id"),
                "kind": "addition",
                "provider": conn.get("provider"),
                "name": conn.get("name"),
                "isActive": conn.get("isActive"),
                "testStatus": conn.get("testStatus"),
                "inPool": connection_in_pool(state, &conn, conn.get("provider").and_then(Value::as_str).unwrap_or("")),
                "excluded": conn.pointer("/providerSpecificData/freeTierExcluded").and_then(Value::as_bool) == Some(true),
                "exposed": conn.pointer("/providerSpecificData/freeTierExpose").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect();
    Ok(json!({
        "enabled": enabled(state),
        "model": FREE_COMBO_MODEL,
        "sync": sync_info(state),
        "members": members,
        "additions": additions,
        "targets": pool_targets(state)?,
    }))
}

fn safe_connection(mut connection: Value) -> Value {
    if let Some(object) = connection.as_object_mut() {
        for field in ["apiKey", "accessToken", "refreshToken", "clientSecret"] {
            object.remove(field);
        }
    }
    connection
}

fn api_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
    crate::inference_media::json_response(status, value)
}

/// Admin-only management routes. Authentication is enforced by the shared
/// management dispatcher before this handler is reached.
pub async fn handle_admin_api(
    state: &AppState,
    method: &Method,
    path: &str,
    body: Value,
) -> Result<Response<Body>, AppError> {
    match (method.as_str(), path) {
        ("GET", "/api/free-tier") => api_response(StatusCode::OK, admin_view(state)?),
        ("GET", "/api/free-tier/registry") => api_response(
            StatusCode::OK,
            json!({"registry": doc(state), "sync": sync_info(state)}),
        ),
        ("POST", "/api/free-tier/sync") => {
            let result = sync(state).await?;
            api_response(
                StatusCode::OK,
                json!({"result":result,"freeTier":admin_view(state)?}),
            )
        }
        ("POST", "/api/free-tier/members") => {
            let connection = create_member(state, &body)?;
            api_response(
                StatusCode::CREATED,
                json!({"connection":safe_connection(connection),"freeTier":admin_view(state)?}),
            )
        }
        ("PATCH", path) if path.starts_with("/api/free-tier/members/") => {
            let member = path.trim_start_matches("/api/free-tier/members/");
            if member.is_empty() || member.contains('/') {
                return Err(AppError::NotFound("free-tier member not found".into()));
            }
            patch_member(state, member, &body)?;
            api_response(StatusCode::OK, admin_view(state)?)
        }
        ("DELETE", path) if path.starts_with("/api/free-tier/members/") => {
            let member = path.trim_start_matches("/api/free-tier/members/");
            if member.is_empty() || member.contains('/') {
                return Err(AppError::NotFound("free-tier member not found".into()));
            }
            delete_member(state, member)?;
            api_response(
                StatusCode::OK,
                json!({"success":true,"freeTier":admin_view(state)?}),
            )
        }
        _ => Err(AppError::NotFound("free-tier route not found".into())),
    }
}

fn create_member(state: &AppState, body: &Value) -> Result<Value, AppError> {
    let provider_data = body
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(member_id) = body.get("memberId").and_then(Value::as_str) {
        let entry = registry_providers(state)
            .into_iter()
            .find(|entry| {
                entry.get("id").and_then(Value::as_str) == Some(member_id)
                    && entry.get("access").and_then(Value::as_str) == Some("key")
                    && entry.get("status").and_then(Value::as_str) == Some("active")
            })
            .ok_or_else(|| {
                AppError::BadRequest("unknown or inactive keyed registry member".into())
            })?;
        let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");
        let existing = state.db.provider_connections(Some(provider), None)?;
        if let Some(existing) = existing.into_iter().find(|conn| {
            conn.pointer("/providerSpecificData/freeTierMemberId")
                .and_then(Value::as_str)
                == Some(member_id)
        }) {
            return Ok(existing);
        }
        let mut data = serde_json::Map::new();
        data.insert("freePool".into(), json!(true));
        data.insert("freeTierMemberId".into(), json!(member_id));
        if let Some(models) = body.get("models").and_then(Value::as_array) {
            data.insert("freeTierModels".into(), json!(models));
        }
        let mut connection = json!({
            "provider": provider,
            "authType": "apikey",
            "name": entry.get("name").and_then(Value::as_str).unwrap_or(provider),
            "priority": 0,
            "isActive": false,
            "testStatus": "unknown",
            "providerSpecificData": Value::Object(data),
        });
        if let Some(key) = body
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            connection["apiKey"] = json!(key);
        }
        return state.db.create_connection(connection);
    }

    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|provider| {
            provider.starts_with("openai-compatible-")
                || provider.starts_with("anthropic-compatible-")
                || crate::providers::provider_entry(provider).is_some()
        })
        .ok_or_else(|| AppError::BadRequest("provider must be a supported API provider".into()))?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .ok_or_else(|| AppError::BadRequest("name is required (max 120 characters)".into()))?;
    let mut data = Value::Object(provider_data);
    let data_object = data.as_object_mut().expect("provider data is object");
    data_object.insert("freePool".into(), json!(true));
    data_object.remove("freeTierMemberId");
    if provider.starts_with("openai-compatible-") || provider.starts_with("anthropic-compatible-") {
        let base_url = data_object
            .get("baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AppError::BadRequest("providerSpecificData.baseUrl is required".into())
            })?;
        let parsed = url::Url::parse(base_url)
            .map_err(|e| AppError::BadRequest(format!("invalid baseUrl: {e}")))?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AppError::BadRequest(
                "custom free provider baseUrl must be credential-free HTTPS".into(),
            ));
        }
        crate::ssrf_guard::assert_public_url(base_url).map_err(AppError::BadRequest)?;
    }
    let mut connection = json!({
        "provider": provider,
        "authType": "apikey",
        "name": name,
        "priority": 0,
        "isActive": false,
        "testStatus": "unknown",
        "providerSpecificData": data,
    });
    if let Some(key) = body
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        connection["apiKey"] = json!(key);
    }
    state.db.create_connection(connection)
}

fn patch_member(state: &AppState, member: &str, body: &Value) -> Result<(), AppError> {
    let registry_entry = registry_providers(state)
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(member));
    if let Some(entry) = registry_entry {
        if entry.get("status").and_then(Value::as_str) != Some("active") {
            return Err(AppError::BadRequest(
                "deprecated registry members are read-only".into(),
            ));
        }
        if let Some(value) = body.get("excluded").and_then(Value::as_bool) {
            set_excluded(state, member, value)?;
        }
        if let Some(value) = body.get("exposeDirectly").and_then(Value::as_bool) {
            set_exposed(state, member, value)?;
        }
        if body.get("excluded").and_then(Value::as_bool).is_none()
            && body
                .get("exposeDirectly")
                .and_then(Value::as_bool)
                .is_none()
        {
            return Err(AppError::BadRequest(
                "excluded or exposeDirectly boolean is required".into(),
            ));
        }
        return Ok(());
    }

    let mut connection = state
        .db
        .provider_connection(member)?
        .ok_or_else(|| AppError::NotFound("free-tier member not found".into()))?;
    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let owned = connection
        .pointer("/providerSpecificData/freePool")
        .and_then(Value::as_bool)
        == Some(true)
        || registry_member_id(state, &provider).is_some();
    if !owned {
        return Err(AppError::BadRequest(
            "connection is not a free-tier member".into(),
        ));
    }
    let mut data = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(value) = body.get("excluded").and_then(Value::as_bool) {
        data.insert("freeTierExcluded".into(), json!(value));
    }
    if let Some(value) = body.get("exposeDirectly").and_then(Value::as_bool) {
        if let Some(member_id) = registry_member_id(state, &provider) {
            set_exposed(state, &member_id, value)?;
        } else {
            data.insert("freeTierExpose".into(), json!(value));
        }
    }
    if !body.get("excluded").and_then(Value::as_bool).is_some()
        && !body
            .get("exposeDirectly")
            .and_then(Value::as_bool)
            .is_some()
    {
        return Err(AppError::BadRequest(
            "excluded or exposeDirectly boolean is required".into(),
        ));
    }
    connection["providerSpecificData"] = Value::Object(data);
    state.db.update_connection(
        member,
        json!({"providerSpecificData":connection["providerSpecificData"].clone()}),
    )?;
    Ok(())
}

fn delete_member(state: &AppState, member: &str) -> Result<(), AppError> {
    if let Some(entry) = registry_providers(state)
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(member))
    {
        if entry.get("access").and_then(Value::as_str) == Some("anonymous") {
            return Err(AppError::BadRequest(
                "anonymous registry definitions cannot be removed".into(),
            ));
        }
        let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");
        let connections = state.db.provider_connections(Some(provider), None)?;
        let mut removed = false;
        for connection in connections {
            if connection
                .pointer("/providerSpecificData/freeTierMemberId")
                .and_then(Value::as_str)
                == Some(member)
            {
                let id = connection.get("id").and_then(Value::as_str).unwrap_or("");
                removed |= state.db.delete_connection(id)?;
            }
        }
        if !removed {
            return Err(AppError::NotFound(
                "no free-tier connection to remove".into(),
            ));
        }
        set_excluded(state, member, false)?;
        set_exposed(state, member, false)?;
        return Ok(());
    }
    let connection = state
        .db
        .provider_connection(member)?
        .ok_or_else(|| AppError::NotFound("free-tier member not found".into()))?;
    if connection
        .pointer("/providerSpecificData/freePool")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(AppError::BadRequest(
            "only a local free-tier addition can be removed here".into(),
        ));
    }
    state.db.delete_connection(member)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Db};
    use tempfile::TempDir;

    fn test_state(temp: &TempDir) -> AppState {
        let db_path = temp.path().join("data.sqlite");
        let db = Db::open(&db_path).expect("open test database");
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
        .expect("create test app state")
    }

    #[test]
    fn bundled_registry_is_valid_and_rejects_ssrf_targets() {
        validate_registry_doc(&bundled_doc()).expect("bundled registry schema");
        let bad = json!({
            "version": 1,
            "providers": [{
                "id": "local", "class": "api", "access": "anonymous",
                "name": "Local", "baseUrl": "https://127.0.0.1/v1",
                "models": ["model"], "status": "active"
            }]
        });
        assert!(validate_registry_doc(&bad).is_err());
    }

    #[test]
    fn builtin_switch_defaults_on_and_disables_pool_and_virtual_connections() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        assert_eq!(state.db.settings().unwrap()["builtinFreeCombo"], true);
        assert!(!pool_targets(&state).unwrap().is_empty());
        let bundled = doc(&state);
        let providers = bundled["providers"].as_array().unwrap();
        let kilo = providers
            .iter()
            .find(|entry| entry["id"] == "kilo")
            .expect("Kilo Auto Free is in the bundled registry");
        assert!(kilo["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model == "kilo-auto/free"));
        assert!(!providers.iter().any(|entry| entry["id"] == "llm7"));
        assert!(!providers.iter().any(|entry| entry["id"] == "ovh"));
        state
            .db
            .update_settings(json!({"builtinFreeCombo":false}))
            .unwrap();
        assert!(!enabled(&state));
        assert!(pool_targets(&state).unwrap().is_empty());
        assert!(virtual_connection(&state, "openai-compatible-free-llm7").is_none());
        assert!(is_hidden_model(&state, "groq/llama-3.3-70b-versatile"));
        set_exposed(&state, "groq", true).unwrap();
        assert!(!is_hidden_model(&state, "groq/llama-3.3-70b-versatile"));
        assert!(is_exposed_model(&state, "groq/llama-3.3-70b-versatile"));
        set_exposed(&state, "kilo", true).unwrap();
        let kilo_model = "openai-compatible-free-kilo/kilo-auto/free";
        assert!(is_exposed_model(&state, kilo_model));
        assert_eq!(
            exposed_virtual_model_info(&state, kilo_model).unwrap()["id"],
            kilo_model
        );
    }

    #[test]
    fn keyed_members_require_an_active_tested_connection_and_honor_exclusion() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let connection = state
            .db
            .create_connection(json!({
                "provider": "groq",
                "authType": "apikey",
                "name": "Groq test",
                "apiKey": "test-key",
                "isActive": true,
                "testStatus": "unknown",
                "providerSpecificData": {
                    "freePool": true,
                    "enabledModels": ["llama-3.3-70b-versatile"]
                }
            }))
            .unwrap();
        let id = connection["id"].as_str().unwrap();
        assert!(!pool_targets(&state)
            .unwrap()
            .iter()
            .any(|target| target == "groq/llama-3.3-70b-versatile"));

        state
            .db
            .update_connection(id, json!({"testStatus":"success"}))
            .unwrap();
        assert!(pool_targets(&state)
            .unwrap()
            .iter()
            .any(|target| target == "groq/llama-3.3-70b-versatile"));
        set_excluded(&state, "groq", true).unwrap();
        assert!(!pool_targets(&state)
            .unwrap()
            .iter()
            .any(|target| target == "groq/llama-3.3-70b-versatile"));
        assert!(is_hidden_model(&state, "groq/llama-3.3-70b-versatile"));
        set_exposed(&state, "groq", true).unwrap();
        assert!(!is_hidden_model(&state, "groq/llama-3.3-70b-versatile"));
    }

    #[test]
    fn keyed_registry_seed_is_inactive_and_removable_without_exposing_secrets() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let connection = create_member(
            &state,
            &json!({
                "memberId": "groq",
                "apiKey": "never-return-this-key"
            }),
        )
        .unwrap();
        assert_eq!(connection["isActive"], false);
        assert_eq!(connection["testStatus"], "unknown");
        assert_eq!(connection["providerSpecificData"]["freePool"], true);
        let view = admin_view(&state).unwrap();
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("never-return-this-key"));
        let id = connection["id"].as_str().unwrap().to_string();
        delete_member(&state, &id).unwrap();
        assert!(state.db.provider_connection(&id).unwrap().is_none());
    }

    #[test]
    fn tested_local_additions_join_pool_and_receive_cooldowns() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp);
        let provider = "openai-compatible-chat-localtest";
        state
            .db
            .create_connection(json!({
                "provider": provider,
                "authType": "apikey",
                "name": "Local free API",
                "isActive": true,
                "testStatus": "success",
                "providerSpecificData": {
                    "freePool": true,
                    "baseUrl": "https://api.example.com/v1",
                    "enabledModels": ["free-model"]
                }
            }))
            .unwrap();
        let target = format!("{provider}/free-model");
        assert!(pool_targets(&state).unwrap().contains(&target));
        note_result(&state, &target, false);
        assert!(!pool_targets(&state).unwrap().contains(&target));
        note_result(&state, &target, true);
        assert!(pool_targets(&state).unwrap().contains(&target));
    }
}
