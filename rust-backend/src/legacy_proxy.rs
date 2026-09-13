use std::net::SocketAddr;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri},
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::{error::AppError, state::AppState};

pub async fn proxy_buffered(
    state: &AppState,
    peer: SocketAddr,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    let Some(origin) = state.config.legacy_backend_origin.as_deref() else {
        return Err(AppError::NotFound("legacy bridge disabled".into()));
    };
    let base = origin.trim_end_matches('/');
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{base}{pq}");
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|e| AppError::Internal(e.into()))?;
    let mut out_headers = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let name = k.as_str().to_ascii_lowercase();
        if is_hop(&name) || name == "host" || name == "content-length" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out_headers.append(n, v);
        }
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&peer.ip().to_string()) {
        out_headers.insert(reqwest::header::HeaderName::from_static("x-9r-real-ip"), v);
    }
    out_headers.insert(
        reqwest::header::HeaderName::from_static("x-9r-rust-legacy-bridge"),
        reqwest::header::HeaderValue::from_static("1.0.1"),
    );
    let response = state
        .proxy_http
        .request(method, &url)
        .headers(out_headers)
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::Upstream(format!("legacy bridge unavailable at {origin}: {e}")))?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let src = response.headers().clone();
    let stream = response
        .bytes_stream()
        .map(|x| x.map_err(std::io::Error::other));
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    copy_headers(&src, out.headers_mut());
    out.headers_mut().insert(
        HeaderName::from_static("x-9router-runtime"),
        HeaderValue::from_static("legacy-bridge"),
    );
    Ok(out)
}

fn copy_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (k, v) in src {
        let n = k.as_str().to_ascii_lowercase();
        if is_hop(&n) || n == "content-length" {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            dst.append(k, v);
        }
    }
}

fn is_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
