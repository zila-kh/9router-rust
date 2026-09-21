use crate::{error::AppError, state::AppState};
use axum::http::{header, HeaderMap, HeaderValue};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    net::{IpAddr, SocketAddr},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const CLI_TOKEN_HEADER: &str = "x-9r-cli-token";
const CLI_TOKEN_SALT: &str = "9r-cli-auth";
const FORWARDED_PEER_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "cf-connecting-ip",
    "true-client-ip",
    "x-client-ip",
    "x-cluster-client-ip",
    "x-9r-via-proxy",
];

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub authenticated: bool,
    pub iat: u64,
    pub exp: u64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn extract_api_key(headers: &HeaderMap, query_key: Option<&str>) -> Option<String> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let mut parts = value.split_whitespace();
        if let (Some(scheme), Some(token), None) = (parts.next(), parts.next(), parts.next()) {
            if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    query_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

fn strip_ipv6_brackets(value: &str) -> &str {
    value
        .strip_prefix("[")
        .and_then(|value| value.strip_suffix("]"))
        .unwrap_or(value)
}

pub fn is_direct_loopback_request(peer: SocketAddr, headers: &HeaderMap) -> bool {
    if !is_loopback(peer)
        || FORWARDED_PEER_HEADERS
            .iter()
            .any(|name| headers.contains_key(*name))
    {
        return false;
    }

    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let Some(host) = origin.host_str() else {
        return false;
    };
    let host = strip_ipv6_brackets(host);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().map(is_loopback_ip).unwrap_or(false)
}

pub fn rate_limit_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let trust_proxy = std::env::var("TRUST_PROXY").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    });
    if trust_proxy && is_loopback(peer) {
        for name in ["x-forwarded-for", "x-real-ip"] {
            let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) else {
                continue;
            };
            let candidate = value.split(',').next().map(str::trim).unwrap_or("");
            if let Some(ip) = parse_ip_candidate(candidate) {
                return ip;
            }
        }
    }
    peer.ip()
}

fn parse_ip_candidate(value: &str) -> Option<IpAddr> {
    value
        .parse::<IpAddr>()
        .ok()
        .or_else(|| value.parse::<SocketAddr>().ok().map(|address| address.ip()))
        .or_else(|| strip_ipv6_brackets(value).parse::<IpAddr>().ok())
}

fn jwt_secret(state: &AppState) -> Result<Vec<u8>, AppError> {
    match std::env::var("JWT_SECRET") {
        Ok(value) if !value.trim().is_empty() => return Ok(value.trim().as_bytes().to_vec()),
        Ok(_) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "JWT_SECRET must not be empty"
            )))
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(error) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "JWT_SECRET is not valid Unicode: {error}"
            )))
        }
    }

    let path = state.config.data_dir.join("jwt-secret");
    if let Some(secret) = read_persisted_secret(&path)? {
        return Ok(secret);
    }

    fs::create_dir_all(&state.config.data_dir).map_err(|error| AppError::Internal(error.into()))?;
    let mut bytes = [0u8; 32];
    use rand::RngCore;
    rand::rng().fill_bytes(&mut bytes);
    let generated = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(generated.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|error| AppError::Internal(error.into()))?;
            Ok(generated.into_bytes())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_persisted_secret(&path)?
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("jwt-secret was created empty"))),
        Err(error) => Err(AppError::Internal(error.into())),
    }
}

fn read_persisted_secret(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::Internal(error.into())),
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "persisted jwt-secret is empty"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(path)
            .map_err(|error| AppError::Internal(error.into()))?
            .permissions();
        if permissions.mode() & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|error| AppError::Internal(error.into()))?;
        }
    }
    Ok(Some(value.as_bytes().to_vec()))
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
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"alg":"HS256","typ":"JWT"}))?);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Value::Object(extra))?);
    let input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(&jwt_secret(state)?)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error.to_string())))?;
    mac.update(input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{input}.{signature}"))
}

fn valid_session_header(encoded: &str) -> bool {
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(encoded) else {
        return false;
    };
    let Ok(header) = serde_json::from_slice::<Value>(&decoded) else {
        return false;
    };
    header.get("alg").and_then(Value::as_str) == Some("HS256")
        && header
            .get("typ")
            .and_then(Value::as_str)
            .is_none_or(|value| value == "JWT")
        && header.get("crit").is_none()
}

pub fn verify_session_token(state: &AppState, token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if !valid_session_header(header) {
        return false;
    }
    let input = format!("{header}.{payload}");
    let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(secret) = jwt_secret(state) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&secret) else {
        return false;
    };
    mac.update(input.as_bytes());
    let expected = mac.finalize().into_bytes();
    if expected.as_slice().ct_eq(signature.as_slice()).unwrap_u8() != 1 {
        return false;
    }
    let Ok(payload) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    value.get("authenticated").and_then(Value::as_bool) == Some(true)
        && value
            .get("iat")
            .and_then(Value::as_u64)
            .is_some_and(|issued| issued <= now.saturating_add(300))
        && value
            .get("exp")
            .and_then(Value::as_u64)
            .is_some_and(|expires| expires > now)
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
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
    let Some(cli_secret) = read_nonempty(state.config.data_dir.join("auth").join("cli-secret"))
    else {
        return false;
    };
    let expected = derive_cli_token(&raw_machine_id, &cli_secret);
    expected.as_bytes().ct_eq(supplied.as_bytes()).unwrap_u8() == 1
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

