use std::{sync::Arc, time::Duration};
use reqwest::Client;
use crate::{config::Config, db::Db};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub http: Client,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(config.upstream_timeout_secs))
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .user_agent("9router-rust/1.0.1")
            .build()?;
        Ok(Self { config: Arc::new(config), db, http })
    }
}
