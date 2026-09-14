# Optional context-aware auto routing

The Rust backend includes an optional virtual-model router for large full-stack workloads. It is **disabled by default** and does not change existing explicit model, model alias, combo, provider-priority, or fallback behavior.

## Enable

Update normal 9Router settings with an `autoRouter` object:

```json
{
  "autoRouter": {
    "enabled": true
  }
}
```

Then request the virtual model `auto` (or `9router-auto`):

```json
{
  "model": "auto",
  "messages": [
    {"role": "user", "content": "Implement this backend feature and its tests"}
  ]
}
```

If a real combo already exists with the same name, the combo wins. This preserves compatibility with existing installations.

## Default routing policy

The built-in profiles are designed for an account where Gemini 3.8 Flash, DeepSeek V4.1 Flash, and GPT-5.6 Luna have higher available volume while GPT-5.6 Sol and GPT-6 Astra are scarce.

Defaults:

- `ag/gemini-3.8-flash-high`: frontend, architecture drafts, debugging, general and large-context work.
- `cbcn/deepseek-v4.1-flash`: backend, tests, debugging, general and large-context work.
- `cx/gpt-5.6-luna`: scouting and general work.
- `cx/gpt-5.6-sol`: limited escalation for architecture, security and debugging.
- `cx/gpt-6-astra`: scarce escalation for security, architecture and difficult debugging.

Only profiles whose provider/model can currently resolve through an active connection are considered.

## Context and token behavior

The router estimates prompt size on the canonical request before dispatch. This is a tokenizer-independent conservative estimate used for routing and guardrails; provider-reported usage remains the source of truth after the call.

- `largeContextTokens` defaults to `160000`. Models marked for large-context work receive a routing bonus above this threshold.
- `hardContextTokens` defaults to `850000`.
- Requests above the hard threshold are rejected **only when using the auto virtual model** unless `allowLargeContext` is enabled or the request includes `x-9router-allow-large-context: true`.
- Explicit model requests bypass the auto-router context guard completely.

Provider prompt caching does not reduce context-window occupancy. The router therefore does not treat cache reuse as context compression.

## Cache affinity

`cacheAffinity` defaults to `true`. The router remembers the successful model for a stable request prefix and prefers that model on related follow-up calls. This reduces unnecessary provider switching and improves the chance of provider-side prefix-cache reuse.

For reliable affinity across a coding-agent session, send:

```http
x-9router-session-id: project-or-agent-session-id
```

The value is hashed before being stored in process memory. Affinity is best-effort, in-memory only, and expires after `cacheAffinityTtlSecs` (default: 1800 seconds). It is not conversation storage and it is not a response cache.

## Quota-aware profiles

You can replace the default profiles and set daily/weekly limits. Limits are optional; omitted limits are treated as unlimited by this feature.

```json
{
  "autoRouter": {
    "enabled": true,
    "aliases": ["auto", "9router-auto"],
    "maxFallbacks": 3,
    "largeContextTokens": 160000,
    "hardContextTokens": 850000,
    "allowLargeContext": false,
    "cacheAffinity": true,
    "cacheAffinityTtlSecs": 1800,
    "profiles": [
      {
        "name": "gemini",
        "model": "ag/gemini-3.8-flash-high",
        "tier": "high_volume",
        "roles": ["frontend", "architecture", "debugging", "general", "large_context"],
        "contextWindow": 1048576,
        "baseScore": 76,
        "targetShare": 0.45,
        "dailyTokenBudget": 5000000,
        "weeklyTokenBudget": 25000000
      },
      {
        "name": "deepseek",
        "model": "cbcn/deepseek-v4.1-flash",
        "tier": "high_volume",
        "roles": ["backend", "testing", "debugging", "general", "large_context"],
        "contextWindow": 1000000,
        "baseScore": 72,
        "targetShare": 0.30
      },
      {
        "name": "luna",
        "model": "cx/gpt-5.6-luna",
        "tier": "high_volume",
        "roles": ["scout", "general"],
        "contextWindow": 1050000,
        "baseScore": 60,
        "targetShare": 0.18
      },
      {
        "name": "sol",
        "model": "cx/gpt-5.6-sol",
        "tier": "limited",
        "roles": ["architecture", "security", "debugging"],
        "contextWindow": 1050000,
        "baseScore": 47,
        "targetShare": 0.05,
        "dailyTokenBudget": 250000,
        "weeklyTokenBudget": 1000000
      },
      {
        "name": "astra",
        "model": "cx/gpt-6-astra",
        "tier": "scarce",
        "roles": ["security", "architecture", "debugging"],
        "contextWindow": 1050000,
        "baseScore": 38,
        "targetShare": 0.02,
        "dailyTokenBudget": 100000,
        "weeklyTokenBudget": 400000
      }
    ]
  }
}
```

Supported per-profile limits:

- `dailyTokenBudget`
- `weeklyTokenBudget`
- `dailyRequestBudget`
- `weeklyRequestBudget`

The current implementation uses 9Router's recorded prompt + completion usage for budget pressure. A model is excluded after a configured hard budget is exhausted, and receives progressively larger score penalties as it approaches a budget.

`weeklyTokenBudget` currently uses the existing rolling `7d` usage window rather than a calendar-week reset.

## Response diagnostics

Auto-routed responses include:

- `x-9router-auto-route: 1`
- `x-9router-selected-model`
- `x-9router-router-version`
- `x-9router-task`
- `x-9router-estimated-input-tokens`
- `x-9router-risk`
- `x-9router-complexity`
- `x-9router-cache-affinity: hit|miss`

These headers are only added for requests that actually used the optional auto router.

## Disable / rollback

Set:

```json
{
  "autoRouter": {
    "enabled": false
  }
}
```

or stop requesting the virtual `auto` model. Existing routing remains available at all times.
