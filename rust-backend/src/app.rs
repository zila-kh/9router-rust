use crate::{error::AppError, gateway, management, media, state::AppState, ui_proxy};
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, Response},
    response::IntoResponse,
    Router,
};
use std::net::SocketAddr;

pub fn router(state: AppState) -> Router {
    Router::new().fallback(entry).with_state(state)
}

async fn entry(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    let is_backend =
        media::is_media_path(&path) || gateway::is_llm_path(&path) || path.starts_with("/api/");
    let result: Result<Response<Body>, AppError> = if media::is_media_path(&path) {
        media::handle(state, peer, req).await
    } else if gateway::is_llm_path(&path) {
        gateway::handle(state, peer, req).await
    } else if path.starts_with("/api/") {
        management::handle(state, peer, req).await
    } else {
        ui_proxy::handle(state, peer, req).await
    };
    match result {
        Ok(mut r) => {
            if is_backend && !r.headers().contains_key("x-9router-runtime") {
                r.headers_mut().insert(
                    "x-9router-runtime",
                    axum::http::HeaderValue::from_static("rust"),
                );
            }
            r
        }
        Err(e) => e.into_response(),
    }
}
