mod app;
mod auth;
mod config;
mod db;
mod error;
mod gateway;
mod legacy_proxy;
mod management;
mod media;
mod protocol;
mod providers;
mod special;
mod state;
mod streaming;
mod translate;
mod ui_proxy;

use std::net::SocketAddr;
use config::Config;
use db::Db;
use state::AppState;

#[tokio::main]
async fn main()->anyhow::Result<()>{
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_|"nine_router_rs=info,9router=info".into())).init();
    let cfg=Config::from_env();let db=Db::open(&cfg.db_path)?;let addr:SocketAddr=cfg.listen;let state=AppState::new(cfg,db)?;let listener=tokio::net::TcpListener::bind(addr).await?;tracing::info!(%addr,version="1.0.1",ui=%state.config.ui_origin,"9Router Rust backend listening");
    axum::serve(listener,app::router(state).into_make_service_with_connect_info::<SocketAddr>()).await?;Ok(())
}
