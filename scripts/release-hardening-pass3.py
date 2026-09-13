#!/usr/bin/env python3
import json
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


# The delimiter audit intentionally stays simple, so avoid char literals containing
# bracket tokens in the two helpers it scans.
for path in ["rust-backend/src/auth.rs", "rust-backend/src/config.rs"]:
    replace_once(
        path,
        ".strip_prefix('[')\n        .and_then(|value| value.strip_suffix(']'))",
        '.strip_prefix("[")\n        .and_then(|value| value.strip_suffix("]"))',
    )

replace_section(
    "rust-backend/src/auth.rs",
    "    #[test]\n    fn session_header_requires_hs256_jwt() {",
    "\n    }\n}",
    '''    #[test]
    fn session_header_requires_hs256_jwt() {
        let valid = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let wrong_algorithm = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let critical = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","crit":["exp"]}"#);
        assert!(valid_session_header(&valid));
        assert!(!valid_session_header(&wrong_algorithm));
        assert!(!valid_session_header(&critical));
    }
''',
)
# replace_section keeps the matched closing marker; the replacement already closes
# the test, so retain only the module close.
replace_once(
    "rust-backend/src/auth.rs",
    "    }\n\n    }\n}",
    "    }\n}\n",
)

# Exact method+path public policy in both Rust layers.
replace_section(
    "rust-backend/src/app.rs",
    "fn is_public_management_request(method: &Method, path: &str) -> bool {",
    "\nfn is_always_protected_path",
    '''fn is_public_management_request(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/init")
            | ("GET", "/api/version")
            | ("GET", "/api/locale")
            | ("GET", "/api/settings/require-login")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("GET", "/api/auth/oidc/start")
            | ("GET", "/api/auth/oidc/callback")
            | ("GET", "/api/auth/saml/start")
            | ("POST", "/api/auth/saml/acs")
            | ("GET", "/api/auth/saml/metadata")
    )
}
''',
)
replace_once(
    "rust-backend/src/app.rs",
    '''        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/login"
        ));
''',
    '''        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/login"
        ));
        assert!(!is_public_management_request(
            &Method::POST,
            "/api/auth/oidc/start"
        ));
        assert!(!is_public_management_request(
            &Method::GET,
            "/api/auth/saml/acs"
        ));
''',
)

