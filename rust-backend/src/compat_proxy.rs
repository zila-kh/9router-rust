use std::net::SocketAddr;

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri},
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::{auth, error::AppError, state::AppState};

const INTERNAL_SECRET_HEADER: &str = "x-9router-ui-secret";

pub async fn proxy_buffered(
    state: &AppState,
    peer: SocketAddr,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    if !state.config.compat_api_enabled {
        return Err(AppError::NotFound(
            "upstream compatibility API disabled".into(),
        ));
    }

    let secret = state.config.ui_only_header_secret.trim();
    if secret.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "NINEROUTER_UI_SECRET must not be empty when NINEROUTER_COMPAT_API is enabled"
        )));
    }

    let base = state.config.ui_origin.trim_end_matches('/');
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let url = format!("{base}{path_and_query}");
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|error| AppError::Internal(error.into()))?;

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

    let mut outbound_headers = reqwest::header::HeaderMap::new();
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
            outbound_headers.append(name, value);
        }
    }

    if let Some(host) = original_host {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&host) {
            outbound_headers.insert(
                reqwest::header::HeaderName::from_static("x-forwarded-host"),
                value,
            );
        }
    }
    outbound_headers.insert(
        reqwest::header::HeaderName::from_static("x-forwarded-proto"),
        reqwest::header::HeaderValue::from_static(forwarded_proto),
    );
    let client_ip = auth::rate_limit_ip(peer, headers);
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&client_ip.to_string()) {
        outbound_headers.insert(
            reqwest::header::HeaderName::from_static("x-9r-real-ip"),
            value.clone(),
        );
        outbound_headers.insert(
            reqwest::header::HeaderName::from_static("x-forwarded-for"),
            value,
        );
    }
    outbound_headers.insert(
        reqwest::header::HeaderName::from_static(INTERNAL_SECRET_HEADER),
        reqwest::header::HeaderValue::from_str(secret).map_err(|error| {
            AppError::Internal(anyhow::anyhow!("invalid NINEROUTER_UI_SECRET: {error}"))
        })?,
    );
    outbound_headers.insert(
        reqwest::header::HeaderName::from_static("x-9r-rust-compat"),
        reqwest::header::HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );

    let response = state
        .proxy_http
        .request(method, &url)
        .headers(outbound_headers)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            AppError::Upstream(format!(
                "upstream compatibility API unavailable at {}: {error}",
                state.config.ui_origin
            ))
        })?;

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = response.headers().clone();
    let stream = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let mut output = Response::new(Body::from_stream(stream));
    *output.status_mut() = status;
    copy_headers(&response_headers, output.headers_mut());
    output.headers_mut().insert(
        HeaderName::from_static("x-9router-runtime"),
        HeaderValue::from_static("upstream-compat"),
    );
    Ok(output)
}

fn copy_headers(source: &reqwest::header::HeaderMap, destination: &mut HeaderMap) {
    for (name, value) in source {
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
            destination.append(name, value);
        }
    }
}

fn is_internal_header(name: &str) -> bool {
    matches!(
        name,
        INTERNAL_SECRET_HEADER
            | "x-9router-runtime"
            | "x-9r-rust-compat"
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
    fn untrusted_forwarding_and_internal_headers_are_classified() {
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
            "x-9r-ui-proxy",
            "x-9r-real-ip",
            "x-9r-peer-token",
            "x-9r-via-proxy",
        ] {
            assert!(is_internal_header(header), "{header}");
        }
    }
}
