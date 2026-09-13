use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Response, StatusCode},
};
use serde_json::{json, Value};

use crate::state::AppState;

const UPSTREAM_VERSION: &str = "0.5.75";
const UPSTREAM_COMMIT: &str = include_str!("../UPSTREAM_COMMIT");

pub fn handle(state: &AppState, method: &Method, path: &str) -> Option<Response<Body>> {
    if method != Method::GET {
        return None;
    }

    let value = match path {
        "/api/health" => json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "runtime": "rust",
            "upstreamSnapshot": UPSTREAM_COMMIT.trim(),
            "upstreamVersion": UPSTREAM_VERSION,
            "compatApiEnabled": state.config.compat_api_enabled,
        }),
        "/api/init" => json!({
            "initialized": true,
            "runtime": "rust",
            "version": env!("CARGO_PKG_VERSION"),
            "upstreamSnapshot": UPSTREAM_COMMIT.trim(),
            "upstreamVersion": UPSTREAM_VERSION,
        }),
        "/api/version" => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "name": "9router-rust",
            "rustBackend": true,
            "upstreamVersion": UPSTREAM_VERSION,
            "upstreamSnapshot": UPSTREAM_COMMIT.trim(),
        }),
        _ => return None,
    };

    Some(json_response(value))
}

fn json_response(value: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}
