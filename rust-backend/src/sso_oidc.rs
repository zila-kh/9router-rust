//! Native OIDC SSO routes (`/api/auth/oidc/**`), ported from the pinned
//! upstream `src/app/api/auth/oidc/**` route files and `lib/auth/oidc.js`.
//! Authorization requests use PKCE (S256), the id_token is verified against
//! the discovery JWKS with issuer/audience/nonce checks, and the diagnostic
//! probe classifies token-endpoint rejections exactly like
//! `probeOidcClientSecret`.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    auth,
    error::AppError,
    sso::{clear_cookie_header, public_origin, redirect_response, set_cookie_header, urlencode},
    state::AppState,
};

const STATE_COOKIE: &str = "oidc_state";
const NONCE_COOKIE: &str = "oidc_nonce";
const VERIFIER_COOKIE: &str = "oidc_code_verifier";
const DEFAULT_SCOPES: &str = "openid profile email";
const DEFAULT_LOGIN_LABEL: &str = "Sign in with OIDC";
const COOKIE_MAX_AGE: i64 = 10 * 60;

pub struct OidcRuntimeConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: String,
    pub login_label: String,
}

fn trim_trailing_slashes(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn normalize_scopes(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SCOPES)
        .to_string()
}

/// Port of upstream `getOidcRuntimeConfig`.
pub fn get_oidc_runtime_config(state: &AppState) -> Result<Option<OidcRuntimeConfig>, AppError> {
    let settings = state.db.settings()?;
    let auth_mode = settings
        .get("authMode")
        .and_then(Value::as_str)
        .unwrap_or("password");
    if auth_mode != "oidc" && auth_mode != "both" {
        return Ok(None);
    }

    let issuer_url = trim_trailing_slashes(
        settings
            .get("oidcIssuerUrl")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let client_id = settings
        .get("oidcClientId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let client_secret = settings
        .get("oidcClientSecret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if issuer_url.is_empty() || client_id.is_empty() || client_secret.is_empty() {
        return Ok(None);
    }

    Ok(Some(OidcRuntimeConfig {
        issuer_url,
        client_id,
        client_secret,
        scopes: normalize_scopes(settings.get("oidcScopes").and_then(Value::as_str)),
        login_label: settings
            .get("oidcLoginLabel")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_LOGIN_LABEL.to_string()),
    }))
}

async fn fetch_json(state: &AppState, url: &str) -> Result<Value, String> {
    let response = state
        .http
        .get(url)
        .header(header::CACHE_CONTROL, "no-store")
        .send()
        .await
        .map_err(|error| format!("Request to {url} failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Request to {url} failed with HTTP {}",
            response.status()
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| format!("Invalid JSON document from {url}"))
}

async fn fetch_oidc_discovery(state: &AppState, issuer_url: &str) -> Result<Value, String> {
    let discovery_url = format!("{issuer_url}/.well-known/openid-configuration");
    let document = fetch_json(state, &discovery_url)
        .await
        .map_err(|_| format!("Failed to load OIDC discovery document from {discovery_url}"))?;
    Ok(document)
}

fn discovery_string<'a>(document: &'a Value, key: &str) -> &'a str {
    document.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn create_pkce_pair() -> (String, String) {
    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_b64url(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// Port of upstream `buildOidcAuthorizationUrl`.
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &str,
    state_value: &str,
    nonce: &str,
    code_challenge: &str,
) -> Result<String, String> {
    let mut url = url::Url::parse(authorization_endpoint)
        .map_err(|error| format!("Invalid authorization endpoint: {error}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("response_type", "code");
        pairs.append_pair("client_id", client_id);
        pairs.append_pair("redirect_uri", redirect_uri);
        pairs.append_pair("scope", scopes);
        pairs.append_pair("state", state_value);
        pairs.append_pair("nonce", nonce);
        pairs.append_pair("code_challenge", code_challenge);
        pairs.append_pair("code_challenge_method", "S256");
    }
    Ok(url.to_string())
}

async fn exchange_oidc_code(
    state: &AppState,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<Value, String> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret));
    }
    let response = state
        .http
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Token endpoint request failed: {error}"))?;
    let status = response.status().as_u16();
    let data = response.json::<Value>().await.unwrap_or(json!({}));
    if !(200..300).contains(&status) {
        return Err(token_error_message(&data, Some(status)));
    }
    Ok(data)
}

