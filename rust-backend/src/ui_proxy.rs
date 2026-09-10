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
    if is_websocket_request(req.headers()) {
        return proxy_websocket(state, req).await;
    }
    proxy(state, peer, req).await
}

fn is_websocket_request(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

async fn proxy_websocket(
    state: AppState,
    mut req: Request<Body>,
) -> Result<Response<Body>, AppError> {
    let uri: url::Url = state
        .config
        .ui_origin
        .parse()
        .map_err(|e: url::ParseError| AppError::Internal(e.into()))?;
    let host = uri.host_str().unwrap_or("127.0.0.1");
    let port = uri.port().unwrap_or(20129);
    let target_addr = format!("{host}:{port}");

    let mut upstream = tokio::net::TcpStream::connect(&target_addr)
        .await
        .map_err(|e| AppError::Upstream(format!("WebSocket connect failed to {target_addr}: {e}")))?;

    let path_and_query = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let mut req_str = format!("GET {} HTTP/1.1\r\nHost: {}:{}\r\n", path_and_query, host, port);
    for (k, v) in req.headers() {
        if !k.as_str().eq_ignore_ascii_case("host") {
            if let Ok(v_str) = v.to_str() {
                req_str.push_str(&format!("{}: {}\r\n", k.as_str(), v_str));
            }
        }
    }
    req_str.push_str("\r\n");

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    upstream
        .write_all(req_str.as_bytes())
        .await
        .map_err(|e| AppError::Upstream(e.to_string()))?;

    const MAX_HANDSHAKE_HEADER: usize = 64 * 1024;
    let handshake = async {
        let mut response_bytes = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = upstream
                .read(&mut buf)
                .await
                .map_err(|e| AppError::Upstream(e.to_string()))?;
            if n == 0 {
                return Err(AppError::Upstream(
                    "Upstream closed during WebSocket handshake".into(),
                ));
            }
            response_bytes.extend_from_slice(&buf[..n]);
            if response_bytes.len() > MAX_HANDSHAKE_HEADER {
                return Err(AppError::Upstream("WebSocket handshake headers exceed 64KB".into()));
            }
            if let Some(pos) = response_bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok((response_bytes, pos));
            }
        }
    };
    let (response_bytes, pos) = tokio::time::timeout(std::time::Duration::from_secs(10), handshake)
        .await
        .map_err(|_| AppError::Upstream("WebSocket handshake timed out after 10s".into()))??;

    let header_str = String::from_utf8_lossy(&response_bytes[..pos]);
    let leftover = response_bytes[pos + 4..].to_vec();

            let mut lines = header_str.split("\r\n");
            let status_line = lines.next().unwrap_or("");
            let status_code = if status_line.contains("101") {
                StatusCode::SWITCHING_PROTOCOLS
            } else {
                StatusCode::BAD_GATEWAY
            };

            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = status_code;

            for line in lines {
                if let Some((k, v)) = line.split_once(": ") {
                    if let (Ok(hn), Ok(hv)) = (
                        header::HeaderName::from_bytes(k.as_bytes()),
                        header::HeaderValue::from_str(v),
                    ) {
                        resp.headers_mut().insert(hn, hv);
                    }
                }
            }

            tokio::spawn(async move {
                match hyper::upgrade::on(&mut req).await {
                    Ok(upgraded) => {
                        let mut client_stream = hyper_util::rt::TokioIo::new(upgraded);
                        if !leftover.is_empty() {
                            let _ = client_stream.write_all(&leftover).await;
                        }
                        let _ = tokio::io::copy_bidirectional(&mut client_stream, &mut upstream).await;
                    }
                    Err(e) => {
                        tracing::warn!("Client WebSocket upgrade failed: {e}");
                    }
                }
            });

            Ok(resp)
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
    let mut rb = state.http.request(method, &url).body(bytes);
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        let name = k.as_str();
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
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
        ) {
            continue;
        }
        // Preserve original case for Next.js internal headers
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            h.append(n, v);
        }
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&peer.ip().to_string()) {
        h.insert(reqwest::header::HeaderName::from_static("x-9r-real-ip"), v);
    }
    h.insert(
        reqwest::header::HeaderName::from_static("x-9r-ui-proxy"),
        reqwest::header::HeaderValue::from_static("rust-1.0.1"),
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
        let name = k.as_str();
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
                | "content-length"
        ) {
            continue;
        }
        // Preserve original case for Next.js internal headers
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
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
