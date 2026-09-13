#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


def replace_section(path: str, start: str, end: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text()
    start_at = text.find(start)
    if start_at < 0:
        raise RuntimeError(f"{path}: start marker not found: {start!r}")
    end_at = text.find(end, start_at)
    if end_at < 0:
        raise RuntimeError(f"{path}: end marker not found: {end!r}")
    file.write_text(text[:start_at] + replacement + text[end_at:])


# Use the shared, proxy-aware policy for every native LLM/media endpoint.
replace_once(
    "rust-backend/src/gateway.rs",
    '''pub(crate) fn authorize_llm(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    query_key: Option<&str>,
) -> Result<(), AppError> {
    let settings = state.db.settings()?;
    let local = auth::is_loopback_ip(peer.ip());
    let require = !local
        || settings
            .get("requireApiKey")
            .and_then(Value::as_bool)
            .unwrap_or(true);
    if !require {
        return Ok(());
    }
    let key = auth::extract_api_key(headers, query_key);
    match key {
        Some(k) if state.db.validate_api_key(&k)? => Ok(()),
        _ => Err(AppError::Unauthorized),
    }
}
''',
    '''pub(crate) fn authorize_llm(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    query_key: Option<&str>,
) -> Result<(), AppError> {
    auth::require_llm(state, headers, peer, query_key)
}
''',
)

# Never forward client-supplied internal trust markers to the private Next listener.
replace_once(
    "rust-backend/src/ui_proxy.rs",
    '''            "host"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
''',
    '''            "host"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
                | "forwarded"
                | "x-forwarded-for"
                | "x-forwarded-host"
                | "x-forwarded-proto"
                | "x-real-ip"
                | "cf-connecting-ip"
                | "true-client-ip"
                | "x-client-ip"
                | "x-cluster-client-ip"
                | "x-9router-ui-secret"
                | "x-9r-rust-compat"
                | "x-9r-ui-proxy"
                | "x-9r-real-ip"
                | "x-9r-peer-token"
                | "x-9r-via-proxy"
''',
)
replace_once(
    "rust-backend/src/ui_proxy.rs",
    '''    if !h.contains_key(reqwest::header::HeaderName::from_static(
        "x-forwarded-proto",
    )) {
        h.insert(
            reqwest::header::HeaderName::from_static("x-forwarded-proto"),
            reqwest::header::HeaderValue::from_static("http"),
        );
    }
''',
    '''    let forwarded_proto = if auth::is_loopback(peer)
        && parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
    {
        "https"
    } else {
        "http"
    };
    h.insert(
        reqwest::header::HeaderName::from_static("x-forwarded-proto"),
        reqwest::header::HeaderValue::from_static(forwarded_proto),
    );
''',
)
replace_once(
    "rust-backend/src/ui_proxy.rs",
    'reqwest::header::HeaderValue::from_static("rust-1.0.1"),',
    'reqwest::header::HeaderValue::from_static(concat!("rust-", env!("CARGO_PKG_VERSION"))),',
)

replace_once(
    "rust-backend/src/legacy_proxy.rs",
    'if is_hop(&name) || name == "host" || name == "content-length" {',
    'if is_hop(&name) || is_internal_header(&name) || name == "host" || name == "content-length" {',
)
replace_once(
    "rust-backend/src/legacy_proxy.rs",
    'reqwest::header::HeaderValue::from_static("1.0.1"),',
    'reqwest::header::HeaderValue::from_static(env!("CARGO_PKG_VERSION")),',
)
replace_once(
    "rust-backend/src/legacy_proxy.rs",
    '''fn is_hop(name: &str) -> bool {
''',
    '''fn is_internal_header(name: &str) -> bool {
    matches!(
        name,
        "x-9router-ui-secret"
            | "x-9r-rust-compat"
            | "x-9r-ui-proxy"
            | "x-9r-real-ip"
            | "x-9r-peer-token"
            | "x-9r-via-proxy"
    )
}

fn is_hop(name: &str) -> bool {
''',
)

# Resolve alias graphs iteratively. Cycles and ambiguous legacy orientation can no
# longer recurse until stack overflow.
replace_once(
    "rust-backend/src/providers.rs",
    'use serde_json::{json, Value};\n',
    'use serde_json::{json, Value};\nuse std::collections::{HashSet, VecDeque};\n',
)
replace_section(
    "rust-backend/src/providers.rs",
    "pub fn resolve_model(state: &AppState, requested: &str) -> Result<ResolvedModel, AppError> {",
    "\nfn resolve_for_provider(",
    '''pub fn resolve_model(state: &AppState, requested: &str) -> Result<ResolvedModel, AppError> {
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
''',
)

# Return the actual persisted combo row after an ON CONFLICT update, and mask
# Unicode keys without slicing through a UTF-8 code point.
replace_once(
    "rust-backend/src/db.rs",
    '''        self.with_conn(|db|{ db.execute("INSERT INTO combos(id,name,kind,models,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(name) DO UPDATE SET kind=excluded.kind,models=excluded.models,updatedAt=excluded.updatedAt",params![id,name,kind,models_s,now])?; Ok(())})?;
        Ok(json!({"id":id,"name":name,"kind":kind,"models":models,"createdAt":now,"updatedAt":now}))
''',
    '''        self.with_conn(|db|{ db.execute("INSERT INTO combos(id,name,kind,models,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(name) DO UPDATE SET kind=excluded.kind,models=excluded.models,updatedAt=excluded.updatedAt",params![id,name,kind,models_s,now])?; Ok(())})?;
        self.combo_by_name(name)?.ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("combo upsert succeeded but row was not found"))
        })
''',
)
replace_once(
    "rust-backend/src/db.rs",
    '''fn mask_key(key: Option<&str>) -> Option<String> {
    key.map(|k| {
        if k.len() <= 8 {
            format!("{}***", k.chars().next().unwrap_or('*'))
        } else {
            format!("{}***", &k[..8])
        }
    })
}
''',
    '''fn mask_key(key: Option<&str>) -> Option<String> {
    key.map(|key| {
        if key.chars().count() <= 8 {
            format!("{}***", key.chars().next().unwrap_or('*'))
        } else {
            format!("{}***", key.chars().take(8).collect::<String>())
        }
    })
}
''',
)

# Strict/native management must use exact public routes and never disclose stored
# provider credentials.
replace_section(
    "rust-backend/src/management.rs",
    "fn is_public(path: &str) -> bool {",
    "\nasync fn dispatch(",
    '''fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/api/health"
            | "/api/init"
            | "/api/version"
            | "/api/auth/login"
            | "/api/auth/logout"
            | "/api/auth/status"
            | "/api/settings/require-login"
            | "/api/auth/oidc/start"
            | "/api/auth/oidc/callback"
            | "/api/auth/saml/start"
            | "/api/auth/saml/acs"
            | "/api/auth/saml/metadata"
    )
}
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '("POST", "/api/auth/reset-password") => reset_password(state, peer),',
    '("POST", "/api/auth/reset-password") => reset_password(state, peer, headers),',
)
replace_once(
    "rust-backend/src/management.rs",
    '''    let env_initial = std::env::var("INITIAL_PASSWORD").ok();
    if stored.is_none() && env_initial.is_none() && !auth::is_loopback(peer) {
''',
    '''    let env_initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if stored.is_none()
        && env_initial.is_none()
        && !auth::is_direct_loopback_request(peer, headers)
    {
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''fn reset_password(state: &AppState, peer: SocketAddr) -> Result<Response<Body>, AppError> {
    if !auth::is_loopback(peer) {
''',
    '''fn reset_password(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<Response<Body>, AppError> {
    if !auth::is_direct_loopback_request(peer, headers) {
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''    for c in &mut cs {
        if let Some(o) = c.as_object_mut() {
            for k in ["apiKey", "accessToken", "refreshToken", "idToken"] {
                o.remove(k);
            }
        }
    }
''',
    '''    for connection in &mut cs {
        redact_secrets(connection);
    }
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''    let mut c = state.db.create_connection(body)?;
    if let Some(o) = c.as_object_mut() {
        for k in ["apiKey", "accessToken", "refreshToken", "idToken"] {
            o.remove(k);
        }
    }
''',
    '''    let mut c = state.db.create_connection(body)?;
    redact_secrets(&mut c);
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''                let c = state
                    .db
                    .provider_connection(id)?
                    .ok_or_else(|| AppError::NotFound("provider connection".into()))?;
                json_response(StatusCode::OK, json!({"connection":c}))
''',
    '''                let mut c = state
                    .db
                    .provider_connection(id)?
                    .ok_or_else(|| AppError::NotFound("provider connection".into()))?;
                redact_secrets(&mut c);
                json_response(StatusCode::OK, json!({"connection":c}))
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''                let mut c = state.db.update_connection(id, body)?;
                if let Some(o) = c.as_object_mut() {
                    for k in ["apiKey", "accessToken", "refreshToken", "idToken"] {
                        o.remove(k);
                    }
                }
''',
    '''                let mut c = state.db.update_connection(id, body)?;
                redact_secrets(&mut c);
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '''fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
''',
    '''fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "apikey"
                        | "api_key"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "authtoken"
                        | "sessiontoken"
                        | "accounttoken"
                        | "password"
                        | "clientsecret"
                        | "client_secret"
                        | "privatekey"
                        | "private_key"
                        | "cookie"
                        | "authorization"
                        | "token"
                )
            });
            for child in object.values_mut() {
                redact_secrets(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_secrets(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {
''',
)
replace_once(
    "rust-backend/src/management.rs",
    'json!({"status":"ok","version":"1.0.1","runtime":"rust","upstreamSnapshot":"eb712ca821f0ba6bc41043fbd14494c5af5daba5"}),',
    'json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"runtime":"rust","upstreamSnapshot":"17c4cc76877bd1755030a8414f8d0083f48dcccf"}),',
)
replace_once(
    "rust-backend/src/management.rs",
    'json!({"initialized":true,"runtime":"rust","version":"1.0.1"}),',
    'json!({"initialized":true,"runtime":"rust","version":env!("CARGO_PKG_VERSION")}),',
)
replace_once(
    "rust-backend/src/management.rs",
    'json!({"version":"1.0.1","name":"9router-rust","rustBackend":true,"upstreamVersion":"0.5.69"}),',
    'json!({"version":env!("CARGO_PKG_VERSION"),"name":"9router-rust","rustBackend":true,"upstreamVersion":"0.5.75"}),',
)

# Keep Gemini credentials out of URLs and therefore out of transport errors/logs.
replace_once(
    "rust-backend/src/media.rs",
    '''        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{model_path}:{op}?key={}",
            url::form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        );
''',
    '''        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{model_path}:{op}"
        );
''',
)
replace_once(
    "rust-backend/src/media.rs",
    '''        (url, request_body, HeaderMap::new())
''',
    '''        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-goog-api-key"),
            HeaderValue::from_str(&key)
                .map_err(|error| AppError::BadRequest(format!("invalid Gemini API key: {error}")))?,
        );
        (url, request_body, headers)
''',
)

# Mirror the exact allow-list and host-control policy in standalone Next mode.
replace_once(
    "frontend/src/dashboardGuard.js",
    '''  "/api/auth/oidc",
  "/api/auth/saml",
''',
    '''  "/api/auth/oidc/start",
  "/api/auth/oidc/callback",
  "/api/auth/saml/start",
  "/api/auth/saml/acs",
  "/api/auth/saml/metadata",
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''const LOCAL_ONLY_PATHS = [
  "/api/cli-tools/cowork-settings",
  "/api/cli-tools/antigravity-mitm",
  "/api/mcp/",
  "/api/tunnel/tailscale-install",
  "/api/tunnel/tailscale-enable",
  "/api/tunnel/tailscale-disable",
  "/api/tunnel/tailscale-check",
  "/api/tunnel/enable",
  "/api/tunnel/disable",
  "/api/oauth/cursor/auto-import",
  "/api/oauth/kiro/auto-import",
  "/api/auth/reset-password",
  "/api/headroom/start",
  "/api/headroom/stop",
  "/api/headroom/proxy",
];
''',
    '''const LOCAL_ONLY_PATHS = [
  "/api/cli-tools/",
  "/api/mcp/",
  "/api/tunnel/",
  "/api/oauth/cursor/auto-import",
  "/api/oauth/kiro/auto-import",
  "/api/auth/reset-password",
  "/api/headroom/",
  "/api/pxpipe/",
  "/api/shutdown",
  "/api/version/shutdown",
  "/api/version/update",
];
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''function extractApiKey(request) {
  const authHeader = request.headers.get("Authorization");
  if (authHeader?.startsWith("Bearer ")) return authHeader.slice(7);
''',
    '''function extractApiKey(request) {
  const authHeader = request.headers.get("Authorization");
  if (authHeader) {
    const match = authHeader.trim().match(/^Bearer\\s+(\\S+)$/i);
    if (match) return match[1];
  }
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''  return PUBLIC_API_PATHS.some((p) => pathname === p || pathname.startsWith(`${p}/`));
''',
    '''  return PUBLIC_API_PATHS.includes(pathname);
''',
)

print("release hardening patch applied")
