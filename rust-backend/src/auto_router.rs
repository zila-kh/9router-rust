use axum::http::HeaderMap;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use crate::{error::AppError, providers, state::AppState, translate::Format};

const ROUTER_VERSION: &str = "2";
const DEFAULT_LARGE_CONTEXT_TOKENS: u64 = 160_000;
const DEFAULT_HARD_CONTEXT_TOKENS: u64 = 850_000;
const MIN_PREFIX_CHARS_FOR_AFFINITY: usize = 512;
const IMAGE_TOKEN_ESTIMATE: u64 = 4_096;

#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub targets: Vec<String>,
    pub estimated_input_tokens: u64,
    pub task: String,
    pub risk: u8,
    pub complexity: u8,
    pub affinity_key: Option<String>,
    pub affinity_hit: bool,
}

#[derive(Debug, Clone)]
struct RouterConfig {
    enabled: bool,
    aliases: Vec<String>,
    profiles: Vec<ModelProfile>,
    max_fallbacks: usize,
    large_context_tokens: u64,
    hard_context_tokens: u64,
    allow_large_context: bool,
    cache_affinity: bool,
    cache_affinity_ttl_secs: u64,
}

#[derive(Debug, Clone)]
struct ModelProfile {
    name: String,
    model: String,
    tier: String,
    roles: Vec<String>,
    context_window: u64,
    base_score: i64,
    target_share: Option<f64>,
    daily_token_budget: Option<u64>,
    weekly_token_budget: Option<u64>,
    daily_request_budget: Option<u64>,
    weekly_request_budget: Option<u64>,
}

#[derive(Debug, Clone)]
struct Candidate {
    profile: ModelProfile,
    provider: String,
    upstream_model: String,
    score: i64,
}

#[derive(Debug, Clone)]
struct AffinityEntry {
    model: String,
    touched: Instant,
}

#[derive(Debug, Clone, Copy)]
struct ModelUsage {
    tokens: u64,
    requests: u64,
}

static AFFINITY: Lazy<DashMap<String, AffinityEntry>> = Lazy::new(DashMap::new);

