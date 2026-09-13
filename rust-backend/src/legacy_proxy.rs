use std::net::SocketAddr;

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri},
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::{auth, error::AppError, state::AppState};

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

    let original_host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let forwarded_proto = if auth::is_loopback(peer)
        && headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("https"))
    {
        "https"
    } else {
        "http"
    };
    let client_ip = auth::rate_limit_ip(peer, headers);

    let mut out_headers = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if is_hop(&lower)
            || is_forwarding_header(&lower)
            || is_internal_header(&lower)
            || lower == "host"
            || lower == "content-length"
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out_headers.append(name, value);
        }
    }

    if let Some(host) = original_host {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&host) {
            out_headers.insert(
                reqwest::header::HeaderName::from_static("x-forwarded-host"),
                value,
            );
        }
    }
    out_headers.insert(
        reqwest::header::HeaderName::from_static("x-forwarded-proto"),
        reqwest::header::HeaderValue::from_static(forwarded_proto),
    );
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&client_ip.to_string()) {
        out_headers.insert(
            reqwest::header::HeaderName::from_static("x-9r-real-ip"),
            value.clone(),
        );
        out_headers.insert(
            reqwest::header::HeaderName::from_static("x-real-ip"),
            value.clone(),
        );
        out_headers.insert(
            reqwest::header::HeaderName::from_static("x-forwarded-for"),
            value,
        );
    }
    out_headers.insert(
        reqwest::header::HeaderName::from_static("x-9r-rust-legacy-bridge"),
        reqwest::header::HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
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
    for (name, value) in src {
        let lower = name.as_str().to_ascii_lowercase();
        if is_hop(&lower)
            || is_forwarding_header(&lower)
            || is_internal_header(&lower)
            || lower == "content-length"
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            dst.append(name, value);
        }
    }
}

fn is_internal_header(name: &str) -> bool {
    matches!(
        name,
        "x-9router-ui-secret"
            | "x-9router-runtime"
            | "x-9r-rust-compat"
            | "x-9r-rust-legacy-bridge"
            | "x-9r-ui-proxy"
            | "x-9r-real-ip"
            | "x-9r-peer-token"
            | "x-9r-via-proxy"
    )
}

fn is_forwarding_header(name: &str) -> bool {
    matches!(
        name,
        "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-real-ip"
            | "cf-connecting-ip"
            | "true-client-ip"
            | "x-client-ip"
            | "x-cluster-client-ip"
    )
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

#[cfg(test)]
mod tests {
    use super::{is_forwarding_header, is_internal_header};

    #[test]
    fn legacy_bridge_rejects_client_identity_headers() {
        for header in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-real-ip",
            "cf-connecting-ip",
        ] {
            assert!(is_forwarding_header(header), "{header}");
        }
        for header in [
            "x-9router-ui-secret",
            "x-9router-runtime",
            "x-9r-rust-compat",
            "x-9r-rust-legacy-bridge",
            "x-9r-real-ip",
            "x-9r-peer-token",
            "x-9r-via-proxy",
        ] {
            assert!(is_internal_header(header), "{header}");
        }
    }
}