fn token_error_message(data: &Value, status: Option<u16>) -> String {
    let description = data
        .get("error_description")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let error = data
        .get("error")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    description
        .or(error)
        .map(str::to_string)
        .unwrap_or_else(|| match status {
            Some(status) => format!("OIDC token exchange failed ({status})"),
            None => "OIDC token exchange failed".into(),
        })
}

/// Port of upstream `probeOidcClientSecret`: posts a deliberately invalid
/// authorization code and classifies the rejection.
pub async fn probe_oidc_client_secret(
    state: &AppState,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<Value, String> {
    if client_secret.is_empty() {
        return Ok(json!({
            "tested": false,
            "valid": null,
            "message": "No client secret was provided, so secret validation was skipped."
        }));
    }

    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", "__oidc_test_invalid_code__"),
        ("redirect_uri", redirect_uri),
        ("code_verifier", "__oidc_test_invalid_verifier__"),
    ];
    let response = state
        .http
        .post(token_endpoint)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Token endpoint request failed: {error}"))?;
    let status = response.status().as_u16();
    let data = response.json::<Value>().await.unwrap_or(json!({}));
    Ok(classify_secret_probe(status, &data))
}

/// Pure classification half of `probeOidcClientSecret`, unit-testable without
/// network access.
pub fn classify_secret_probe(status: u16, data: &Value) -> Value {
    let error = data
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let description = data
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");

    if (200..300).contains(&status) {
        return json!({
            "tested": true,
            "valid": true,
            "message": "Client secret was accepted by the token endpoint."
        });
    }
    let description_matches_client = {
        let lower = description.to_ascii_lowercase();
        lower.contains("client")
            && (lower.contains("invalid") || lower.contains("failed") || lower.contains("mismatch"))
    };
    if error == "invalid_client" || error == "unauthorized_client" || description_matches_client {
        return json!({
            "tested": true,
            "valid": false,
            "message": if description.is_empty() { "Client secret is not valid." } else { description }
        });
    }
    if error == "invalid_grant"
        || error == "invalid_code"
        || description.to_ascii_lowercase().contains("grant")
        || description.to_ascii_lowercase().contains("code")
    {
        return json!({
            "tested": true,
            "valid": true,
            "message": "Client secret was accepted; the token exchange failed only because the test authorization code is invalid."
        });
    }
    json!({
        "tested": true,
        "valid": null,
        "message": if description.is_empty() { format!("Token endpoint responded with {status}") } else { description.to_string() }
    })
}

/// Port of upstream `verifyOidcIdToken`: JWKS lookup, algorithm restriction by
/// key type, issuer/audience validation with zero leeway, and a manual nonce
/// comparison.
pub async fn verify_oidc_id_token(
    state: &AppState,
    id_token: &str,
    issuer: &str,
    audience: &str,
    jwks_uri: &str,
    nonce: &str,
) -> Result<Value, String> {
    let header = jsonwebtoken::decode_header(id_token)
        .map_err(|error| format!("Invalid OIDC id_token: {error}"))?;
    let jwks: jsonwebtoken::jwk::JwkSet = state
        .http
        .get(jwks_uri)
        .send()
        .await
        .map_err(|error| format!("Request to {jwks_uri} failed: {error}"))?
        .json()
        .await
        .map_err(|_| format!("Invalid JWKS document from {jwks_uri}"))?;

    let jwk = if let Some(kid) = &header.kid {
        jwks.keys
            .iter()
            .find(|key| key.common.key_id.as_deref() == Some(kid))
            .ok_or("no matching key found in the JSON Web Key Set")?
    } else {
        if jwks.keys.len() != 1 {
            return Err("multiple matching keys found in the JSON Web Key Set".into());
        }
        &jwks.keys[0]
    };

    let allowed: &[Algorithm] = allowed_algorithms(jwk);
    if !allowed.contains(&header.alg) {
        return Err("unexpected JWT alg used".into());
    }

    let mut validation = Validation::new(header.alg);
    validation.leeway = 0;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    let payload = jsonwebtoken::decode::<Value>(
        id_token,
        &DecodingKey::from_jwk(jwk).map_err(|e| e.to_string())?,
        &validation,
    )
    .map_err(|error| format!("OIDC id_token validation failed: {error}"))?
    .claims;

    if payload.get("nonce").and_then(Value::as_str) != Some(nonce) {
        return Err("unexpected nonce".into());
    }
    Ok(payload)
}

