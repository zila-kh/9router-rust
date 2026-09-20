# Built-in free tier (`combo-free`) plan

**Status:** Implemented; the anonymous-member live leg passes, the keyed-member
and operator legs remain pending
**Created:** 2026-09-20

## Problem and goal

A fresh install offers nothing until the operator creates provider accounts, and
free tiers that operators do add die silently (models deprecated, keys rotated,
limits changed). The goal is a built-in, on-by-default free tier:

- works with zero configuration on a fresh install;
- grows automatically as the operator adds free-tier keys that pass testing;
- is curated **online** so dead providers are dropped by registry update rather
  than a local fix or a release;
- is **admin-managed**: API-key consumers see one opaque model, `combo-free`,
  and cannot list or call the member models directly.

## Design

### 1. Online free-registry (global core, online-managed)

A versioned JSON file, `free-registry.json`, served from this repository
(raw/CDN URL pinned in the binary), PR-reviewed like the release manifest.
Schema:

```json
{
  "version": 1,
  "updated": "2026-09-20T00:00:00Z",
  "providers": [
    {
      "id": "kilo",
      "class": "api",
      "access": "anonymous",
      "name": "Kilo Free Gateway",
      "baseUrl": "https://api.kilo.ai/api/gateway",
      "models": ["kilo-auto/free"],
      "limits": { "requestsPerHourPerIp": 200 },
      "status": "active"
    },
    {
      "id": "groq",
      "class": "api",
      "access": "key",
      "provider": "groq",
      "signupUrl": "https://console.groq.com/keys",
      "limits": { "rpm": 30, "rpd": 250 },
      "status": "active"
    }
  ]
}
```

- `access: "anonymous"` entries become **virtual connections** computed from the
  registry at request time; they are never written to the database, so sync
  updates apply immediately and nothing goes stale.
- `access: "key"` entries surface as one-click adds (seeded inactive, awaiting
  key) on the Providers page.
- `status: "deprecated"` entries drop out of the pool on next sync.
- Sync reuses the existing model-catalog sync pattern (periodic + on-demand),
  cached at `<data>/free-registry.json`, with a **bundled fallback copy** in the
  binary so offline installs keep working. Fetched through the existing SSRF
  guard; the URL is pinned to this repository. The registry is configuration,
  not code, and carries no secrets.

### 2. Membership: two zones

- **Global core** (from the registry): definitions are read-only locally. The
  admin cannot edit a core entry — only exclude it or switch the tier off.
- **Local additions** (add-only): the admin can contribute extra free providers
  as normal `providerConnections` rows tagged `freePool: true` in
  `providerSpecificData`, created through the existing providers API. They can
  delete their own additions; core definitions remain immutable.
- A member contributes to the pool only while it **passes its test** and is
  active. Anonymous built-ins are exempt from the test (no key to test) but
  covered by health cooldowns.

### 3. Visibility rules

`combo-free` is the entitlement of a registered consumer. "Registered" means
holding an API key: this port has no user-account or self-signup system, so the
key is the whole registration boundary, and the entitlement seam noted at the
end of this document is per-key plan data rather than a separate user table.

- The tier is **never public**. With the shipped defaults (`requireLogin` and
  `requireApiKey` both true, see `db.rs`), an unauthenticated request to
  `/v1/models`, `/v1/models/info`, or `/v1/chat/completions` is rejected with
  `401` before any routing happens, including from loopback. Relaxing that is an
  explicit operator choice: `requireApiKey: false` is what admits a keyless
  loopback caller, and it is not the shipped default.
- API-key consumers (`/v1/models`, `/v1/models/info`, model resolution):
  see exactly one model, **`combo-free`**. Member models are hidden; a request
  naming a member model directly (e.g. `groq/llama-…`) does not resolve for
  consumers.
