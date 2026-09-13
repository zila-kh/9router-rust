use anyhow::{bail, Context};
use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub ui_origin: String,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub upstream_timeout_secs: u64,
    pub ui_only_header_secret: String,
    pub legacy_backend_origin: Option<String>,
    pub compat_api_enabled: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let host: IpAddr = match env::var("NINEROUTER_HOST") {
            Ok(value) => value
                .parse()
                .with_context(|| format!("invalid NINEROUTER_HOST value: {value}"))?,
            Err(env::VarError::NotPresent) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Err(error) => return Err(error).context("NINEROUTER_HOST is not valid Unicode"),
        };
        let port = match env::var("PORT") {
            Ok(value) => value
                .parse::<u16>()
                .with_context(|| format!("invalid PORT value: {value}"))?,
            Err(env::VarError::NotPresent) => 20128,
            Err(error) => return Err(error).context("PORT is not valid Unicode"),
        };
        let data_dir = env::var_os("NINEROUTER_DATA_DIR")
            .or_else(|| env::var_os("DATA_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);
        let db_path = env::var_os("NINEROUTER_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("db").join("data.sqlite"));
        let ui_origin = validate_loopback_origin(
            "NINEROUTER_UI_ORIGIN",
            env::var("NINEROUTER_UI_ORIGIN")
                .unwrap_or_else(|_| "http://127.0.0.1:20129".into()),
        )?;
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
        let ui_only_header_secret = env::var("NINEROUTER_UI_SECRET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let legacy_backend_origin = if env_flag("NINEROUTER_DISABLE_LEGACY_BRIDGE", false)? {
            None
        } else {
            env::var("NINEROUTER_LEGACY_BACKEND_ORIGIN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| validate_loopback_origin("NINEROUTER_LEGACY_BACKEND_ORIGIN", value))
                .transpose()?
        };
        let compat_api_enabled = env_flag("NINEROUTER_COMPAT_API", false)?;
        Ok(Self {
            listen: SocketAddr::new(host, port),
            ui_origin,
            data_dir,
            db_path,
            upstream_timeout_secs,
            ui_only_header_secret,
            legacy_backend_origin,
            compat_api_enabled,
        })
    }
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
}