replace_once(
    "rust-backend/src/management.rs",
    "use crate::{auth, error::AppError, providers, state::AppState};",
    "use crate::{auth, error::AppError, login_limiter, providers, state::AppState};",
)
replace_once(
    "rust-backend/src/management.rs",
    "const MAX_BODY: usize = 128 * 1024 * 1024;\n",
    '''const MAX_BODY: usize = 128 * 1024 * 1024;
const RESET_HINT: &str =
    "Forgot password? Reset to default via 9Router CLI -> Settings -> Reset Password to Default.";
''',
)
replace_once(
    "rust-backend/src/management.rs",
    "    if !is_public(&path) {",
    "    if !is_public(&method, &path) {",
)
replace_section(
    "rust-backend/src/management.rs",
    "fn is_public(path: &str) -> bool {",
    "\nasync fn dispatch(",
    '''fn is_public(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/api/health")
            | ("GET", "/api/init")
            | ("GET", "/api/version")
            | ("POST", "/api/auth/login")
            | ("POST", "/api/auth/logout")
            | ("GET", "/api/auth/status")
            | ("GET", "/api/settings/require-login")
            | ("GET", "/api/auth/oidc/start")
            | ("GET", "/api/auth/oidc/callback")
            | ("GET", "/api/auth/saml/start")
            | ("POST", "/api/auth/saml/acs")
            | ("GET", "/api/auth/saml/metadata")
    )
}
''',
)
replace_once(
    "rust-backend/src/management.rs",
    '("POST", "/api/auth/logout") => logout(),',
    '("POST", "/api/auth/logout") => logout(headers),',
)
replace_section(
    "rust-backend/src/management.rs",
    "fn login(\n",
    "fn reset_password(\n",
    '''fn login(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
    body: Value,
) -> Result<Response<Body>, AppError> {
    let ip = auth::rate_limit_ip(peer, headers);
    let lock = login_limiter::check_lock(ip);
    if lock.locked {
        return login_locked_response(lock.retry_after_secs);
    }

    let settings = state.db.settings()?;
    if is_tunnel_request(headers, &settings)
        && settings
            .get("tunnelDashboardAccess")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return json_response_no_store(
            StatusCode::FORBIDDEN,
            json!({"error":"Dashboard access via tunnel is disabled"}),
        );
    }
    if let Some(message) = password_login_disabled(&settings) {
        return json_response_no_store(StatusCode::FORBIDDEN, json!({"error":message}));
    }

    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    if !auth::verify_password(state, password)? {
        let remaining = login_limiter::record_failure(ip);
        let post_lock = login_limiter::check_lock(ip);
        if post_lock.locked {
            return login_locked_response(post_lock.retry_after_secs);
        }
        return json_response_no_store(
            StatusCode::UNAUTHORIZED,
            json!({
                "error":format!("Invalid password. {remaining} attempt(s) left before lockout."),
                "remainingBeforeLock":remaining
            }),
        );
    }
    login_limiter::record_success(ip);

    let stored = settings
        .get("password")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let env_initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if stored.is_none() && env_initial.is_none() && !auth::is_direct_loopback_request(peer, headers)
    {
        return json_response_no_store(
            StatusCode::FORBIDDEN,
            json!({"success":false,"error":"Default password must be changed before remote access. Change it from the local machine (or set INITIAL_PASSWORD).","mustChangePassword":true}),
        );
    }

    let token = auth::create_session_token(state, Map::new())?;
    let mut response = json_response_no_store(
        StatusCode::OK,
        json!({"success":true,"mustChangePassword":false}),
    )?;
    response.headers_mut().append(
        header::SET_COOKIE,
        auth::session_cookie_header(headers, &token),
    );
    Ok(response)
}

fn logout(headers: &HeaderMap) -> Result<Response<Body>, AppError> {
    let mut response = json_response_no_store(StatusCode::OK, json!({"success":true}))?;
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth::clear_session_cookie_header(headers));
    Ok(response)
}

fn auth_status(state: &AppState, headers: &HeaderMap) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let authenticated = auth::cookie(headers, "auth_token")
        .map(|token| auth::verify_session_token(state, &token))
        .unwrap_or(false);
    let oidc = oidc_configured(&settings);
    let saml = saml_configured(&settings);
    json_response_no_store(
        StatusCode::OK,
        json!({"requireLogin":settings.get("requireLogin").and_then(Value::as_bool).unwrap_or(true),"authMode":settings.get("authMode").cloned().unwrap_or(json!("password")),"ssoType":settings.get("ssoType").cloned().unwrap_or(json!("oidc")),"oidcConfigured":oidc,"oidcLoginLabel":settings.get("oidcLoginLabel").cloned().unwrap_or(json!("Sign in with OIDC")),"samlConfigured":saml,"samlLoginLabel":settings.get("samlLoginLabel").cloned().unwrap_or(json!("Sign in with SAML SSO")),"hasPassword":settings.get("password").and_then(Value::as_str).is_some_and(|value|!value.trim().is_empty()),"displayName":"Password user","loginMethod":"Password","authenticated":authenticated,"oidcName":null,"oidcEmail":null,"oidcLogin":false,"samlName":null,"samlEmail":null,"samlLogin":false}),
    )
}

fn login_locked_response(retry_after_secs: u64) -> Result<Response<Body>, AppError> {
    let mut response = json_response_no_store(
        StatusCode::TOO_MANY_REQUESTS,
        json!({
            "error":format!("Too many failed attempts. Try again in {retry_after_secs}s. {RESET_HINT}"),
            "retryAfter":retry_after_secs,
            "resetHint":RESET_HINT
        }),
    )?;
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_secs.to_string())
            .map_err(|error| AppError::Internal(error.into()))?,
    );
    Ok(response)
}

fn password_login_disabled(settings: &Value) -> Option<&'static str> {
    let mode = settings
        .get("authMode")
        .and_then(Value::as_str)
        .unwrap_or("password");
    if !matches!(mode, "sso" | "saml" | "oidc") {
        return None;
    }
    let sso_type = settings
        .get("ssoType")
        .and_then(Value::as_str)
        .unwrap_or(if mode == "saml" { "saml" } else { "oidc" });
    match sso_type {
        "saml" if saml_configured(settings) => {
            Some("Password login is disabled. Use SAML SSO sign in.")
        }
        "oidc" if oidc_configured(settings) => {
            Some("Password login is disabled. Use OIDC sign in.")
        }
        _ => None,
    }
}

fn nonempty_setting(settings: &Value, key: &str) -> bool {
    settings
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn oidc_configured(settings: &Value) -> bool {
    nonempty_setting(settings, "oidcIssuerUrl")
        && nonempty_setting(settings, "oidcClientId")
        && nonempty_setting(settings, "oidcClientSecret")
}

fn saml_configured(settings: &Value) -> bool {
    nonempty_setting(settings, "samlEntryPoint") && nonempty_setting(settings, "samlCert")
}

fn is_tunnel_request(headers: &HeaderMap, settings: &Value) -> bool {
    let Some(host) = request_hostname(headers) else {
        return false;
    };
    ["tunnelUrl", "tailscaleUrl"].iter().any(|key| {
        settings
            .get(*key)
            .and_then(Value::as_str)
            .and_then(|value| url::Url::parse(value).ok())
            .and_then(|value| value.host_str().map(str::to_string))
            .is_some_and(|configured| configured.eq_ignore_ascii_case(&host))
    })
}

fn request_hostname(headers: &HeaderMap) -> Option<String> {
    let authority = headers
        .get(header::HOST)?
        .to_str()
        .ok()?
        .parse::<axum::http::uri::Authority>()
        .ok()?;
    Some(authority.host().trim_matches(['[', ']']).to_ascii_lowercase())
}

''',
)
replace_section(
    "rust-backend/src/management.rs",
    "fn settings_update(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {",
    "\nfn providers_get",
    '''fn settings_update(state: &AppState, mut body: Value) -> Result<Response<Body>, AppError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Invalid settings payload".into()))?;

    object.remove("password");
    object.remove("mitmSudoEncrypted");
    let new_password = object
        .remove("newPassword")
        .and_then(|value| value.as_str().map(str::to_string))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let current_password = object
        .remove("currentPassword")
        .and_then(|value| value.as_str().map(str::to_string));

    if object
        .get("oidcClientSecret")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().is_empty())
    {
        object.remove("oidcClientSecret");
    }

    if let Some(new_password) = new_password {
        let settings = state.db.settings()?;
        let has_stored_password = settings
            .get("password")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        if has_stored_password {
            let Some(current_password) = current_password.as_deref() else {
                return json_response_no_store(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Current password required"}),
                );
            };
            if !auth::verify_password(state, current_password)? {
                return json_response_no_store(
                    StatusCode::UNAUTHORIZED,
                    json!({"error":"Invalid current password"}),
                );
            }
        } else if current_password
            .as_deref()
            .is_some_and(|value| !value.is_empty() && value != "123456")
        {
            return json_response_no_store(
                StatusCode::UNAUTHORIZED,
                json!({"error":"Invalid current password"}),
            );
        }
        object.insert(
            "password".into(),
            json!(bcrypt::hash(new_password, 10)
                .map_err(|error| AppError::Internal(anyhow::anyhow!(error.to_string())))?),
        );
    }

    let settings = state.db.update_settings(body)?;
    let mut safe = settings.clone();
    if let Some(object) = safe.as_object_mut() {
        for key in ["password", "oidcClientSecret"] {
            object.remove(key);
        }
    }
    json_response_no_store(
        StatusCode::OK,
        json!({"settings":safe,"success":true}),
    )
}
''',
)
replace_once(
    "rust-backend/src/management.rs",
    "fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {",
    '''fn json_response_no_store(
    status: StatusCode,
    value: Value,
) -> Result<Response<Body>, AppError> {
    let mut response = json_response(status, value)?;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn json_response(status: StatusCode, value: Value) -> Result<Response<Body>, AppError> {''',
)