- An API key *is* a consumer, so it also sees only `combo-free` — including a key
  the operator creates for themselves. `api_key_consumer` treats a request as a
  consumer unless it also carries a valid CLI token or dashboard session, so
  those two surfaces keep full visibility while a key does not.
- A connection that is not a tier member is not addressable by any consumer key
  while the tier is on, and the expose toggle does not change that:
  `is_exposed_provider` returns false before consulting it when the provider is
  neither a registry member nor a local pool addition. The remedies are the tier
  switch — `builtinFreeCombo: false`, which drops `combo-free` from `/v1/models`
  and from routing and restores direct addressing — or making the connection a
  pool member, after which expose-directly can make it individually addressable.
- Membership is not a shortcut for a private upstream: `POST /api/free-tier/members`
  requires an `openai-compatible-*` addition to carry a credential-free public
  HTTPS `baseUrl` and passes it through the SSRF guard, so a loopback or private
  upstream cannot be contributed at all and the tier switch is its only route.
  Membership also cuts the other way — a contributed connection becomes a pool
  member that every consumer draws on.
- Admin (dashboard session / CLI token): full member visibility through the
  management API and the Providers page.
- Escape hatch: a member connection can be marked **also expose directly**
  (default off) to make its models individually addressable outside the pool.
- `combo-free` is a **computed combo**, never a row in the user `combos`
  table — nothing to clobber, absent from combo CRUD.

### 4. Admin management model

| Member kind | Managed by | Admin local actions |
|---|---|---|
| Global core (registry) | online registry sync | exclude member; expose-directly; tier switch |
| Anonymous built-ins | registry (virtual, not in DB) | none needed; covered by cooldowns |
| Keyed registry entries | admin (one-click add + key) | add / test / activate / remove |
| Local additions | admin (add-only) | add / remove own; never edit core |

Admin UI: the Providers page gains two tabs — **Providers** (today's page,
unchanged) and **Global Free Tier** (admin-only). The free-tier tab reuses the
existing connection-card grid and filters, bound to the merged `/api/free-tier`
view: registry-owned core members render as locked cards (definitions
read-only; only exclude/expose toggles live), keyed members and local
additions render as normal cards, and a slim header strip above the grid holds
the master toggle, sync status, and the add flows (one-click keyed seed,
custom free provider).

Admin API: a dedicated, session/CLI-token-gated namespace; handlers stay thin
and delegate to the same core as the provider routes.

```
GET    /api/free-tier              merged view: registry + local members + health + sync status
POST   /api/free-tier/sync         refresh the registry now
GET    /api/free-tier/registry     raw cached registry (debug)
POST   /api/free-tier/members      add a local addition / one-click keyed seed
PATCH  /api/free-tier/members/{id} exclude-from-pool / expose-directly toggles
DELETE /api/free-tier/members/{id} remove a local addition the admin owns
```

The master switch remains `builtinFreeCombo` on the existing protected
`PUT /api/settings`.

### 5. Routing and reliability

- Request-time assembly over healthy members: anonymous virtual connections +
  active free-tagged connections, ordered by priority and health.
- v1 reliability: in-memory per-member cooldown (10 minutes) after a failure;
  no automatic retries of video/media-style creations; no retry after response
  bytes are committed (existing streaming rule).
- Consumers share the instance's free quotas; the registry limits are displayed
  on the admin card as the ceiling.

### 6. The switch

`builtinFreeCombo` setting, default **true**, togglable only via the protected
settings API. When **off**: `combo-free` is filtered from `/v1/models`,
`/v1/models/info`, and never selected for routing — members configured as
expose-directly remain available as themselves.

## Implementation slices

1. `builtinFreeCombo` setting + computed combo assembly + `/v1/models` and
   `/v1/models/info` filtering for API-key requests.
2. Registry schema + sync + bundled fallback + deprecation handling + virtual
   anonymous connections (Kilo Auto Free in the bundled seed). LLM7 requires an
   API token, while OVHcloud's documented chat models are paid and its examples
   use an access token; add either only as a keyed/custom member after checking
   its current free eligibility and terms.
