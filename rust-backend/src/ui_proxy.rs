use std::net::SocketAddr;

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
};
use futures_util::StreamExt;

use crate::{auth, error::AppError, state::AppState};

const MAX_BODY: usize = 128 * 1024 * 1024;

pub async fn handle(
    state: AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let path = req.uri().path().to_string();
    if path == "/" {
        return redirect("/dashboard");
    }
    if path.starts_with("/dashboard") && !auth::dashboard_authenticated(&state, req.headers())? {
        return redirect("/login");
    }
    proxy(state, peer, req).await
}

async fn proxy(
    state: AppState,
    peer: SocketAddr,
    req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, MAX_BODY)
        .await
        .map_err(|e| AppError::BadRequest(format!("UI request body: {e}")))?;
    let base = state.config.ui_origin.trim_end_matches('/');
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{base}{pq}");
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|e| AppError::Internal(e.into()))?;
    let original_host = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut rb = state.proxy_http.request(method, &url).body(bytes);
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in &parts.headers {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
                | "forwarded"
                | "x-forwarded-for"
                | "x-forwarded-host"
                | "x-forwarded-proto"
                | "x-real-ip"
                | "cf-connecting-ip"
                | "true-client-ip"
                | "x-client-ip"
                | "x-cluster-client-ip"
                | "x-9router-ui-secret"
                | "x-9r-rust-compat"
                | "x-9r-ui-proxy"
                | "x-9r-real-ip"
                | "x-9r-peer-token"
                | "x-9r-via-proxy"
        ) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            h.append(n, v);
        }
    }
    if let Some(host) = original_host {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&host) {
            h.insert(
                reqwest::header::HeaderName::from_static("x-forwarded-host"),
                value,
            );
        }
    }
    let forwarded_proto = if auth::is_loopback(peer)
        && parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
    {
        "https"
    } else {
        "http"
    };
    h.insert(
        reqwest::header::HeaderName::from_static("x-forwarded-proto"),
        reqwest::header::HeaderValue::from_static(forwarded_proto),
    );
    let client_ip = auth::rate_limit_ip(peer, &parts.headers);
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&client_ip.to_string()) {
        h.insert(
            reqwest::header::HeaderName::from_static("x-9r-real-ip"),
            v.clone(),
        );
        h.insert(
            reqwest::header::HeaderName::from_static("x-forwarded-for"),
            v,
        );
    }
    let secret = state.config.ui_only_header_secret.trim();
    if secret.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "NINEROUTER_UI_SECRET must not be empty"
        )));
    }
    h.insert(
        reqwest::header::HeaderName::from_static("x-9router-ui-secret"),
        reqwest::header::HeaderValue::from_str(secret).map_err(|error| {
            AppError::Internal(anyhow::anyhow!("invalid NINEROUTER_UI_SECRET: {error}"))
        })?,
    );
    h.insert(
        reqwest::header::HeaderName::from_static("x-9r-ui-proxy"),
        reqwest::header::HeaderValue::from_static(concat!("rust-", env!("CARGO_PKG_VERSION"))),
    );
    rb = rb.headers(h);
    let r = rb.send().await.map_err(|e| {
        AppError::Upstream(format!(
            "Next UI unavailable at {}: {e}",
            state.config.ui_origin
        ))
    })?;
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = r.headers().clone();
    let stream = r.bytes_stream().map(|x| x.map_err(std::io::Error::other));
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    copy_headers(&headers, out.headers_mut());
    Ok(out)
}

fn copy_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (k, v) in src {
        let n = k.as_str().to_ascii_lowercase();
        if matches!(
            n.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
                | "forwarded"
                | "x-forwarded-for"
                | "x-forwarded-host"
                | "x-forwarded-proto"
                | "x-real-ip"
                | "x-9router-ui-secret"
                | "x-9r-rust-compat"
                | "x-9r-ui-proxy"
                | "x-9r-real-ip"
                | "x-9r-peer-token"
                | "x-9r-via-proxy"
        ) {
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

fn redirect(location: &str) -> Result<Response<Body>, AppError> {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    r.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).map_err(|e| AppError::Internal(e.into()))?,
    );
    Ok(r)
}