fn allowed_algorithms(jwk: &jsonwebtoken::jwk::Jwk) -> &'static [Algorithm] {
    use jsonwebtoken::jwk::{AlgorithmParameters, KeyAlgorithm};
    if let Some(algorithm) = jwk.common.key_algorithm {
        return match algorithm {
            KeyAlgorithm::RS256 => &[Algorithm::RS256],
            KeyAlgorithm::RS384 => &[Algorithm::RS384],
            KeyAlgorithm::RS512 => &[Algorithm::RS512],
            KeyAlgorithm::ES256 => &[Algorithm::ES256],
            KeyAlgorithm::ES384 => &[Algorithm::ES384],
            KeyAlgorithm::EdDSA => &[Algorithm::EdDSA],
            _ => &[],
        };
    }
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => &[Algorithm::RS256, Algorithm::RS384, Algorithm::RS512],
        AlgorithmParameters::EllipticCurve(curve) => match curve.curve {
            jsonwebtoken::jwk::EllipticCurve::P256 => &[Algorithm::ES256],
            jsonwebtoken::jwk::EllipticCurve::P384 => &[Algorithm::ES384],
            _ => &[],
        },
        AlgorithmParameters::OctetKeyPair(_) => &[Algorithm::EdDSA],
        _ => &[],
    }
}