3. Providers page tabs: Global Free Tier tab (admin-only) reusing the existing
   card grid with locked core cards, header strip for toggle/sync/add flows.
4. Per-member exclusion + expose-directly toggles; in-memory cooldown.

## Verification and acceptance

- Unit tests: models filtering (consumer vs admin surface), pool assembly with
  mixed member states (inactive, failed-test, excluded, deprecated), switch-off
  behavior.
- Registry: mock server tests for active/deprecated/malformed entries, stale
  cache fallback to the bundled copy, SSRF-guarded fetch.
- Live acceptance: fresh data dir with zero accounts → `combo-free` resolves a
  chat completion through an anonymous member; adding a Groq key that passes
  test grows the pool; turning the tier off removes it from `/v1/models` and
  routing; a consumer naming a member model directly gets an error.
- `x-9router-runtime: rust` on all touched surfaces; strict mode unaffected.

Automated verification in the current workspace: `cargo test` passes (281
tests), the frontend lint and production build gates pass, and `cargo fmt`
passes.

Live results recorded on 2026-09-21, release binary, strict mode, fresh data
directory, shipped defaults (`requireLogin` and `requireApiKey` both true):

- Unauthenticated `POST` to `/v1/models`, `/v1/models/info`, and
  `/v1/chat/completions` each answered `401 Unauthorized` — the tier is not
  reachable without a key.
- After creating one API key, that consumer's `/v1/models` listed exactly
  `combo-free`.
- `POST /v1/chat/completions` with `model: "combo-free"` returned `HTTP 200`
  with a real completion served by an anonymous registry member. This is the
  first leg of the live acceptance above, and it passes end to end.
- Turning the tier off removed `combo-free` from the consumer's list and let a
  directly created provider connection be called with the key (`200`);
  re-enabling restored the `404`. Naming a model that is neither `combo-free`
  nor expose-directly answered
  `{"error":"The model '…' does not exist or you do not have access to it."}`.

Still pending: the keyed-member leg (a Groq key that passes its test growing the
pool) needs a real provider key, and none was available for this run. The
pool-contribution remedy in the visibility rules was not exercised live either,
because contributing an `openai-compatible-*` member requires a public HTTPS
upstream and the available stand-in was loopback. The registry change has not
been exercised against a live sync either, so the anonymous leg above ran
against the bundled seed. The inference checks send a prompt to third-party
providers and should keep using an explicit, harmless test prompt.

## Risks and limits

- Shared quotas: all consumers draw on the same free limits; the admin page
  shows the ceiling but cannot raise it.
- Registry trust: the feed decides which endpoints receive user prompts; mitigated
  by repo pinning, PR review, HTTPS, and the SSRF guard.
- Upstream ToS changes can invalidate a member at any time; the registry exists
  precisely to absorb that.
- Free-tier rate limits make `combo-free` best-effort, not a capacity guarantee.
- Privacy: free model providers may log prompts or use them to improve services.
  The dashboard and model metadata warn consumers not to send confidential or
  personal data through the shared combo.

## Non-goals

- CLI-wrapped providers (e.g. Freebuff) — the registry accepts real API
  endpoints only (`class: "api"`).
- Per-consumer quota accounting or paid-tier members.

## Future: entitlements (deliberate seam, not built now)

Monetization is out of scope, but the design leaves the seam open without
rework:

- The registry sync URL can move to an authenticated endpoint (token header)
  without schema changes; entries can gain an optional `tier` field.
- Entitlement checks map onto the existing `apiKeys` table (per-key plan) and
  `usageHistory` (per-key quota accounting).

A paid offering must be built on capacity the operator owns (own accounts, or
providers whose terms allow commercial use). Reselling access to third parties'
free tiers is against those providers' terms and operationally fragile — the
same reason CLI-wrapped free clients are excluded.
