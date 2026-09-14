from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text()
    actual = text.count(old)
    if actual < count:
        raise SystemExit(
            f"{path}: expected at least {count} occurrence(s), found {actual}: {old[:100]!r}"
        )
    p.write_text(text.replace(old, new, count))


replace(
    "rust-backend/src/providers.rs",
    "pub fn media_config(id: &str, kind: &str) -> Value {",
    '''pub fn rust_gateway_supports_provider(id: &str) -> bool {
    let transport = transport(id);
    matches!(
        transport
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("openai"),
        "openai"
            | "claude"
            | "gemini"
            | "openai-responses"
            | "responses"
            | "kiro"
            | "commandcode"
    )
}

pub fn media_config(id: &str, kind: &str) -> Value {''',
)

replace(
    "rust-backend/src/auto_router.rs",
    "use std::time::{Duration, Instant};",
    "use std::{collections::HashSet, time::{Duration, Instant}};",
)
replace(
    "rust-backend/src/auto_router.rs",
    '    let today = state.db.usage_stats("today")?;\n    let week = state.db.usage_stats("7d")?;',
    '    let today = state.db.usage_route_stats("today")?;\n    let week = state.db.usage_route_stats("7d")?;',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''    let today_total = total_tokens(&today);
    let mut candidates = Vec::new();

    for profile in config.profiles.iter().cloned() {
        if estimated_input_tokens > profile.context_window.saturating_mul(95) / 100 {''',
    '''    let today_total = total_tokens(&today);
    let mut candidates = Vec::new();
    let mut seen_routes = HashSet::new();

    for profile in config.profiles.iter().cloned() {
        if !tier_eligible(&profile, risk, complexity) {
            continue;
        }
        if estimated_input_tokens > profile.context_window.saturating_mul(95) / 100 {''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''        let resolved = match providers::resolve_model(state, &profile.model) {
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

        let today_model = model_usage(&today, &resolved.provider, &resolved.model);''',
    '''        let resolved = match providers::resolve_model(state, &profile.model) {
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
        if !providers::rust_gateway_supports_provider(&resolved.provider) {
            tracing::debug!(
                model = %profile.model,
                provider = %resolved.provider,
                "auto-router candidate skipped because its transport is not implemented by the Rust gateway"
            );
            continue;
        }
        let route_key = format!("{}\\0{}", resolved.provider, resolved.model);
        if !seen_routes.insert(route_key) {
            tracing::debug!(model = %profile.model, "auto-router duplicate route skipped");
            continue;
        }

        let today_model = model_usage(&today, &resolved.provider, &resolved.model);''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''    let targets = candidates
        .iter()
        .take(config.max_fallbacks.min(candidates.len()))''',
    '''    let affinity_hit = affinity_model.as_ref().is_some_and(|model| {
        candidates
            .iter()
            .any(|candidate| candidate.profile.model == *model)
    });
    let targets = candidates
        .iter()
        .take(config.max_fallbacks.min(candidates.len()))''',
)
replace(
    "rust-backend/src/auto_router.rs",
    "        affinity_hit: affinity_model.is_some(),",
    "        affinity_hit,",
)
replace(
    "rust-backend/src/auto_router.rs",
    '''            daily_token_budget: positive_u64(value.get("dailyTokenBudget")),
            weekly_token_budget: positive_u64(value.get("weeklyTokenBudget")),
            daily_request_budget: positive_u64(value.get("dailyRequestBudget")),
            weekly_request_budget: positive_u64(value.get("weeklyRequestBudget")),''',
    '''            daily_token_budget: optional_u64(value.get("dailyTokenBudget")),
            weekly_token_budget: optional_u64(value.get("weeklyTokenBudget")),
            daily_request_budget: optional_u64(value.get("dailyRequestBudget")),
            weekly_request_budget: optional_u64(value.get("weeklyRequestBudget")),''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''fn quota_pressure(used: u64, budget: Option<u64>) -> i64 {
    let Some(budget) = budget else {
        return 0;
    };
    let ratio = used as f64 / budget as f64;''',
    '''fn quota_pressure(used: u64, budget: Option<u64>) -> i64 {
    let Some(budget) = budget else {
        return 0;
    };
    if budget == 0 {
        return -1_000;
    }
    let ratio = used as f64 / budget as f64;''',
)
replace(
    "rust-backend/src/auto_router.rs",
    "fn default_profiles() -> Vec<ModelProfile> {",
    '''fn tier_eligible(profile: &ModelProfile, risk: u8, complexity: u8) -> bool {
    match profile.tier.as_str() {
        "critical" | "scarce" => risk >= 3 || complexity >= 4,
        "premium" | "limited" => risk >= 2 || complexity >= 3,
        _ => true,
    }
}

fn default_profiles() -> Vec<ModelProfile> {''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''fn estimate_input_tokens(value: &Value) -> u64 {
    let chars = estimate_text_chars(value);
    let structure = estimate_structure_units(value);
    ((chars as f64 / 3.5).ceil() as u64)
        .saturating_add(structure)
        .max(1)
}

fn estimate_text_chars(value: &Value) -> usize {
    match value {
        Value::String(text) => text.chars().count(),
        Value::Array(values) => values.iter().map(estimate_text_chars).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| key.chars().count() + estimate_text_chars(value))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn estimate_structure_units(value: &Value) -> u64 {
    match value {
        Value::Array(values) => 2 + values.iter().map(estimate_structure_units).sum::<u64>(),
        Value::Object(map) => 4 + map.values().map(estimate_structure_units).sum::<u64>(),
        Value::String(_) => 1,
        Value::Null | Value::Bool(_) | Value::Number(_) => 1,
    }
}''',
    '''pub(crate) fn estimate_input_tokens(value: &Value) -> u64 {
    let chars = estimate_text_chars(value);
    let structure = estimate_structure_units(value);
    ((chars as f64 / 3.5).ceil() as u64)
        .saturating_add(structure)
        .max(1)
}

fn estimate_text_chars(value: &Value) -> usize {
    match value {
        Value::String(text) if is_inline_data(text) => 0,
        Value::String(text) => text.chars().count(),
        Value::Array(values) => values.iter().map(estimate_text_chars).sum(),
        Value::Object(map) if is_image_object(map) => 0,
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| key.chars().count() + estimate_text_chars(value))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn estimate_structure_units(value: &Value) -> u64 {
    match value {
        Value::Array(values) => 2 + values.iter().map(estimate_structure_units).sum::<u64>(),
        Value::Object(map) if is_image_object(map) => 4_096,
        Value::Object(map) => 4 + map.values().map(estimate_structure_units).sum::<u64>(),
        Value::String(text) if is_inline_data(text) => 4_096,
        Value::String(_) => 1,
        Value::Null | Value::Bool(_) | Value::Number(_) => 1,
    }
}

fn is_inline_data(text: &str) -> bool {
    text.starts_with("data:") && text.contains(";base64,")
}

fn is_image_object(map: &serde_json::Map<String, Value>) -> bool {
    matches!(
        map.get("type").and_then(Value::as_str),
        Some("image" | "image_url" | "input_image")
    ) || map.contains_key("image_url")
        || map
            .get("source")
            .and_then(Value::as_object)
            .is_some_and(|source| {
                matches!(source.get("type").and_then(Value::as_str), Some("base64" | "url"))
            })
}''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''fn positive_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}''',
    '''fn optional_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}''',
)
replace(
    "rust-backend/src/auto_router.rs",
    '''    #[test]
    fn default_config_is_opt_in() {
        let value = json!({});
        assert!(!value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false));
    }
}''',
    '''    #[test]
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
            "limited", "p/limited", "limited", &["general"], 1_000_000, 1, 0.1,
        );
        let scarce = profile(
            "scarce", "p/scarce", "scarce", &["general"], 1_000_000, 1, 0.1,
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
}''',
)

replace(
    "rust-backend/src/db.rs",
    "CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId);",
    "CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId);\nCREATE INDEX IF NOT EXISTS idx_uh_ts_provider_model ON usageHistory(timestamp, provider, model);",
)
replace(
    "rust-backend/src/db.rs",
    "    pub fn usage_stats(&self, period: &str) -> Result<Value, AppError> {",
    '''    pub fn usage_route_stats(&self, period: &str) -> Result<Value, AppError> {
        let cutoff = usage_cutoff(period)?;
        self.with_conn(|db| {
            let sql = if cutoff.is_some() {
                "SELECT provider,model,COUNT(*),COALESCE(SUM(promptTokens),0),COALESCE(SUM(completionTokens),0) FROM usageHistory WHERE timestamp>=?1 GROUP BY provider,model"
            } else {
                "SELECT provider,model,COUNT(*),COALESCE(SUM(promptTokens),0),COALESCE(SUM(completionTokens),0) FROM usageHistory GROUP BY provider,model"
            };
            let mut stmt = db.prepare(sql)?;
            let mut rows = if let Some(cutoff) = cutoff.as_deref() {
                stmt.query(params![cutoff])?
            } else {
                stmt.query([])?
            };
            let mut by_model = Map::new();
            let mut total_prompt = 0i64;
            let mut total_completion = 0i64;
            while let Some(row) = rows.next()? {
                let provider: Option<String> = row.get(0)?;
                let model: Option<String> = row.get(1)?;
                let requests: i64 = row.get(2)?;
                let prompt: i64 = row.get(3)?;
                let completion: i64 = row.get(4)?;
                total_prompt += prompt;
                total_completion += completion;
                let provider = provider.unwrap_or_default();
                let model = model.unwrap_or_default();
                let key = if provider.is_empty() {
                    model.clone()
                } else {
                    format!("{model}|{provider}")
                };
                by_model.insert(
                    key,
                    json!({
                        "requests": requests,
                        "promptTokens": prompt,
                        "completionTokens": completion,
                        "rawModel": model,
                        "provider": provider
                    }),
                );
            }
            Ok(json!({
                "totalPromptTokens": total_prompt,
                "totalCompletionTokens": total_completion,
                "byModel": by_model
            }))
        })
    }

    pub fn usage_stats(&self, period: &str) -> Result<Value, AppError> {''',
)

replace(
    "rust-backend/src/providers.rs",
    "    use super::{resolve_env_placeholders_with, resolve_model};",
    "    use super::{resolve_env_placeholders_with, resolve_model, rust_gateway_supports_provider};",
)
replace(
    "rust-backend/src/providers.rs",
    "    fn test_state() -> (tempfile::TempDir, AppState) {",
    '''    #[test]
    fn native_gateway_support_filter_rejects_unimplemented_custom_transports() {
        assert!(!rust_gateway_supports_provider("antigravity"));
        assert!(rust_gateway_supports_provider("codebuddy-cn"));
        assert!(rust_gateway_supports_provider("codex"));
    }

    fn test_state() -> (tempfile::TempDir, AppState) {''',
)