# Trust public routes by method in standalone Next mode and parse tunnel hosts as
# proper authorities so IPv6 literals are not truncated.
replace_section(
    "frontend/src/dashboardGuard.js",
    "const PUBLIC_API_PATHS = [",
    "];\n\n// Public top-level prefixes",
    '''const PUBLIC_API_ROUTES = new Set([
  "GET /api/health",
  "GET /api/init",
  "GET /api/locale",
  "POST /api/auth/login",
  "POST /api/auth/logout",
  "GET /api/auth/status",
  "GET /api/auth/oidc/start",
  "GET /api/auth/oidc/callback",
  "GET /api/auth/saml/start",
  "POST /api/auth/saml/acs",
  "GET /api/auth/saml/metadata",
  "GET /api/version",
  "GET /api/settings/require-login",
]);
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''function isPublicApi(pathname) {
  if (isPublicLlmApi(pathname)) return true;
  return PUBLIC_API_PATHS.includes(pathname);
}
''',
    '''function isPublicApi(request) {
  const pathname = request.nextUrl.pathname;
  if (isPublicLlmApi(pathname)) return true;
  const method = request.method.toUpperCase();
  if (PUBLIC_API_ROUTES.has(`${method} ${pathname}`)) return true;
  return method === "OPTIONS" && [...PUBLIC_API_ROUTES].some((route) => route.endsWith(` ${pathname}`));
}

function requestHostname(request) {
  const authority = request.headers.get("host") || "";
  if (!authority) return "";
  try {
    return new URL(`http://${authority}`).hostname.toLowerCase();
  } catch {
    return "";
  }
}
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    "    if (isPublicApi(pathname)) return NextResponse.next();",
    "    if (isPublicApi(request)) return NextResponse.next();",
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''          const host = (request.headers.get("host") || "").split(":")[0].toLowerCase();
''',
    '''          const host = requestHostname(request);
''',
)
replace_once(
    "frontend/src/dashboardGuard.js",
    '''  canAccessLocalOnlyRoute,
};
''',
    '''  canAccessLocalOnlyRoute,
  isPublicApi,
  requestHostname,
};
''',
)

