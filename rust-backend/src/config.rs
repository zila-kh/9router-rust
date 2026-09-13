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
    pub fn from_env() -> Self {
        let host: IpAddr = env::var("NINEROUTER_HOST")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let port = env::var("PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(20128);
        let data_dir = env::var_os("NINEROUTER_DATA_DIR")
            .or_else(|| env::var_os("DATA_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);
        let db_path = env::var_os("NINEROUTER_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("db").join("data.sqlite"));
        let ui_origin =
            env::var("NINEROUTER_UI_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:20129".into());
        let upstream_timeout_secs = env::var("NINEROUTER_UPSTREAM_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(600);
        let ui_only_header_secret = env::var("NINEROUTER_UI_SECRET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let legacy_backend_origin = if env_flag("NINEROUTER_DISABLE_LEGACY_BRIDGE", false) {
            None
        } else {
            env::var("NINEROUTER_LEGACY_BACKEND_ORIGIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let compat_api_enabled = env_flag("NINEROUTER_COMPAT_API", false);
        Self {
            listen: SocketAddr::new(host, port),
            ui_origin,
            data_dir,
            db_path,
            upstream_timeout_secs,
            ui_only_header_secret,
            legacy_backend_origin,
            compat_api_enabled,
        }
    }
}

fn env_flag(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
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
