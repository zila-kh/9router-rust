#![recursion_limit = "512"]
// The port deliberately carries protocol codecs and migration entry points before
// every upstream transport is wired into the public request path. Keep that staged
// code compiled and tested without treating temporary reachability as an error.
#![allow(dead_code)]
// Several compatibility functions mirror upstream wire/database contracts. Their
// signatures and a few mechanically equivalent forms are kept stable while the
// native implementations are completed; all other Clippy warnings remain denied.
#![allow(
    clippy::if_same_then_else,
    clippy::manual_split_once,
    clippy::needless_return,
    clippy::redundant_closure,
    clippy::too_many_arguments
)]

mod app;
mod auth;
mod auto_router;
mod compat_media;
mod compat_proxy;
mod config;
mod db;
mod error;
mod gateway;
mod inference_media;
mod legacy_proxy;
mod login_limiter;
mod management;
mod media;
mod model_catalog;
mod models_mgmt;
mod ollama;
mod protocol;
mod providers;
mod providers_oauth;
mod remaining_infra;
mod request_path;
mod special;
mod state;
mod streaming;
mod translate;
mod ui_proxy;
mod usage_mgmt;
mod voice_catalog;

use anyhow::bail;
use config::Config;
use db::Db;
use serde_json::{json, Value};
use state::AppState;
use std::{env, net::SocketAddr};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nine_router_rs=info,9router=info".into()),
        )
        .init();
    let cfg = Config::from_env()?;
    let db = Db::open(&cfg.db_path)?;
    seed_initial_password(&db)?;
    let addr: SocketAddr = cfg.listen;
    let state = AppState::new(cfg, db)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        %addr,
        version = env!("CARGO_PKG_VERSION"),
        ui = %state.config.ui_origin,
        compat_api = state.config.compat_api_enabled,
        "9Router Rust backend listening"
    );
    axum::serve(
        listener,
        app::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn seed_initial_password(db: &Db) -> anyhow::Result<()> {
    let initial_password = match env::var("INITIAL_PASSWORD") {
        Ok(value) if value.trim().is_empty() => bail!("INITIAL_PASSWORD must not be empty"),
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let settings = db.settings()?;
    let has_stored_password = settings
        .get("password")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if has_stored_password {
        return Ok(());
    }

    let password_hash = bcrypt::hash(initial_password.trim(), 12)?;
    db.update_settings(json!({"password":password_hash}))?;
    Ok(())
}