pub fn plan_if_requested(
    state: &AppState,
    requested: &str,
    canonical: &Value,
    headers: &HeaderMap,
) -> Result<Option<RoutePlan>, AppError> {
    let config = RouterConfig::load(state)?;
    if !config.enabled
        || !config
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(requested))
    {
        return Ok(None);
    }

    let estimated_input_tokens = estimate_input_tokens(canonical);
    let allow_large_header = headers
        .get("x-9router-allow-large-context")
        .and_then(|value| value.to_str().ok())
        .is_some_and(truthy);
    if estimated_input_tokens > config.hard_context_tokens
        && !config.allow_large_context
        && !allow_large_header
    {
        return Err(AppError::BadRequest(format!(
            "auto router estimated {estimated_input_tokens} input tokens, above its safety limit of {}. Compact/retrieve less context, raise autoRouter.hardContextTokens, set autoRouter.allowLargeContext=true, or explicitly choose a model to bypass the optional auto-router guard",
            config.hard_context_tokens
        )));
    }

    let latest = latest_user_text(canonical);
    let task = classify_task(&latest);
    let risk = risk_score(&latest, &task);
    let complexity = complexity_score(canonical, estimated_input_tokens, &latest, &task);
    let affinity_key = config
        .cache_affinity
        .then(|| affinity_key(canonical, headers))
        .flatten();

    cleanup_affinity(config.cache_affinity_ttl_secs);
    let affinity_model = affinity_key.as_ref().and_then(|key| {
        AFFINITY.get(key).and_then(|entry| {
            (entry.touched.elapsed() <= Duration::from_secs(config.cache_affinity_ttl_secs))
                .then(|| entry.model.clone())
        })
    });

    // The router only needs provider/model token totals. We intentionally consume
    // the existing stats shape for compatibility; this can be replaced by a
    // dedicated aggregate query later without changing routing semantics.
    let today = state.db.usage_route_stats("today")?;
    let week = state.db.usage_route_stats("7d")?;
    let today_total = total_tokens(&today);
    let mut candidates = Vec::new();
    let mut seen_routes = HashSet::new();

    for profile in config.profiles.iter().cloned() {
        // Limited/scarce capacity must never become an accidental fallback for an
        // easy request merely because a cheap provider is offline.
        if !tier_eligible(&profile, risk, complexity) {
            continue;
        }
        if estimated_input_tokens > profile.context_window.saturating_mul(95) / 100 {
            continue;
        }

        let resolved = match providers::resolve_model(state, &profile.model) {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::debug!(
                    model = %profile.model,
                    error = %error,
                    "auto-router candidate unavailable"
                );
                continue;
            }
        };

        if !rust_gateway_supports_provider(&resolved.provider) {
            tracing::debug!(
                model = %profile.model,
                provider = %resolved.provider,
                "auto-router candidate skipped: provider transport is not executable by the native Rust gateway"
            );
            continue;
        }

        match state
            .db
            .provider_connections(Some(&resolved.provider), Some(true))
        {
            Ok(connections) if !connections.is_empty() => {}
            Ok(_) => {
                tracing::debug!(
                    model = %profile.model,
                    provider = %resolved.provider,
                    "auto-router candidate skipped: no active provider connection"
                );
                continue;
            }
            Err(error) => return Err(error),
        }

        // Multiple aliases/profiles can map to the same provider + upstream model.
        // Never spend fallback slots retrying the exact same route twice.
        let route_key = format!("{}\0{}", resolved.provider, resolved.model);
        if !seen_routes.insert(route_key) {
            tracing::debug!(model = %profile.model, "auto-router duplicate route skipped");
            continue;
        }

        let today_model = model_usage(&today, &resolved.provider, &resolved.model);
        let week_model = model_usage(&week, &resolved.provider, &resolved.model);
        if budget_exhausted(&profile, today_model, week_model) {
            tracing::debug!(
                model = %profile.model,
                "auto-router candidate excluded by configured quota"
            );
            continue;
        }

        let mut score = profile.base_score;
        score += tier_bias(&profile.tier);
        score += role_bias(&profile.roles, &task);
        score += context_bias(
            &profile,
            estimated_input_tokens,
            config.large_context_tokens,
        );
        score += risk_complexity_bias(&profile, risk, complexity);
        score += quota_bias(&profile, today_model, week_model);
        score += share_bias(&profile, today_model.tokens, today_total);
        if affinity_model.as_deref() == Some(profile.model.as_str()) {
            score += 35;
        }

        candidates.push(Candidate {
            profile,
            provider: resolved.provider,
            upstream_model: resolved.model,
            score,
        });
    }

    if candidates.is_empty() {
        return Err(AppError::NotFound(
            "auto router has no safe executable candidate models; configure autoRouter.profiles with models supported by the native Rust gateway and active providers".into(),
        ));
    }

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.profile.name.cmp(&right.profile.name))
    });

    let affinity_hit = affinity_model.as_ref().is_some_and(|model| {
        candidates
            .iter()
            .any(|candidate| candidate.profile.model == *model)
    });
    let targets = candidates
        .iter()
        .take(config.max_fallbacks.min(candidates.len()))
        .map(|candidate| candidate.profile.model.clone())
        .collect::<Vec<_>>();

    tracing::info!(
        router_version = ROUTER_VERSION,
        task,
        risk,
        complexity,
        estimated_input_tokens,
        targets = ?targets,
        scores = ?candidates
            .iter()
            .map(|candidate| (
                &candidate.profile.model,
                candidate.score,
                &candidate.provider,
                &candidate.upstream_model,
            ))
            .collect::<Vec<_>>(),
        "auto-router planned request"
    );

    Ok(Some(RoutePlan {
        targets,
        estimated_input_tokens,
        task,
        risk,
        complexity,
        affinity_key,
        affinity_hit,
    }))
}

pub fn remember_success(plan: &RoutePlan, model: &str) {
    if let Some(key) = plan.affinity_key.as_ref() {
        AFFINITY.insert(
            key.clone(),
            AffinityEntry {
                model: model.to_string(),
                touched: Instant::now(),
            },
        );
    }
}

