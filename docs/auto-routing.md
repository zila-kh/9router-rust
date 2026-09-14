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

If a real combo already exists with the same name, the combo wins. Explicit model requests also continue through the original routing path.

## Fail-safe routing policy

The built-in profiles are designed for an account where high-volume models should absorb normal full-stack work while GPT-5.6 Sol and GPT-6 Astra remain protected capacity.

- `cbcn/deepseek-v4.1-flash`: high-volume backend, testing, debugging, general and large-context work.
- `cx/gpt-5.6-luna`: high-volume scouting and general work.
- `cx/gpt-5.6-sol`: limited escalation for sufficiently risky or complex architecture/security/debugging work.
- `cx/gpt-6-astra`: scarce escalation for only the highest-risk or highest-complexity work.
- `ag/gemini-3.8-flash-high`: profile is retained for the intended high-volume policy, but the native Rust gateway currently skips it because Antigravity requires a dedicated executor that has not yet been ported.

A profile must resolve to a model **and** use a transport executable by the native Rust gateway. Unsupported custom transports are excluded before scoring rather than being selected and failing at dispatch.

If Gemini should participate today, configure a Gemini profile that resolves through a Rust-supported transport. Otherwise port the upstream Antigravity executor before relying on the built-in `ag/...` profile.

### Escalation gates

Tier penalties alone are not used to protect limited models. They are hard eligibility gates:

- `high_volume`, `bulk`, and normal tiers can handle ordinary work.
- `limited` / `premium` models require risk >= 2 or complexity >= 3.
- `scarce` / `critical` models require risk >= 3 or complexity >= 4.

This prevents Sol/Astra from becoming accidental fallbacks merely because a cheap provider is offline.

`maxFallbacks` is always a hard upper bound. Duplicate aliases that resolve to the same provider/model are collapsed before fallback slots are assigned.

## Fallback safety

Auto routing distinguishes local failures from provider-specific failures.

- Local bad requests, gateway authentication failures, local policy failures, and internal errors stop immediately. Trying another paid model cannot repair them.
- Provider network failures and provider-specific HTTP failures such as expired provider credentials, quota/capacity errors, missing provider models, or context/provider limitations may continue to the next planned candidate.
- A provider HTTP `400` is treated as a request-level failure and stops auto fallback to avoid repeating a clearly rejected request across multiple models.

These rules apply only to the optional auto route. Existing explicit/combo behavior is unchanged.

## Context and token behavior

The router estimates prompt size on the canonical request before dispatch. This is tokenizer-independent routing telemetry, not a replacement for provider-reported usage.

- `largeContextTokens` defaults to `160000`.
- `hardContextTokens` defaults to `850000`.
- Requests above the hard threshold are rejected **only for the auto virtual model** unless `allowLargeContext` is enabled or `x-9router-allow-large-context: true` is supplied.
- Explicit model requests bypass the auto-router context guard.
- Inline/base64 image payload bytes are not counted as ordinary text. Image-like inputs receive a bounded conservative token estimate instead of making a screenshot appear to be hundreds of thousands of text tokens.
- Built-in Codex-provider profiles for Luna, Sol and Astra currently use a conservative `272000` context-window assumption. Override `contextWindow` only when the actual transport/account capability is known.

Provider prompt caching does **not** reduce context-window occupancy, so cache reuse is never treated as context compression.

## Streaming usage accounting

Native pass-through streams return to the client before 9Router can inspect their final provider usage event. Without special handling, those requests would appear as zero usage and quota-aware routing would drift.

For **auto-routed direct pass-through streams only**, 9Router records the pre-dispatch estimated input tokens with status `stream_estimate`. Completion tokens remain `0` for that estimate. Buffered/translated responses continue to use provider-reported usage through the existing path.

This keeps quota pressure conservative without changing explicit-model streaming behavior. Exact provider cache/output telemetry can be added later when each streaming protocol exposes a reliable final usage event.

## Cache affinity

`cacheAffinity` defaults to `true`. The router remembers the successful model for a stable request prefix and prefers that model on related follow-up calls. This reduces unnecessary provider switching and improves the chance of provider-side prefix-cache reuse.

For reliable affinity across a coding-agent session, send:

```http
x-9router-session-id: project-or-agent-session-id
```

The value is hashed before being stored in process memory. Affinity is best-effort, in-memory only, and expires after `cacheAffinityTtlSecs` (default `1800`). It is not conversation storage and it is not a response cache.

## Quota-aware profiles

You can replace the defaults and set per-model daily/weekly limits. Omitted limits mean unlimited for this feature. An explicit value of `0` disables that profile for auto routing.

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
        "name": "deepseek",
        "model": "cbcn/deepseek-v4.1-flash",
        "tier": "high_volume",
        "roles": ["backend", "testing", "debugging", "general", "large_context"],
        "contextWindow": 1000000,
        "baseScore": 72,
        "targetShare": 0.50
      },
      {
        "name": "luna",
        "model": "cx/gpt-5.6-luna",
        "tier": "high_volume",
        "roles": ["scout", "general"],
        "contextWindow": 272000,
        "baseScore": 60,
        "targetShare": 0.35
      },
      {
        "name": "sol",
        "model": "cx/gpt-5.6-sol",
        "tier": "limited",
        "roles": ["architecture", "security", "debugging"],
        "contextWindow": 272000,
        "baseScore": 47,
        "targetShare": 0.10,
        "dailyTokenBudget": 250000,
        "weeklyTokenBudget": 1000000
      },
      {
        "name": "astra",
        "model": "cx/gpt-6-astra",
        "tier": "scarce",
        "roles": ["security", "architecture", "debugging"],
        "contextWindow": 272000,
        "baseScore": 38,
        "targetShare": 0.05,
        "dailyTokenBudget": 100000,
        "weeklyTokenBudget": 400000
      }
    ]
  }
}
```

Supported limits:

- `dailyTokenBudget`
- `weeklyTokenBudget`
- `dailyRequestBudget`
- `weeklyRequestBudget`

The router uses recorded prompt + completion usage for budget pressure. A configured profile is excluded after a hard budget is exhausted and receives progressively stronger penalties as it approaches a budget.

`weeklyTokenBudget` currently uses the existing rolling `7d` usage window rather than a calendar-week reset. `today` follows the backend host's local-day boundary.

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

These headers appear only when the optional auto router actually handled the request.

## Disable / rollback

Set:

```json
{
  "autoRouter": {
    "enabled": false
  }
}
```

or stop requesting `auto`. Existing explicit/combo routing remains available at all times.
