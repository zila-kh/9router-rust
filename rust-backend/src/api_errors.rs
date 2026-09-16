//! Response helpers shared by the native search and web-fetch adapters.
//!
//! Mirrors upstream `frontend/open-sse/utils/error.js` (`buildErrorBody`,
//! `errorResponse`) and `frontend/open-sse/config/errorConfig.js`
//! (`ERROR_TYPES` / `DEFAULT_ERROR_MESSAGES`) for the envelopes these two
//! routes return, plus the `{ error: { message, code } }` envelope the
//! search/web-fetch cores use for provider failures.

use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

use crate::{error::AppError, inference_media::json_response};

/// `ERROR_TYPES` from upstream `errorConfig.js`.
fn error_type(status: u16) -> (&'static str, String) {
    match status {
        400 => ("invalid_request_error", "bad_request".to_string()),
        401 => ("authentication_error", "invalid_api_key".to_string()),
        402 => ("billing_error", "payment_required".to_string()),
        403 => ("permission_error", "insufficient_quota".to_string()),
        404 => ("invalid_request_error", "model_not_found".to_string()),
        406 => ("invalid_request_error", "model_not_supported".to_string()),
        429 => ("rate_limit_error", "rate_limit_exceeded".to_string()),
        500 => ("server_error", "internal_server_error".to_string()),
        502 => ("server_error", "bad_gateway".to_string()),
        503 => ("server_error", "service_unavailable".to_string()),
        504 => ("server_error", "gateway_timeout".to_string()),
        _ if status >= 500 => ("server_error", "internal_server_error".to_string()),
        _ => ("invalid_request_error", String::new()),
    }
}

fn default_message(status: u16) -> &'static str {
    match status {
        400 => "Bad request",
        401 => "Invalid API key provided",
        402 => "Payment required",
        403 => "You exceeded your current quota",
        404 => "Model not found",
        406 => "Model not supported",
        429 => "Rate limit exceeded",
        500 => "Internal server error",
        502 => "Bad gateway - upstream provider error",
        503 => "Service temporarily unavailable",
        504 => "Gateway timeout",
        _ => "An error occurred",
    }
}

/// Upstream `buildErrorBody(statusCode, message)`.
pub fn error_body(status: u16, message: Option<&str>) -> Value {
    let (kind, code) = error_type(status);
    let message = message
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_message(status));
    json!({
        "error": {
            "message": message,
            "type": kind,
            "code": code,
        }
    })
}

/// Upstream `errorResponse(statusCode, message)`.
pub fn error_response(status: u16, message: Option<&str>) -> Result<Response<Body>, AppError> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json_response(status, error_body(status.as_u16(), message))
}

/// Upstream search/web-fetch core failure envelope:
/// `{ error: { message, code } }` returned with the provider's status code.
pub fn provider_error_response(status: u16, message: &str) -> Result<Response<Body>, AppError> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    json_response(
        status,
        json!({ "error": { "message": message, "code": status.as_u16() } }),
    )
}

/// Upstream `jsonResponse(body, status)` for successful payloads.
pub fn ok_response(value: Value) -> Result<Response<Body>, AppError> {
    json_response(StatusCode::OK, value)
}

/// Upstream routes only export `POST`; the repo's long-standing contract for
/// these paths is a 405 with this payload.
pub fn method_not_allowed() -> Result<Response<Body>, AppError> {
    json_response(
        StatusCode::METHOD_NOT_ALLOWED,
        json!({ "error": "Method Not Allowed" }),
    )
}

/// `text.slice(0, max)` measured in UTF-16 code units (upstream string length),
/// cut on a UTF-8 boundary.
pub fn truncate_utf16(text: &str, max: Option<usize>) -> String {
    let Some(max) = max else {
        return text.to_string();
    };
    let mut units = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let width = ch.len_utf16();
        if units + width > max {
            break;
        }
        units += width;
        out.push(ch);
    }
    out
}

impl AppError {
    /// Convert any error into the OpenAI-shaped envelope used by these routes.
    pub fn into_public_response(self) -> Response<Body> {
        match self {
            AppError::BadRequest(message) => {
                error_response(400, Some(&message)).unwrap_or_else(IntoResponse::into_response)
            }
            AppError::Unauthorized => {
                error_response(401, None).unwrap_or_else(IntoResponse::into_response)
            }
            AppError::Forbidden(message) => {
                error_response(403, Some(&message)).unwrap_or_else(IntoResponse::into_response)
            }
            AppError::NotFound(message) => {
                error_response(404, Some(&message)).unwrap_or_else(IntoResponse::into_response)
            }
            other => other.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use pretty_assertions::assert_eq;

    async fn body_of(response: Response<Body>) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn error_envelope_matches_upstream_error_types() {
        let cases = [
            (400, "invalid_request_error", "bad_request"),
            (401, "authentication_error", "invalid_api_key"),
            (402, "billing_error", "payment_required"),
            (403, "permission_error", "insufficient_quota"),
            (404, "invalid_request_error", "model_not_found"),
            (429, "rate_limit_error", "rate_limit_exceeded"),
            (500, "server_error", "internal_server_error"),
            (502, "server_error", "bad_gateway"),
            (503, "server_error", "service_unavailable"),
            (504, "server_error", "gateway_timeout"),
        ];
        for (status, kind, code) in cases {
            let value = error_body(status, Some("boom"));
            assert_eq!(value["error"]["message"], "boom");
            assert_eq!(value["error"]["type"], kind, "status {status}");
            assert_eq!(value["error"]["code"], code, "status {status}");
        }
    }

    #[tokio::test]
    async fn missing_message_falls_back_to_defaults() {
        let response = error_response(400, None).expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_of(response).await;
        assert_eq!(body["error"]["message"], "Bad request");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn provider_error_uses_status_code_in_body() {
        let response =
            provider_error_response(401, "brave-search returned 401: nope").expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_of(response).await;
        assert_eq!(body["error"]["message"], "brave-search returned 401: nope");
        assert_eq!(body["error"]["code"], 401);
        assert!(body["error"].get("type").is_none());
    }

    #[test]
    fn truncate_utf16_counts_code_units() {
        assert_eq!(truncate_utf16("hello", Some(3)), "hel");
        assert_eq!(truncate_utf16("hello", None), "hello");
        assert_eq!(truncate_utf16("aé😀b", Some(2)), "aé");
        assert_eq!(truncate_utf16("aé😀b", Some(4)), "aé😀");
    }
}