pub fn record_stream_estimate_if_passthrough(
    state: &AppState,
    plan: &RoutePlan,
    target: &str,
    caller: Format,
    wants_stream: bool,
) {
    if !wants_stream {
        return;
    }
    let Ok(resolved) = providers::resolve_model(state, target) else {
        return;
    };
    let transport = providers::transport(&resolved.provider);
    let transport_format = transport
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    if matches!(
        transport_format,
        "kiro" | "commandcode" | "cursor" | "windsurf"
    ) {
        return;
    }
    if caller != Format::from_provider(transport_format) {
        return;
    }

    let prompt = plan.estimated_input_tokens.min(i64::MAX as u64) as i64;
    if let Err(error) = state.db.usage_record(
        Some(&resolved.provider),
        Some(&resolved.model),
        None,
        "auto-router:stream-estimate",
        prompt,
        0,
        "stream_estimate",
        &json!({
            "estimated": true,
            "reason": "native pass-through streams do not expose final usage before the response is returned"
        }),
    ) {
        tracing::warn!(error = %error, model = %target, "failed to record auto-router stream usage estimate");
    }
}

pub fn annotate_response(
    response: &mut axum::http::Response<axum::body::Body>,
    plan: &RoutePlan,
    model: &str,
) {
    insert_header(response, "x-9router-auto-route", "1");
    insert_header(response, "x-9router-router-version", ROUTER_VERSION);
    insert_header(response, "x-9router-selected-model", model);
    insert_header(response, "x-9router-task", &plan.task);
    insert_header(
        response,
        "x-9router-estimated-input-tokens",
        &plan.estimated_input_tokens.to_string(),
    );
    insert_header(response, "x-9router-risk", &plan.risk.to_string());
    insert_header(
        response,
        "x-9router-complexity",
        &plan.complexity.to_string(),
    );
    insert_header(
        response,
        "x-9router-cache-affinity",
        if plan.affinity_hit { "hit" } else { "miss" },
    );
}

fn insert_header(
    response: &mut axum::http::Response<axum::body::Body>,
    name: &'static str,
    value: &str,
) {
    if let Ok(value) = axum::http::HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

fn rust_gateway_supports_provider(provider: &str) -> bool {
    let transport = providers::transport(provider);
    matches!(
        transport
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("openai"),
        "openai" | "claude" | "gemini" | "openai-responses" | "responses" | "kiro" | "commandcode"
    )
}

impl RouterConfig {
    fn load(state: &AppState) -> Result<Self, AppError> {
        let settings = state.db.settings()?;
        let value = settings
            .get("autoRouter")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let aliases = string_array(value.get("aliases"))
            .filter(|aliases| !aliases.is_empty())
            .unwrap_or_else(|| vec!["auto".into(), "9router-auto".into()]);
        let profiles = value
            .get("profiles")
            .and_then(Value::as_array)
            .filter(|profiles| !profiles.is_empty())
            .map(|profiles| {
                profiles
                    .iter()
                    .filter_map(ModelProfile::from_value)
                    .collect()
            })
            .filter(|profiles: &Vec<ModelProfile>| !profiles.is_empty())
            .unwrap_or_else(default_profiles);

        Ok(Self {
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            aliases,
            profiles,
            max_fallbacks: value
                .get("maxFallbacks")
                .and_then(Value::as_u64)
                .unwrap_or(3)
                .clamp(1, 5) as usize,
            large_context_tokens: value
                .get("largeContextTokens")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LARGE_CONTEXT_TOKENS),
            hard_context_tokens: value
                .get("hardContextTokens")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_HARD_CONTEXT_TOKENS),
            allow_large_context: value
                .get("allowLargeContext")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            cache_affinity: value
                .get("cacheAffinity")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            cache_affinity_ttl_secs: value
                .get("cacheAffinityTtlSecs")
                .and_then(Value::as_u64)
                .unwrap_or(1800)
                .clamp(60, 86_400),
        })
    }
}

impl ModelProfile {
    fn from_value(value: &Value) -> Option<Self> {
        let model = value.get("model")?.as_str()?.trim();
        if model.is_empty() {
            return None;
        }
        Some(Self {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
            model: model.to_string(),
            tier: value
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("standard")
                .to_ascii_lowercase(),
            roles: string_array(value.get("roles")).unwrap_or_default(),
            context_window: value
                .get("contextWindow")
                .and_then(Value::as_u64)
                .unwrap_or(262_144)
                .max(16_384),
            base_score: value.get("baseScore").and_then(Value::as_i64).unwrap_or(50),
            target_share: value
                .get("targetShare")
                .and_then(Value::as_f64)
                .filter(|share| *share > 0.0 && *share <= 1.0),
            // Omitted = unlimited. Explicit zero = disabled for this router.
            daily_token_budget: optional_u64(value.get("dailyTokenBudget")),
            weekly_token_budget: optional_u64(value.get("weeklyTokenBudget")),
            daily_request_budget: optional_u64(value.get("dailyRequestBudget")),
            weekly_request_budget: optional_u64(value.get("weeklyRequestBudget")),
        })
    }
}

