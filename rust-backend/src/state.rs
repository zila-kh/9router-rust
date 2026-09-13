use crate::{config::Config, db::Db};
use reqwest::{redirect::Policy, Client};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub http: Client,
    pub proxy_http: Client,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(config.upstream_timeout_secs);
        let http = Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .user_agent(concat!("9router-rust/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let proxy_http = Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .redirect(Policy::none())
            .user_agent(concat!(
                "9router-rust-proxy/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            db,
            http,
            proxy_http,
        })
    }
}