replace_once(
    "frontend/src/app/api/auth/login/route.js",
    '''function isTunnelRequest(request, settings) {
  const host = (request.headers.get("host") || "").split(":")[0].toLowerCase();
''',
    '''function isTunnelRequest(request, settings) {
  let host = "";
  const authority = request.headers.get("host") || "";
  try {
    host = new URL(`http://${authority}`).hostname.toLowerCase();
  } catch {
    host = "";
  }
''',
)

# Rust stamps a secret on every private UI request; the custom server accepts
# forwarded identities only when both loopback transport and that secret agree.
replace_once(
    "rust-backend/src/ui_proxy.rs",
    '''    if let Ok(v) = reqwest::header::HeaderValue::from_str(&peer.ip().to_string()) {
''',
    '''    let client_ip = auth::rate_limit_ip(peer, &parts.headers);
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&client_ip.to_string()) {
''',
)
replace_once(
    "rust-backend/src/ui_proxy.rs",
    '''    h.insert(
        reqwest::header::HeaderName::from_static("x-9r-ui-proxy"),
''',
    '''    let secret = state.config.ui_only_header_secret.trim();
    if secret.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "NINEROUTER_UI_SECRET must not be empty"
        )));
    }
    h.insert(
        reqwest::header::HeaderName::from_static("x-9router-ui-secret"),
        reqwest::header::HeaderValue::from_str(secret).map_err(|error| {
            AppError::Internal(anyhow::anyhow!("invalid NINEROUTER_UI_SECRET: {error}"))
        })?,
    );
    h.insert(
        reqwest::header::HeaderName::from_static("x-9r-ui-proxy"),
''',
)
replace_once(
    "rust-backend/src/ui_proxy.rs",
    '''                | "content-length"
        ) {
''',
    '''                | "content-length"
                | "forwarded"
                | "x-forwarded-for"
                | "x-forwarded-host"
                | "x-forwarded-proto"
                | "x-real-ip"
                | "x-9router-ui-secret"
                | "x-9r-rust-compat"
                | "x-9r-ui-proxy"
                | "x-9r-real-ip"
                | "x-9r-peer-token"
                | "x-9r-via-proxy"
        ) {
''',
)
replace_once(
    "rust-backend/src/compat_proxy.rs",
    '''    if let Ok(value) = reqwest::header::HeaderValue::from_str(&peer.ip().to_string()) {
''',
    '''    let client_ip = auth::rate_limit_ip(peer, headers);
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&client_ip.to_string()) {
''',
)