fn model_usage(stats: &Value, provider: &str, model: &str) -> ModelUsage {
    let key = format!("{model}|{provider}");
    let entry = stats
        .get("byModel")
        .and_then(Value::as_object)
        .and_then(|models| models.get(&key));
    let prompt = entry
        .and_then(|entry| entry.get("promptTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = entry
        .and_then(|entry| entry.get("completionTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let requests = entry
        .and_then(|entry| entry.get("requests"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    ModelUsage {
        tokens: prompt.saturating_add(completion),
        requests,
    }
}

fn total_tokens(stats: &Value) -> u64 {
    stats
        .get("totalPromptTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            stats
                .get("totalCompletionTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
}

fn budget_exhausted(profile: &ModelProfile, today: ModelUsage, week: ModelUsage) -> bool {
    over_budget(today.tokens, profile.daily_token_budget)
        || over_budget(week.tokens, profile.weekly_token_budget)
        || over_budget(today.requests, profile.daily_request_budget)
        || over_budget(week.requests, profile.weekly_request_budget)
}

fn over_budget(used: u64, budget: Option<u64>) -> bool {
    budget.is_some_and(|budget| used >= budget)
}

fn quota_bias(profile: &ModelProfile, today: ModelUsage, week: ModelUsage) -> i64 {
    quota_pressure(today.tokens, profile.daily_token_budget)
        + quota_pressure(week.tokens, profile.weekly_token_budget)
        + quota_pressure(today.requests, profile.daily_request_budget)
        + quota_pressure(week.requests, profile.weekly_request_budget)
}

fn quota_pressure(used: u64, budget: Option<u64>) -> i64 {
    let Some(budget) = budget else {
        return 0;
    };
    if budget == 0 {
        return -1_000;
    }
    let ratio = used as f64 / budget as f64;
    if ratio >= 0.95 {
        -80
    } else if ratio >= 0.85 {
        -50
    } else if ratio >= 0.70 {
        -30
    } else if ratio >= 0.50 {
        -10
    } else {
        0
    }
}

fn share_bias(profile: &ModelProfile, model_tokens: u64, all_tokens: u64) -> i64 {
    let Some(target_share) = profile.target_share else {
        return 0;
    };
    if all_tokens < 10_000 {
        return 0;
    }
    let share = model_tokens as f64 / all_tokens as f64;
    if share > target_share * 1.75 {
        -30
    } else if share > target_share * 1.35 {
        -15
    } else if share < target_share * 0.65 {
        8
    } else {
        0
    }
}

fn tier_bias(tier: &str) -> i64 {
    match tier {
        "high_volume" | "bulk" => 12,
        "premium" | "limited" => -20,
        "critical" | "scarce" => -38,
        _ => 0,
    }
}

fn tier_eligible(profile: &ModelProfile, risk: u8, complexity: u8) -> bool {
    match profile.tier.as_str() {
        "critical" | "scarce" => risk >= 3 || complexity >= 4,
        "premium" | "limited" => risk >= 2 || complexity >= 3,
        _ => true,
    }
}

fn role_bias(roles: &[String], task: &str) -> i64 {
    if roles.iter().any(|role| role == task) {
        30
    } else if roles.iter().any(|role| role == "general") {
        8
    } else {
        0
    }
}

fn context_bias(profile: &ModelProfile, input_tokens: u64, large_context_tokens: u64) -> i64 {
    let mut score = 0;
    if input_tokens >= large_context_tokens
        && profile.roles.iter().any(|role| role == "large_context")
    {
        score += 24;
    }
    let utilization = input_tokens as f64 / profile.context_window as f64;
    if utilization >= 0.90 {
        score -= 45;
    } else if utilization >= 0.80 {
        score -= 25;
    } else if utilization >= 0.65 {
        score -= 10;
    }
    score
}

fn risk_complexity_bias(profile: &ModelProfile, risk: u8, _complexity: u8) -> i64 {
    match profile.tier.as_str() {
        "critical" | "scarce" => {
            if risk >= 3 {
                60
            } else {
                28
            }
        }
        "premium" | "limited" => 30,
        _ if risk >= 3 => -8,
        _ => 0,
    }
}

fn default_profiles() -> Vec<ModelProfile> {
    vec![
        profile(
            "gemini",
            "ag/gemini-3.8-flash-high",
            "high_volume",
            &[
                "frontend",
                "architecture",
                "debugging",
                "general",
                "large_context",
            ],
            1_048_576,
            76,
            0.45,
        ),
        profile(
            "deepseek",
            "cbcn/deepseek-v4.1-flash",
            "high_volume",
            &[
                "backend",
                "testing",
                "debugging",
                "general",
                "large_context",
            ],
            1_000_000,
            72,
            0.30,
        ),
        // Codex transport context metadata is provider-specific and has historically
        // exposed 272k variants. Stay conservative instead of assuming the public
        // API model's larger context applies to this OAuth transport.
        profile(
            "luna",
            "cx/gpt-5.6-luna",
            "high_volume",
            &["scout", "general"],
            272_000,
            60,
            0.18,
        ),
        profile(
            "sol",
            "cx/gpt-5.6-sol",
            "limited",
            &["architecture", "security", "debugging"],
            272_000,
            47,
            0.05,
        ),
        profile(
            "astra",
            "cx/gpt-6-astra",
            "scarce",
            &["security", "architecture", "debugging"],
            272_000,
            38,
            0.02,
        ),
    ]
}

fn profile(
    name: &str,
    model: &str,
    tier: &str,
    roles: &[&str],
    context_window: u64,
    base_score: i64,
    target_share: f64,
) -> ModelProfile {
    ModelProfile {
        name: name.into(),
        model: model.into(),
        tier: tier.into(),
        roles: roles.iter().map(|role| (*role).into()).collect(),
        context_window,
        base_score,
        target_share: Some(target_share),
        daily_token_budget: None,
        weekly_token_budget: None,
        daily_request_budget: None,
        weekly_request_budget: None,
    }
}

pub(crate) fn estimate_input_tokens(value: &Value) -> u64 {
    let chars = estimate_text_chars(value);
    let structure = estimate_structure_units(value);
    ((chars as f64 / 3.5).ceil() as u64)
        .saturating_add(structure)
        .max(1)
}

fn estimate_text_chars(value: &Value) -> usize {
    match value {
        Value::String(text) if is_inline_data(text) => 0,
        Value::String(text) => weighted_text_units(text),
        Value::Array(values) => values.iter().map(estimate_text_chars).sum(),
        Value::Object(map) if is_image_object(map) => 0,
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| weighted_text_units(key) + estimate_text_chars(value))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn weighted_text_units(text: &str) -> usize {
    text.chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 4 })
        .sum()
}

fn estimate_structure_units(value: &Value) -> u64 {
    match value {
        Value::Array(values) => 2 + values.iter().map(estimate_structure_units).sum::<u64>(),
        Value::Object(map) if is_image_object(map) => IMAGE_TOKEN_ESTIMATE,
        Value::Object(map) => 4 + map.values().map(estimate_structure_units).sum::<u64>(),
        Value::String(text) if is_inline_data(text) => IMAGE_TOKEN_ESTIMATE,
        Value::String(_) => 1,
        Value::Null | Value::Bool(_) | Value::Number(_) => 1,
    }
}

fn is_inline_data(text: &str) -> bool {
    text.starts_with("data:") && text.contains(";base64,")
}

fn is_image_object(map: &Map<String, Value>) -> bool {
    matches!(
        map.get("type").and_then(Value::as_str),
        Some("image" | "image_url" | "input_image")
    ) || map.contains_key("image_url")
        || map
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|source| {
                matches!(
                    source.get("type").and_then(Value::as_str),
                    Some("base64" | "url")
                )
            })
}

fn latest_user_text(canonical: &Value) -> String {
    canonical
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(message_text)
        .unwrap_or_default()
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn classify_task(text: &str) -> String {
    let text = text.to_ascii_lowercase();
    let task = if contains_any(
        &text,
        &[
            "security",
            "oauth",
            "rbac",
            "authorization",
            "authentication",
            "privilege",
            "injection",
            "xss",
            "csrf",
            "secret",
            "cve",
            "tenant escape",
        ],
    ) {
        "security"
    } else if contains_any(
        &text,
        &[
            "architecture",
            "system design",
            "distributed",
            "microservice",
            "design the system",
            "schema design",
            "migration plan",
            "tradeoff",
        ],
    ) {
        "architecture"
    } else if contains_any(
        &text,
        &[
            "bug",
            "debug",
            "error",
            "crash",
            "panic",
            "failing",
            "failed",
            "traceback",
            "root cause",
            "regression",
        ],
    ) {
        "debugging"
    } else if contains_any(
        &text,
        &[
            "test",
            "spec",
            "coverage",
            "cargo test",
            "pytest",
            "jest",
            "playwright",
            "e2e",
        ],
    ) {
        "testing"
    } else if contains_any(
        &text,
        &[
            "frontend",
            "react",
            "next.js",
            "nextjs",
            "svelte",
            "vue",
            "css",
            "tailwind",
            "component",
            "responsive",
            "accessibility",
            "ui",
        ],
    ) {
        "frontend"
    } else if contains_any(
        &text,
        &[
            "backend",
            "api",
            "endpoint",
            "database",
            "sql",
            "postgres",
            "sqlite",
            "rust",
            "service",
            "controller",
            "repository layer",
            "grpc",
        ],
    ) {
        "backend"
    } else if contains_any(
        &text,
        &[
            "summarize",
            "search",
            "find",
            "locate",
            "scan repo",
            "readme",
            "documentation",
            "docs",
            "explain files",
        ],
    ) {
        "scout"
    } else {
        "general"
    };
    task.to_string()
}

fn risk_score(text: &str, task: &str) -> u8 {
    let text = text.to_ascii_lowercase();
    let mut score = u8::from(task == "security") * 2;
    if contains_any(
        &text,
        &[
            "production",
            "billing",
            "payment",
            "delete",
            "drop table",
            "destructive",
            "migration",
            "secret",
            "credential",
            "data loss",
        ],
    ) {
        score = score.saturating_add(1);
    }
    if contains_any(
        &text,
        &[
            "critical",
            "p0",
            "incident",
            "privilege escalation",
            "tenant escape",
        ],
    ) {
        score = score.saturating_add(1);
    }
    score.min(4)
}

fn complexity_score(canonical: &Value, input_tokens: u64, text: &str, task: &str) -> u8 {
    let mut score = 0u8;
    score += u8::from(input_tokens >= 64_000);
    score += u8::from(input_tokens >= 160_000);
    score += u8::from(input_tokens >= 300_000);
    score += u8::from(
        canonical
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| messages.len() >= 20),
    );
    score += u8::from(
        canonical
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools.len() >= 10),
    );
    score += u8::from(matches!(task, "architecture" | "security" | "debugging"));
    score += u8::from(contains_any(
        &text.to_ascii_lowercase(),
        &[
            "multi-file",
            "cross-service",
            "large refactor",
            "concurrency",
            "race condition",
            "distributed",
            "production incident",
            "unknown root cause",
        ],
    ));
    score.min(5)
}

fn affinity_key(canonical: &Value, headers: &HeaderMap) -> Option<String> {
    if let Some(session) = headers
        .get("x-9router-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("session:{}", sha256_hex(session.as_bytes())));
    }

    let mut stable = String::new();
    if let Some(messages) = canonical.get("messages").and_then(Value::as_array) {
        for message in messages.iter().take(4) {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            if matches!(role, "system" | "developer" | "user") {
                stable.push_str(role);
                stable.push('\n');
                stable.push_str(&message_text(message));
                stable.push('\n');
            }
        }
    }
    if let Some(tools) = canonical.get("tools") {
        stable.push_str(&serde_json::to_string(tools).unwrap_or_default());
    }
    if stable.chars().count() < MIN_PREFIX_CHARS_FOR_AFFINITY {
        return None;
    }
    Some(format!("prefix:{}", sha256_hex(stable.as_bytes())))
}

fn cleanup_affinity(ttl_secs: u64) {
    if AFFINITY.len() < 1024 {
        return;
    }
    let ttl = Duration::from_secs(ttl_secs);
    AFFINITY.retain(|_, entry| entry.touched.elapsed() <= ttl);
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.to_ascii_lowercase())
            .collect(),
    )
}

fn optional_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| contains_term(text, needle))
}

fn contains_term(text: &str, needle: &str) -> bool {
    let simple = needle
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.'));
    if !simple {
        return text.contains(needle);
    }
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.')))
        .any(|token| token == needle)
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_full_stack_tasks() {
        assert_eq!(
            classify_task("Build a React dashboard component"),
            "frontend"
        );
        assert_eq!(
            classify_task("Add Rust API endpoint and SQLite query"),
            "backend"
        );
        assert_eq!(classify_task("Add Playwright e2e coverage"), "testing");
        assert_eq!(
            classify_task("Review OAuth RBAC privilege escalation"),
            "security"
        );
        assert_eq!(
            classify_task("Find and summarize the relevant files"),
            "scout"
        );
    }

    #[test]
    fn context_estimator_grows_with_payload() {
        let small = json!({"messages":[{"role":"user","content":"hello"}]});
        let large = json!({"messages":[{"role":"user","content":"x".repeat(35_000)}]});
        assert!(estimate_input_tokens(&large) > estimate_input_tokens(&small));
        assert!(estimate_input_tokens(&large) > 9_000);
    }

    #[test]
    fn quota_pressure_protects_nearly_exhausted_models() {
        assert_eq!(quota_pressure(10, None), 0);
        assert_eq!(quota_pressure(50, Some(100)), -10);
        assert_eq!(quota_pressure(75, Some(100)), -30);
        assert_eq!(quota_pressure(90, Some(100)), -50);
        assert_eq!(quota_pressure(99, Some(100)), -80);
    }

    #[test]
    fn premium_models_are_penalized_for_easy_work_and_recovered_for_risk() {
        let profile = profile(
            "premium",
            "provider/model",
            "limited",
            &["security"],
            1_000_000,
            0,
            0.05,
        );
        assert!(risk_complexity_bias(&profile, 0, 1) > 0);
        assert!(risk_complexity_bias(&profile, 3, 4) > 0);
        assert!(!tier_eligible(&profile, 0, 1));
        assert!(tier_eligible(&profile, 2, 1));
    }

    #[test]
    fn default_config_is_opt_in() {
        let value = json!({});
        assert!(!value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false));
    }

    #[test]
    fn zero_budget_means_disabled_not_unlimited() {
        assert!(over_budget(0, Some(0)));
        assert_eq!(quota_pressure(0, Some(0)), -1_000);
    }

    #[test]
    fn scarce_models_are_not_easy_task_fallbacks() {
        let limited = profile(
            "limited",
            "provider/limited",
            "limited",
            &["general"],
            1_000_000,
            1,
            0.1,
        );
        let scarce = profile(
            "scarce",
            "provider/scarce",
            "scarce",
            &["general"],
            1_000_000,
            1,
            0.1,
        );
        assert!(!tier_eligible(&limited, 0, 1));
        assert!(!tier_eligible(&scarce, 0, 1));
        assert!(tier_eligible(&limited, 2, 1));
        assert!(tier_eligible(&scarce, 3, 1));
    }

    #[test]
    fn base64_images_do_not_look_like_giant_text_prompts() {
        let request = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": format!("data:image/png;base64,{}", "A".repeat(2_000_000))}
                }]
            }]
        });
        assert!(estimate_input_tokens(&request) < 20_000);
    }

    #[test]
    fn classifier_does_not_match_keywords_inside_other_words() {
        assert_eq!(classify_task("Get the latest upstream changes"), "general");
        assert_eq!(classify_task("Debug the Rust API failure"), "debugging");
        assert_eq!(classify_task("Review API implementation"), "backend");
    }

    #[test]
    fn non_ascii_context_estimation_is_conservative() {
        let ascii = json!({"messages":[{"role":"user","content":"a".repeat(10_000)}]});
        let khmer = json!({"messages":[{"role":"user","content":"ក".repeat(10_000)}]});
        assert!(estimate_input_tokens(&khmer) > estimate_input_tokens(&ascii) * 3);
    }

    #[test]
    fn custom_antigravity_transport_is_not_considered_executable() {
        assert!(!rust_gateway_supports_provider("antigravity"));
        assert!(rust_gateway_supports_provider("codebuddy-cn"));
        assert!(rust_gateway_supports_provider("codex"));
    }
}