/// Free-tier administration is always restricted to an authenticated dashboard
/// session or the local CLI token, even when dashboard login is disabled.
pub fn require_dashboard_admin(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if has_valid_cli_token(state, headers) || has_valid_dashboard_session(state, headers) {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

#[allow(dead_code)]
pub fn require_llm(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
    query_key: Option<&str>,
) -> Result<(), AppError> {
    // Dashboard-owned clients (including Playground Chat) use the same public
    // inference routes as API consumers. A valid HttpOnly dashboard session is
    // therefore an explicit credential here, while unauthenticated local calls
    // still follow the normal requireApiKey policy below.
    if has_valid_cli_token(state, headers) || has_valid_dashboard_session(state, headers) {
        return Ok(());
    }
    let settings = state.db.settings()?;
    let direct_local = is_direct_loopback_request(peer, headers);
    let require_key = !direct_local
        || settings
            .get("requireApiKey")
            .and_then(Value::as_bool)
            .unwrap_or(true);
    if !require_key {
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
    if let Some(hash) = settings
        .get("password")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    {
        return Ok(bcrypt::verify(password, hash).unwrap_or(false));
    }
    let initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "123456".to_string());
    Ok(password == initial)
}

pub fn secure_cookie(headers: &HeaderMap) -> bool {
    std::env::var("AUTH_COOKIE_SECURE")
        .ok()
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
        || headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
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
    .expect("generated session cookie contains only valid header characters")
}

pub fn clear_session_cookie_header(headers: &HeaderMap) -> HeaderValue {
    let secure = if secure_cookie(headers) {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "auth_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"
    ))
    .expect("generated clear-session cookie contains only valid header characters")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(value: &str) -> SocketAddr {
        value.parse().expect("valid test socket address")
    }

    #[test]
    fn cli_token_matches_upstream_shape() {
        let token = derive_cli_token("machine-123", "secret-456");
        assert_eq!(token.len(), 16);
        assert_eq!(token, derive_cli_token("machine-123", "secret-456"));
        assert_ne!(token, derive_cli_token("machine-124", "secret-456"));
        assert_ne!(token, derive_cli_token("machine-123", "secret-457"));
    }

    #[test]
    fn api_key_parser_accepts_case_insensitive_bearer_and_trims_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer test-token"),
        );
        assert_eq!(
            extract_api_key(&headers, None).as_deref(),
            Some("test-token")
        );

        headers.remove(header::AUTHORIZATION);
        headers.insert("x-api-key", HeaderValue::from_static("  key-123  "));
        assert_eq!(extract_api_key(&headers, None).as_deref(), Some("key-123"));
        assert_eq!(
            extract_api_key(&HeaderMap::new(), Some("  query-key ")).as_deref(),
            Some("query-key")
        );
    }

    #[test]
    fn direct_loopback_rejects_proxy_hops_and_remote_origins() {
        let headers = HeaderMap::new();
        assert!(is_direct_loopback_request(peer("127.0.0.1:1234"), &headers));
        assert!(!is_direct_loopback_request(
            peer("192.0.2.20:1234"),
            &headers
        ));

        let mut forwarded = HeaderMap::new();
        forwarded.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.20"));
        assert!(!is_direct_loopback_request(
            peer("127.0.0.1:1234"),
            &forwarded
        ));

        let mut origin = HeaderMap::new();
        origin.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://router.example.com"),
        );
        assert!(!is_direct_loopback_request(peer("127.0.0.1:1234"), &origin));

        let mut ipv6_origin = HeaderMap::new();
        ipv6_origin.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://[::1]:20128"),
        );
        assert!(is_direct_loopback_request(peer("[::1]:1234"), &ipv6_origin));
    }

    #[test]
    fn session_header_requires_hs256_jwt() {
        let valid = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let wrong_algorithm = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let critical = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","crit":["exp"]}"#);
        assert!(valid_session_header(&valid));
        assert!(!valid_session_header(&wrong_algorithm));
        assert!(!valid_session_header(&critical));
    }

    #[test]
    fn admin_gate_requires_a_real_session_even_when_dashboard_login_is_disabled() {
        let temp = tempfile::tempdir().expect("temporary data directory");
        let config = crate::config::Config {
            listen: "127.0.0.1:20130".parse().expect("valid listen address"),
            ui_origin: "http://127.0.0.1:20129".into(),
            data_dir: temp.path().to_path_buf(),
            db_path: temp.path().join("data.sqlite"),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let db = crate::db::Db::open(&config.db_path).expect("test database");
        db.update_settings(json!({"requireLogin": false}))
            .expect("disable general dashboard login");
        let state = AppState::new(config, db).expect("test app state");

        assert!(require_dashboard_admin(&state, &HeaderMap::new()).is_err());

        let token =
            create_session_token(&state, serde_json::Map::new()).expect("create dashboard session");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("auth_token={token}")).expect("valid session cookie"),
        );
        assert!(require_dashboard_admin(&state, &headers).is_ok());
    }

    #[test]
    fn llm_gate_accepts_a_valid_dashboard_session() {
        let temp = tempfile::tempdir().expect("temporary data directory");
        let config = crate::config::Config {
            listen: "127.0.0.1:20130".parse().expect("valid listen address"),
            ui_origin: "http://127.0.0.1:20129".into(),
            data_dir: temp.path().to_path_buf(),
            db_path: temp.path().join("data.sqlite"),
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(200),
            stream_stall_timeout: std::time::Duration::from_secs(360),
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: false,
        };
        let db = crate::db::Db::open(&config.db_path).expect("test database");
        db.update_settings(json!({"requireLogin": true, "requireApiKey": true}))
            .expect("enable authentication");
        let state = AppState::new(config, db).expect("test app state");

        assert!(require_llm(&state, &HeaderMap::new(), peer("127.0.0.1:1234"), None).is_err());

        let token =
            create_session_token(&state, serde_json::Map::new()).expect("create dashboard session");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("auth_token={token}")).expect("valid session cookie"),
        );
        assert!(require_llm(&state, &headers, peer("127.0.0.1:1234"), None).is_ok());
    }
}
