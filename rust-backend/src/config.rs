use anyhow::{bail, Context};
use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub ui_origin: String,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub upstream_timeout_secs: u64,
    /// How long a stream may go without its first chunk (provider prefill).
    pub stream_first_chunk_timeout: Duration,
    /// How long a streaming response may go without any chunk once it started.
    pub stream_stall_timeout: Duration,
    #[allow(dead_code)]
    pub ui_only_header_secret: String,
    pub legacy_backend_origin: Option<String>,
    pub compat_api_enabled: bool,
}

/// Read a millisecond timeout from the first environment name that is set.
fn stream_timeout(names: &[&str], default_ms: u64) -> anyhow::Result<Duration> {
    for name in names {
        match env::var(name) {
            Ok(value) => {
                let ms: u64 = value
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid {name} value: {value}"))?;
                if ms == 0 {
                    anyhow::bail!("{name} must be greater than zero");
                }
                return Ok(Duration::from_millis(ms));
            }
            Err(env::VarError::NotPresent) => continue,
            Err(error) => return Err(error).context(format!("{name} is not valid Unicode")),
        }
    }
    Ok(Duration::from_millis(default_ms))
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let host: IpAddr = match env::var("NINEROUTER_HOST") {
            Ok(value) => value
                .parse()
                .with_context(|| format!("invalid NINEROUTER_HOST value: {value}"))?,
            // Safe direct-binary default. Public exposure must be an explicit
            // deployment choice through NINEROUTER_HOST.
            Err(env::VarError::NotPresent) => default_listen_host(),
            Err(error) => return Err(error).context("NINEROUTER_HOST is not valid Unicode"),
        };
        let port = match env::var("PORT") {
            Ok(value) => parse_port("PORT", &value)?,
            Err(env::VarError::NotPresent) => 20130,
            Err(error) => return Err(error).context("PORT is not valid Unicode"),
        };
        let data_dir = env::var_os("NINEROUTER_DATA_DIR")
            .or_else(|| env::var_os("DATA_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);
        let db_path = env::var_os("NINEROUTER_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("db").join("data.sqlite"));
        let ui_origin_value = match env::var("NINEROUTER_UI_ORIGIN") {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => "http://127.0.0.1:20129".into(),
            Err(error) => return Err(error).context("NINEROUTER_UI_ORIGIN is not valid Unicode"),
        };
        let ui_origin = validate_loopback_origin("NINEROUTER_UI_ORIGIN", ui_origin_value)?;
        let upstream_timeout_secs = match env::var("NINEROUTER_UPSTREAM_TIMEOUT_SECS") {
            Ok(value) => value.parse::<u64>().with_context(|| {
                format!("invalid NINEROUTER_UPSTREAM_TIMEOUT_SECS value: {value}")
            })?,
            Err(env::VarError::NotPresent) => 600,
            Err(error) => {
                return Err(error).context("NINEROUTER_UPSTREAM_TIMEOUT_SECS is not valid Unicode")
            }
        };
        if upstream_timeout_secs == 0 {
            bail!("NINEROUTER_UPSTREAM_TIMEOUT_SECS must be greater than zero");
        }
        // Stream inactivity limits, matching upstream's STREAM_*_TIMEOUT_MS
        // defaults (200s prefill, 360s inter-chunk). Both names are accepted so an
        // existing upstream deployment's environment keeps working here.
        let stream_first_chunk_timeout = stream_timeout(
            &[
                "NINEROUTER_STREAM_FIRST_CHUNK_TIMEOUT_MS",
                "STREAM_FIRST_CHUNK_TIMEOUT_MS",
            ],
            200_000,
        )?;
        let stream_stall_timeout = stream_timeout(
            &[
                "NINEROUTER_STREAM_STALL_TIMEOUT_MS",
                "STREAM_STALL_TIMEOUT_MS",
            ],
            360_000,
        )?;
        let compat_api_enabled = env_flag("NINEROUTER_COMPAT_API", false)?;
        let ui_only_header_secret = match env::var("NINEROUTER_UI_SECRET") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            Ok(_) => bail!("NINEROUTER_UI_SECRET must not be empty"),
            Err(env::VarError::NotPresent) if compat_api_enabled => {
                bail!("NINEROUTER_UI_SECRET is required when NINEROUTER_COMPAT_API is enabled")
            }
            Err(env::VarError::NotPresent) => uuid::Uuid::new_v4().to_string(),
            Err(error) => return Err(error).context("NINEROUTER_UI_SECRET is not valid Unicode"),
        };
        let legacy_backend_origin = if env_flag("NINEROUTER_DISABLE_LEGACY_BRIDGE", false)? {
            None
        } else {
            match env::var("NINEROUTER_LEGACY_BACKEND_ORIGIN") {
                Ok(value) if value.trim().is_empty() => None,
                Ok(value) => Some(validate_loopback_origin(
                    "NINEROUTER_LEGACY_BACKEND_ORIGIN",
                    value,
                )?),
                Err(env::VarError::NotPresent) => None,
                Err(error) => {
                    return Err(error)
                        .context("NINEROUTER_LEGACY_BACKEND_ORIGIN is not valid Unicode")
                }
            }
        };
        Ok(Self {
            listen: SocketAddr::new(host, port),
            ui_origin,
            data_dir,
            db_path,
            upstream_timeout_secs,
            stream_first_chunk_timeout,
            stream_stall_timeout,
            ui_only_header_secret,
            legacy_backend_origin,
            compat_api_enabled,
        })
    }
}

fn parse_port(name: &str, value: &str) -> anyhow::Result<u16> {
    let port = value
        .parse::<u16>()
        .with_context(|| format!("invalid {name} value: {value}"))?;
    if port == 0 {
        bail!("{name} must be between 1 and 65535");
    }
    Ok(port)
}

fn default_listen_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn validate_loopback_origin(name: &str, value: String) -> anyhow::Result<String> {
    let value = value.trim();
    let parsed = url::Url::parse(value).with_context(|| format!("invalid {name}: {value}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("{name} must use http or https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("{name} must not contain embedded credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() || parsed.path() != "/" {
        bail!("{name} must be an origin without a path, query, or fragment");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{name} must include a host"))?;
    if !is_loopback_host(host) {
        bail!("{name} must target localhost or a loopback IP address");
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix("[")
        .and_then(|value| value.strip_suffix("]"))
        .unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| {
            ip.is_loopback()
                || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().map(|v4| v4.is_loopback()).unwrap_or(false))
        })
        .unwrap_or(false)
}

fn env_flag(name: &str, default: bool) -> anyhow::Result<bool> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(default),
        Err(error) => return Err(error).with_context(|| format!("{name} is not valid Unicode")),
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name} must be one of: 1, 0, true, false, yes, no, on, off"),
    }
}

fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .map(|home| home.join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|| PathBuf::from("."))
            .join("9router")
    } else {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".9router")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_origins_are_accepted() {
        assert_eq!(
            validate_loopback_origin("TEST", "http://127.0.0.1:20129/".into()).unwrap(),
            "http://127.0.0.1:20129"
        );
        assert!(validate_loopback_origin("TEST", "http://localhost:20129".into()).is_ok());
        assert!(validate_loopback_origin("TEST", "http://[::1]:20129".into()).is_ok());
        assert!(validate_loopback_origin("TEST", "https://example.com".into()).is_err());
        assert!(validate_loopback_origin("TEST", "http://127.0.0.1:20129/path".into()).is_err());
    }

    #[test]
    fn listener_port_must_not_be_zero() {
        assert_eq!(parse_port("PORT", "20130").unwrap(), 20130);
        assert!(parse_port("PORT", "0").is_err());
        assert!(parse_port("PORT", "65536").is_err());
        assert!(parse_port("PORT", "not-a-port").is_err());
    }

    #[test]
    fn direct_binary_defaults_to_loopback() {
        assert!(default_listen_host().is_loopback());
    }
}
