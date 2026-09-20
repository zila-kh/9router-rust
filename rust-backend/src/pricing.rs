//! Model rate lookup and cost math for the usage ledger.
//!
//! Ports `getPricingForModel` and `calculateCostFromTokens` from
//! `open-sse/providers/pricing.js` so a request served natively by the Rust
//! gateway lands in `usageHistory` with the same cost the dashboard would have
//! computed for the same tokens — including the cache discounts, which is what
//! makes a cache hit visible as money saved rather than only as a token count.

use serde_json::Value;

use crate::model_capabilities::match_pattern;

/// Built-in rate tables, exported into the provider catalog by
/// `scripts/export-rust-catalog.mjs`. A catalog without them yields no cost,
/// never a wrong one.
fn builtin() -> &'static Value {
    crate::providers::catalog_pricing()
}

/// Rates for one model, following the JS fallback chain:
/// user override -> provider table -> canonical model table -> glob pattern.
fn resolve(user_pricing: Option<&Value>, provider: Option<&str>, model: &str) -> Option<Value> {
    if model.is_empty() {
        return None;
    }
    if let Some(rates) = user_pricing
        .and_then(|user| provider.and_then(|provider| user.get(provider)))
        .and_then(|provider_rates| provider_rates.get(model))
    {
        return Some(rates.clone());
    }

    let tables = builtin();
    if let Some(rates) = provider
        .and_then(|provider| tables.get("provider").and_then(|t| t.get(provider)))
        .and_then(|provider_rates| provider_rates.get(model))
    {
        return Some(rates.clone());
    }

    // Vendor-prefixed ids ("anthropic/claude-sonnet-4.5") price as the bare model.
    let base_model = model.rsplit('/').next().unwrap_or(model);
    let model_table = tables.get("model");
    for candidate in [base_model, model] {
        if let Some(rates) = model_table.and_then(|t| t.get(candidate)) {
            return Some(rates.clone());
        }
    }

    tables
        .get("pattern")
        .and_then(Value::as_array)
        .and_then(|patterns| {
            patterns.iter().find(|entry| {
                let pattern = entry.get("pattern").and_then(Value::as_str).unwrap_or("");
                !pattern.is_empty()
                    && (match_pattern(pattern, base_model) || match_pattern(pattern, model))
            })
        })
        .and_then(|entry| entry.get("pricing").cloned())
}

/// Cost in USD for a canonical token record. `prompt_tokens` is cache-inclusive
/// (see `translate::stored_tokens`), so the cache subsets are subtracted before
/// the full input rate is applied and then billed at their own rates; a missing
/// cache rate falls back to the base input rate, exactly as the JS does.
pub fn cost_from_tokens(tokens: &Value, rates: &Value) -> f64 {
    let rate = |key: &str| rates.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let count = |key: &str| tokens.get(key).and_then(Value::as_f64).unwrap_or(0.0);

    let input_rate = rate("input");
    if input_rate == 0.0 {
        return 0.0;
    }
    let output_rate = {
        let value = rate("output");
        if value == 0.0 {
            input_rate
        } else {
            value
        }
    };
    let pick = |specific: &str, fallback: f64| {
        let value = rate(specific);
        if value == 0.0 {
            fallback
        } else {
            value
        }
    };

    let prompt = count("prompt_tokens").max(count("input_tokens"));
    let cached = count("cached_tokens").max(count("cache_read_input_tokens"));
    let cache_creation = count("cache_creation_input_tokens");
    let completion = count("completion_tokens").max(count("output_tokens"));
    let reasoning = count("reasoning_tokens");

    let uncached = (prompt - cached - cache_creation).max(0.0);
    let mut cost = uncached * input_rate;
    if cached > 0.0 {
        cost += cached * pick("cached", input_rate);
    }
    cost += completion * output_rate;
    if reasoning > 0.0 {
        cost += reasoning * pick("reasoning", output_rate);
    }
    if cache_creation > 0.0 {
        cost += cache_creation * pick("cache_creation", input_rate);
    }
    cost / 1_000_000.0
}

/// Convenience wrapper for the ledger: 0.0 whenever no rate is known.
pub fn cost_for(user_pricing: Option<&Value>, provider: &str, model: &str, tokens: &Value) -> f64 {
    resolve(user_pricing, Some(provider), model)
        .map(|rates| cost_from_tokens(tokens, &rates))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tokens() -> Value {
        json!({
            "prompt_tokens": 120_000,
            "completion_tokens": 1_000,
            "total_tokens": 121_000,
            "cached_tokens": 100_000,
            "cache_creation_input_tokens": 10_000,
            "reasoning_tokens": 500,
        })
    }

    #[test]
    fn canonical_model_table_is_found_for_bare_and_prefixed_ids() {
        let rates = resolve(None, Some("claude"), "claude-sonnet-4.5").unwrap();
        assert_eq!(rates["input"].as_f64(), Some(3.0));
        let prefixed = resolve(None, Some("tokenrouter"), "anthropic/claude-sonnet-4.5").unwrap();
        assert_eq!(prefixed["cached"].as_f64(), Some(0.3));
    }

    #[test]
    fn pattern_table_covers_unlisted_models() {
        let rates = resolve(None, Some("codex"), "gpt-5.1-codex-max").unwrap();
        assert_eq!(rates["input"].as_f64(), Some(8.0));
    }

    #[test]
    fn user_pricing_wins_over_builtin() {
        let user =
            json!({"claude": {"claude-sonnet-4.5": {"input": 1.0, "output": 2.0, "cached": 0.1}}});
        let rates = resolve(Some(&user), Some("claude"), "claude-sonnet-4.5").unwrap();
        assert_eq!(rates["input"].as_f64(), Some(1.0));
        // A user entry for another provider must not leak into this one.
        let other = resolve(Some(&user), Some("openai"), "claude-sonnet-4.5").unwrap();
        assert_eq!(other["input"].as_f64(), Some(3.0));
    }

    #[test]
    fn cache_reads_bill_at_the_cache_rate_not_the_input_rate() {
        let rates = json!({"input": 3.0, "output": 15.0, "cached": 0.3, "cache_creation": 3.75, "reasoning": 15.0});
        // 10k uncached * 3 + 100k cached * 0.3 + 10k creation * 3.75 + 1000 output * 15
        // + 500 reasoning * 15, all per million.
        let expected =
            (10_000.0 * 3.0 + 100_000.0 * 0.3 + 10_000.0 * 3.75 + 1_000.0 * 15.0 + 500.0 * 15.0)
                / 1_000_000.0;
        let cost = cost_from_tokens(&tokens(), &rates);
        assert!((cost - expected).abs() < 1e-12, "{cost} != {expected}");
    }

    #[test]
    fn a_full_cache_miss_costs_the_full_input_rate() {
        let rates = json!({"input": 3.0, "output": 15.0, "cached": 0.3});
        let miss =
            json!({"prompt_tokens": 100_000, "completion_tokens": 0, "total_tokens": 100_000});
        let cost = cost_from_tokens(&miss, &rates);
        assert!((cost - 0.3).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn unknown_models_and_missing_tables_cost_nothing() {
        assert!(resolve(None, Some("nope"), "not-a-real-model-xyz").is_none());
        assert_eq!(
            cost_for(None, "nope", "not-a-real-model-xyz", &tokens()),
            0.0
        );
    }
}