fn pick_oidc_display_name(payload: &Value) -> String {
    for key in ["preferred_username", "email", "name", "given_name", "sub"] {
        if let Some(value) = payload.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    "OIDC user".to_string()
}

fn pick_oidc_email(payload: &Value) -> Option<String> {
    payload
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// GET /api/auth/oidc/start
// ---------------------------------------------------------------------------

pub async fn handle_start(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response<Body>, AppError> {
    let origin = public_origin(headers, uri);
    let Some(config) = get_oidc_runtime_config(state)? else {
        return Ok(redirect_response(
            &format!("{origin}/login?error=oidc_not_configured"),
            Vec::new(),
        ));
    };

    match run_start(state, headers, uri, &config).await {
        Ok(response) => Ok(response),
        Err(message) => Ok(redirect_response(
            &format!("{origin}/login?error={}", urlencode(&message)),
            Vec::new(),
        )),
    }
}

async fn run_start(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    config: &OidcRuntimeConfig,
) -> Result<Response<Body>, String> {
    let discovery = fetch_oidc_discovery(state, &config.issuer_url).await?;
    let state_value = random_b64url(16);
    let nonce = random_b64url(16);
    let (verifier, challenge) = create_pkce_pair();
    let origin = public_origin(headers, uri);
    let redirect_uri = format!("{origin}/api/auth/oidc/callback");
    let auth_url = build_authorization_url(
        discovery_string(&discovery, "authorization_endpoint"),
        &config.client_id,
        &redirect_uri,
        &config.scopes,
        &state_value,
        &nonce,
        &challenge,
    )?;

    Ok(redirect_response(
        &auth_url,
        vec![
            set_cookie_header(headers, STATE_COOKIE, &state_value, COOKIE_MAX_AGE),
            set_cookie_header(headers, NONCE_COOKIE, &nonce, COOKIE_MAX_AGE),
            set_cookie_header(headers, VERIFIER_COOKIE, &verifier, COOKIE_MAX_AGE),
        ],
    ))
}

// ---------------------------------------------------------------------------
// GET /api/auth/oidc/callback
// ---------------------------------------------------------------------------

pub async fn handle_callback(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response<Body>, AppError> {
    let origin = public_origin(headers, uri);
    let query = uri.query().unwrap_or("");
    let param = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default()
    };

    let error = param("error");
    if !error.is_empty() {
        return Ok(redirect_response(
            &format!("{origin}/login?error={}", urlencode(&error)),
            Vec::new(),
        ));
    }
    let code = param("code");
    let returned_state = param("state");
    if code.is_empty() || returned_state.is_empty() {
        return Ok(redirect_response(
            &format!("{origin}/login?error=oidc_missing_code"),
            Vec::new(),
        ));
    }

    let stored_state = auth::cookie(headers, STATE_COOKIE).unwrap_or_default();
    let stored_nonce = auth::cookie(headers, NONCE_COOKIE).unwrap_or_default();
    let code_verifier = auth::cookie(headers, VERIFIER_COOKIE).unwrap_or_default();
    let clear_cookies = vec![
        clear_cookie_header(headers, STATE_COOKIE),
        clear_cookie_header(headers, NONCE_COOKIE),
        clear_cookie_header(headers, VERIFIER_COOKIE),
    ];

    if stored_state.is_empty()
        || stored_nonce.is_empty()
        || code_verifier.is_empty()
        || stored_state != returned_state
    {
        return Ok(redirect_response(
            &format!("{origin}/login?error=oidc_invalid_state"),
            clear_cookies,
        ));
    }

    let Some(config) = get_oidc_runtime_config(state)? else {
        return Ok(redirect_response(
            &format!("{origin}/login?error=oidc_not_configured"),
            clear_cookies,
        ));
    };

    match run_callback(
        state,
        &origin,
        &config,
        &code,
        &stored_nonce,
        &code_verifier,
    )
    .await
    {
        Ok(token) => {
            let mut cookies = clear_cookies;
            cookies.push(auth::session_cookie_header(headers, &token));
            Ok(redirect_response(&format!("{origin}/dashboard"), cookies))
        }
        Err(message) => Ok(redirect_response(
            &format!("{origin}/login?error={}", urlencode(&message)),
            clear_cookies,
        )),
    }
}

async fn run_callback(
    state: &AppState,
    origin: &str,
    config: &OidcRuntimeConfig,
    code: &str,
    stored_nonce: &str,
    code_verifier: &str,
) -> Result<String, String> {
    let discovery = fetch_oidc_discovery(state, &config.issuer_url).await?;
    let discovered_issuer = {
        let issuer = discovery_string(&discovery, "issuer");
        if issuer.is_empty() {
            config.issuer_url.clone()
        } else {
            issuer.to_string()
        }
    };
    let redirect_uri = format!("{origin}/api/auth/oidc/callback");
    let token_data = exchange_oidc_code(
        state,
        discovery_string(&discovery, "token_endpoint"),
        &config.client_id,
        &config.client_secret,
        code,
        &redirect_uri,
        code_verifier,
    )
    .await?;

    let id_token = token_data
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("OIDC provider did not return an id_token")?;

    let payload = verify_oidc_id_token(
        state,
        id_token,
        &discovered_issuer,
        &config.client_id,
        discovery_string(&discovery, "jwks_uri"),
        stored_nonce,
    )
    .await?;

    let mut claims = Map::new();
    claims.insert("oidc".into(), Value::Bool(true));
    claims.insert(
        "oidcSub".into(),
        payload
            .get("sub")
            .and_then(Value::as_str)
            .map(|value| Value::String(value.to_string()))
            .unwrap_or(Value::Null),
    );
    claims.insert(
        "oidcEmail".into(),
        pick_oidc_email(&payload)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    claims.insert(
        "oidcName".into(),
        Value::String(pick_oidc_display_name(&payload)),
    );
    auth::create_session_token(state, claims)
        .map_err(|error| format!("Session creation failed: {error}"))
}

// ---------------------------------------------------------------------------
// POST /api/auth/oidc/test
// ---------------------------------------------------------------------------

pub async fn handle_test(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let allowed = settings.get("requireLogin").and_then(Value::as_bool) == Some(false)
        || auth::cookie(headers, "auth_token")
            .map(|token| auth::verify_session_token(state, &token))
            .unwrap_or(false);
    if !allowed {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let body = body.as_object().cloned().unwrap_or_default();
    let body_str = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("");
    let setting_str = |key: &str| settings.get(key).and_then(Value::as_str).unwrap_or("");

    let issuer_url = first_non_empty(&[body_str("issuerUrl"), setting_str("oidcIssuerUrl")]);
    let client_id = first_non_empty(&[body_str("clientId"), setting_str("oidcClientId")]);
    let scopes = {
        let value = first_non_empty(&[body_str("scopes"), setting_str("oidcScopes")]);
        if value.is_empty() {
            DEFAULT_SCOPES.to_string()
        } else {
            value
        }
    };
    let client_secret = match body.get("clientSecret") {
        Some(Value::String(value)) => value.trim().to_string(),
        _ => setting_str("oidcClientSecret").trim().to_string(),
    };

    if issuer_url.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Issuer URL is required",
        ));
    }
    if client_id.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "Client ID is required"));
    }

    let discovery = match fetch_oidc_discovery(state, &issuer_url).await {
        Ok(document) => document,
        Err(message) => return Ok(json_error(StatusCode::INTERNAL_SERVER_ERROR, &message)),
    };
    let origin = public_origin(headers, uri);
    let redirect_uri = format!("{origin}/api/auth/oidc/callback");
    let secret_probe = match probe_oidc_client_secret(
        state,
        discovery_string(&discovery, "token_endpoint"),
        &client_id,
        &client_secret,
        &redirect_uri,
    )
    .await
    {
        Ok(probe) => probe,
        Err(message) => return Ok(json_error(StatusCode::INTERNAL_SERVER_ERROR, &message)),
    };

    let base = json!({
        "discoveryOk": true,
        "clientSecretTested": secret_probe.get("tested").cloned().unwrap_or(Value::Bool(false)),
        "clientSecretValid": secret_probe.get("valid").cloned().unwrap_or(Value::Null),
        "issuerUrl": issuer_url,
        "clientId": client_id,
        "scopes": scopes,
        "redirectUri": redirect_uri,
        "authorizationEndpoint": discovery_string(&discovery, "authorization_endpoint"),
        "tokenEndpoint": discovery_string(&discovery, "token_endpoint"),
        "jwksUri": discovery_string(&discovery, "jwks_uri"),
    });
    let mut response = base.as_object().cloned().unwrap_or_default();

    let tested = secret_probe.get("tested").and_then(Value::as_bool) == Some(true);
    let valid_is_false = secret_probe.get("valid").and_then(Value::as_bool) == Some(false);
    if tested && valid_is_false {
        let probe_message = secret_probe
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        response.insert("ok".into(), Value::Bool(false));
        response.insert(
            "error".into(),
            Value::String(format!(
                "Discovery loaded, but the client secret is not valid: {probe_message}"
            )),
        );
    } else {
        response.insert("ok".into(), Value::Bool(true));
        if let Some(message) = secret_probe.get("message").cloned() {
            response.insert("message".into(), message);
        }
    }

    Ok(json_response(StatusCode::OK, Value::Object(response)))
}

fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(serde_json::to_vec(&value).unwrap_or_default()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, json!({ "error": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = create_pkce_pair();
        assert_eq!(verifier.len(), 43); // 32 bytes → 43 base64url chars
        assert_eq!(challenge.len(), 43);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn build_authorization_url_contains_all_params_in_call_order() {
        let url = build_authorization_url(
            "https://login.example.com/authorize",
            "client-123",
            "https://myapp.example.com/callback",
            "openid profile email",
            "state-abc",
            "nonce-xyz",
            "challenge-123",
        )
        .unwrap();

        assert!(url.starts_with("https://login.example.com/authorize?"));
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], "client-123");
        assert_eq!(params["redirect_uri"], "https://myapp.example.com/callback");
        assert_eq!(params["scope"], "openid profile email");
        assert_eq!(params["state"], "state-abc");
        assert_eq!(params["nonce"], "nonce-xyz");
        assert_eq!(params["code_challenge"], "challenge-123");
        assert_eq!(params["code_challenge_method"], "S256");
    }

    #[test]
    fn classify_secret_probe_matches_upstream_contract() {
        // Invalid client → tested + invalid
        assert_eq!(
            classify_secret_probe(401, &json!({"error": "invalid_client"})),
            json!({"tested": true, "valid": false, "message": "Client secret is not valid."})
        );
        // Unauthorized client → tested + invalid
        assert_eq!(
            classify_secret_probe(401, &json!({"error": "unauthorized_client"})),
            json!({"tested": true, "valid": false, "message": "Client secret is not valid."})
        );
        // Client mismatch in description → invalid
        assert_eq!(
            classify_secret_probe(401, &json!({"error_description": "client mismatch"})),
            json!({"tested": true, "valid": false, "message": "client mismatch"})
        );
        // Invalid grant → tested + valid (accepts)
        assert_eq!(
            classify_secret_probe(400, &json!({"error": "invalid_grant"})),
            json!({"tested": true, "valid": true, "message": "Client secret was accepted; the token exchange failed only because the test authorization code is invalid."})
        );
        // Invalid code → valid
        assert_eq!(
            classify_secret_probe(400, &json!({"error": "invalid_code"})),
            json!({"tested": true, "valid": true, "message": "Client secret was accepted; the token exchange failed only because the test authorization code is invalid."})
        );
        // Description contains "grant" → valid
        assert_eq!(
            classify_secret_probe(400, &json!({"error_description": "grant expired"})),
            json!({"tested": true, "valid": true, "message": "Client secret was accepted; the token exchange failed only because the test authorization code is invalid."})
        );
        // Description contains "code" → valid
        assert_eq!(
            classify_secret_probe(400, &json!({"error_description": "code already used"})),
            json!({"tested": true, "valid": true, "message": "Client secret was accepted; the token exchange failed only because the test authorization code is invalid."})
        );
        // HTTP 200 → tested + valid (secret was accepted)
        assert_eq!(
            classify_secret_probe(200, &json!({"error": "invalid_grant"})),
            json!({"tested": true, "valid": true, "message": "Client secret was accepted by the token endpoint."})
        );
        // Unrelated error → tested + inconclusive
        let result = classify_secret_probe(500, &json!({"error": "server_error"}));
        assert_eq!(result["tested"], json!(true));
        assert_eq!(result["valid"], json!(null));
        assert!(result["message"]
            .as_str()
            .unwrap()
            .contains("Token endpoint responded with 500"));
        // Note: "empty secret → skipped" is handled by the caller before
        // making the HTTP request, so classify_secret_probe never sees it.
    }

    #[test]
    fn get_oidc_runtime_config_returns_none_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: dir.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        assert!(
            get_oidc_runtime_config(&state).unwrap().is_none(),
            "default settings have no OIDC config"
        );
    }

    #[test]
    fn get_oidc_runtime_config_reads_settings() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.update_settings(json!({
            "authMode": "both",
            "oidcIssuerUrl": "https://sso.example.com/",
            "oidcClientId": "client-123",
            "oidcClientSecret": "secret-abc",
            "oidcScopes": "openid email",
            "oidcLoginLabel": "Sign in with SSO"
        }))
        .unwrap();
        let config = crate::config::Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: dir.path().to_path_buf(),
            db_path: db_path.clone(),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let state = crate::state::AppState::new(config, db).unwrap();
        let cfg = get_oidc_runtime_config(&state).unwrap().unwrap();
        assert_eq!(cfg.issuer_url, "https://sso.example.com");
        assert_eq!(cfg.client_id, "client-123");
        assert_eq!(cfg.client_secret, "secret-abc");
        assert_eq!(cfg.scopes, "openid email");
        assert_eq!(cfg.login_label, "Sign in with SSO");
    }

    #[test]
    fn pick_oidc_display_name_uses_preferred_username() {
        assert_eq!(
            pick_oidc_display_name(&json!({"preferred_username": "alice"})),
            "alice"
        );
        assert_eq!(pick_oidc_display_name(&json!({"email": "a@b.c"})), "a@b.c");
        assert_eq!(pick_oidc_display_name(&json!({"name": "Alice"})), "Alice");
        assert_eq!(pick_oidc_display_name(&json!({"sub": "u-123"})), "u-123");
        assert_eq!(pick_oidc_display_name(&json!({})), "OIDC user");
    }

    #[test]
    fn pick_oidc_email_returns_email_when_present() {
        assert_eq!(
            pick_oidc_email(&json!({"email": "a@b.c"})),
            Some("a@b.c".to_string())
        );
        assert_eq!(pick_oidc_email(&json!({})), None);
    }
}
