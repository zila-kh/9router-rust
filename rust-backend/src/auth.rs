use crate::{error::AppError, state::AppState};
use axum::http::{HeaderMap, HeaderValue};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::{IpAddr, SocketAddr},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const CLI_TOKEN_HEADER: &str = "x-9r-cli-token";
const CLI_TOKEN_SALT: &str = "9r-cli-auth";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub authenticated: bool,
    pub iat: u64,
    pub exp: u64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn extract_api_key(headers: &HeaderMap, query_key: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(v) = v.strip_prefix("Bearer ") {
            return Some(v.to_string());
        }
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(v.to_string());
        }
    }
    query_key.map(str::to_string)
}

pub fn is_loopback(peer: SocketAddr) -> bool {
    is_loopback_ip(peer.ip())
}
pub fn is_loopback_ip(ip: IpAddr) -> bool {
    ip.is_loopback() || is_ipv4_mapped_loopback(ip)
}
fn is_ipv4_mapped_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|v4| v4.is_loopback())
            .unwrap_or(false),
        _ => false,
    }
}

fn jwt_secret(state: &AppState) -> Result<Vec<u8>, AppError> {
    if let Ok(v) = std::env::var("JWT_SECRET") {
        return Ok(v.into_bytes());
    }
    let path = state.config.data_dir.join("jwt-secret");
    if let Ok(s) = fs::read_to_string(&path) {
        let s = s.trim();
        if !s.is_empty() {
            return Ok(s.as_bytes().to_vec());
        }
    }
    fs::create_dir_all(&state.config.data_dir).map_err(|e| AppError::Internal(e.into()))?;
    let mut bytes = [0u8; 32];
    use rand::RngCore;
    rand::rng().fill_bytes(&mut bytes);
    let generated = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    fs::write(&path, &generated).map_err(|e| AppError::Internal(e.into()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| AppError::Internal(e.into()))?;
    }
    Ok(generated.into_bytes())
}

pub fn create_session_token(
    state: &AppState,
    mut extra: serde_json::Map<String, Value>,
) -> Result<String, AppError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    extra.insert("authenticated".into(), Value::Bool(true));
    extra.insert("iat".into(), Value::from(now));
    extra.insert("exp".into(), Value::from(now + 86400));
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Value::Object(extra))?);
    let input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(&jwt_secret(state)?)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e.to_string())))?;
    mac.update(input.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{input}.{sig}"))
}

pub fn verify_session_token(state: &AppState, token: &str) -> bool {
    let mut p = token.split('.');
    let (Some(h), Some(b), Some(s), None) = (p.next(), p.next(), p.next(), p.next()) else {
        return false;
    };
    let input = format!("{h}.{b}");
    let Ok(sig) = URL_SAFE_NO_PAD.decode(s) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&jwt_secret(state).unwrap_or_default()) else {
        return false;
    };
    mac.update(input.as_bytes());
    let expected = mac.finalize().into_bytes();
    if expected.as_slice().ct_eq(sig.as_slice()).unwrap_u8() != 1 {
        return false;
    }
    let Ok(payload) = URL_SAFE_NO_PAD.decode(b) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&payload) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    v.get("authenticated").and_then(Value::as_bool) == Some(true)
        && v.get("exp")
            .and_then(Value::as_u64)
            .map(|e| e > now)
            .unwrap_or(false)
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let mut p = part.trim().splitn(2, '=');
            let k = p.next()?;
            let v = p.next()?;
            if k == name {
                Some(v.to_string())
            } else {
                None
            }
        })
}

pub fn has_valid_cli_token(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(supplied) = headers
        .get(CLI_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    let Some(raw_machine_id) = read_nonempty(state.config.data_dir.join("machine-id")) else {
        return false;
    };
    let Some(cli_secret) = read_nonempty(
        state
            .config
            .data_dir
            .join("auth")
            .join("cli-secret"),
    ) else {
        return false;
    };
    let expected = derive_cli_token(&raw_machine_id, &cli_secret);
    expected
        .as_bytes()
        .ct_eq(supplied.as_bytes())
        .unwrap_u8()
        == 1
}

fn read_nonempty(path: std::path::PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn derive_cli_token(raw_machine_id: &str, cli_secret: &str) -> String {
    let digest = Sha256::digest(format!(
        "{}{}{}",
        raw_machine_id.trim(),
        CLI_TOKEN_SALT,
        cli_secret.trim()
    ));
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn has_valid_dashboard_session(state: &AppState, headers: &HeaderMap) -> bool {
    cookie(headers, "auth_token")
        .map(|token| verify_session_token(state, &token))
        .unwrap_or(false)
}

pub fn dashboard_authenticated(state: &AppState, headers: &HeaderMap) -> Result<bool, AppError> {
    let settings = state.db.settings()?;
    if settings.get("requireLogin").and_then(Value::as_bool) == Some(false) {
        return Ok(true);
    }
    Ok(has_valid_dashboard_session(state, headers))
}

pub fn require_dashboard(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if has_valid_cli_token(state, headers) || dashboard_authenticated(state, headers)? {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

pub fn require_llm(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
    query_key: Option<&str>,
) -> Result<(), AppError> {
    if is_loopback(peer) {
        return Ok(());
    }
    let settings = state.db.settings()?;
    if settings.get("requireApiKey").and_then(Value::as_bool) == Some(false) {
        return Ok(());
    }
    let Some(key) = extract_api_key(headers, query_key) else {
        return Err(AppError::Unauthorized);
    };
    if state.db.validate_api_key(&key)? {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

pub fn verify_password(state: &AppState, password: &str) -> Result<bool, AppError> {
    if password.is_empty() {
        return Ok(false);
    }
    let settings = state.db.settings()?;
    if let Some(hash) = settings.get("password").and_then(Value::as_str) {
        return Ok(bcrypt::verify(password, hash).unwrap_or(false));
    }
    Ok(password == std::env::var("INITIAL_PASSWORD").unwrap_or_else(|_| "123456".into()))
}

pub fn secure_cookie(headers: &HeaderMap) -> bool {
    std::env::var("AUTH_COOKIE_SECURE").ok().as_deref() == Some("true")
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            == Some("https")
}

pub fn session_cookie_header(headers: &HeaderMap, token: &str) -> HeaderValue {
    let secure = if secure_cookie(headers) {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "auth_token={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400{secure}"
    ))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_token_matches_upstream_shape() {
        let token = derive_cli_token("machine-123", "secret-456");
        assert_eq!(token.len(), 16);
        assert_eq!(token, derive_cli_token("machine-123", "secret-456"));
        assert_ne!(token, derive_cli_token("machine-124", "secret-456"));
        assert_ne!(token, derive_cli_token("machine-123", "secret-457"));
    }
}