replace_once(
    "frontend/custom-server.js",
    '''const origCreate = http.createServer.bind(http);

// Per-process secret proving x-9r-real-ip was stamped below rather than sent by the client.
''',
    '''const origCreate = http.createServer.bind(http);
const INTERNAL_SECRET_HEADER = "x-9router-ui-secret";

function timingSafeStringEqual(left, right) {
  const leftBuffer = Buffer.from(String(left || ""));
  const rightBuffer = Buffer.from(String(right || ""));
  return leftBuffer.length === rightBuffer.length && crypto.timingSafeEqual(leftBuffer, rightBuffer);
}

function isLoopbackAddress(value) {
  let address = String(value || "").trim().toLowerCase();
  if (address.startsWith("[") && address.endsWith("]")) address = address.slice(1, -1);
  if (address.startsWith("::ffff:")) address = address.slice(7);
  return address === "127.0.0.1" || address === "::1" || address === "localhost";
}

// Per-process secret proving x-9r-real-ip was stamped below rather than sent by the client.
''',
)
replace_section(
    "frontend/custom-server.js",
    '''    const socketIp = req.socket && req.socket.remoteAddress ? req.socket.remoteAddress : "";
''',
    '''    return handler(req, res);
''',
    '''    const socketIp = req.socket && req.socket.remoteAddress ? req.socket.remoteAddress : "";
    const xff = req.headers["x-forwarded-for"];
    const xRealIp = req.headers["x-real-ip"];
    const configuredSecret = process.env.NINEROUTER_UI_SECRET || "";
    const suppliedSecret = req.headers[INTERNAL_SECRET_HEADER] || "";
    const isLoopbackProxy = isLoopbackAddress(socketIp);
    const trustedRustProxy = isLoopbackProxy
      && configuredSecret
      && timingSafeStringEqual(suppliedSecret, configuredSecret);
    const proxyIp = trustedRustProxy
      ? (xRealIp || (xff ? String(xff).split(",")[0].trim() : ""))
      : "";
    const ip = proxyIp || socketIp;
    const viaProxy = Boolean(trustedRustProxy && proxyIp && !isLoopbackAddress(proxyIp));
    delete req.headers["x-9r-real-ip"];
    delete req.headers["x-forwarded-for"];
    delete req.headers["x-real-ip"];
    delete req.headers["x-9r-via-proxy"];
    delete req.headers["x-9r-peer-token"];
    if (!trustedRustProxy) delete req.headers[INTERNAL_SECRET_HEADER];
    req.headers["x-9r-real-ip"] = ip;
    req.headers["x-9r-peer-token"] = PEER_TOKEN;
    if (viaProxy) req.headers["x-9r-via-proxy"] = "1";
    return handler(req, res);
''',
)

# An unexpected child exit is a stack failure even when that child returns 0.
for path in ["scripts/run-prod.sh", "scripts/run-dev.sh", "scripts/run-full-stack-strict.sh"]:
    replace_once(
        path,
        '''  set -e
  echo "$component exited with status $status" >&2
  return "$status"
''',
        '''  set -e
  if [[ "$status" == 0 ]]; then status=1; fi
  echo "$component exited unexpectedly with status $status" >&2
  return "$status"
''',
    )

package_path = Path("frontend/package.json")
package = json.loads(package_path.read_text())
package.setdefault("scripts", {})["lint"] = "eslint . --max-warnings=0"
package["dependencies"]["monaco-editor"] = "^0.56.0"
package_path.write_text(json.dumps(package, indent=2, ensure_ascii=False) + "\n")

print("credential and dependency hardening pass applied")
