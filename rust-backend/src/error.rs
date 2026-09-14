use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("upstream HTTP {status}: {message}")]
    UpstreamHttp { status: u16, message: String },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".into()),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            // Upstream strings may contain credential-bearing URLs or provider
            // response bodies. Never return these details to a public API user.
            Self::Upstream(_) | Self::UpstreamHttp { .. } => {
                (StatusCode::BAD_GATEWAY, "Upstream request failed".into())
            }
            Self::Internal(e) => {
                tracing::error!(error=?e, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".into(),
                )
            }
        };
        let mut response = (status, Json(json!({"error": message}))).into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}

impl AppError {
    pub fn auto_route_retryable(&self) -> bool {
        match self {
            // Network failures and provider-specific HTTP failures are normally
            // safe to try on another provider/model. 401/403/404/429 in
            // particular can be account-, quota-, or model-specific and should
            // not prevent a healthy fallback from serving the request.
            Self::Upstream(_) => true,
            Self::UpstreamHttp { status, .. } => *status != 400,
            // During an auto plan the model has already resolved. A later
            // NotFound is normally a provider/account availability race, so let
            // the next planned candidate run.
            Self::NotFound(_) => true,
            // These are local/request/auth/internal failures. Trying more paid
            // models cannot fix them and can create unnecessary fan-out.
            Self::BadRequest(_) | Self::Unauthorized | Self::Forbidden(_) | Self::Internal(_) => {
                false
            }
        }
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Internal(value.into())
    }
}
impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        Self::Upstream(value.without_url().to_string())
    }
}
impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::BadRequest(value.to_string())
    }
}

#[cfg(test)]
mod release_error_tests {
    use super::*;
    use axum::{body::to_bytes, http::header};

    #[test]
    fn auto_route_retries_provider_specific_failures_but_not_bad_requests() {
        assert!(AppError::Upstream("network timeout".into()).auto_route_retryable());
        for status in [401, 403, 404, 408, 413, 422, 429, 500, 503] {
            assert!(AppError::UpstreamHttp {
                status,
                message: "provider-specific failure".into()
            }
            .auto_route_retryable());
        }
        assert!(!AppError::UpstreamHttp {
            status: 400,
            message: "invalid request".into()
        }
        .auto_route_retryable());
        assert!(AppError::NotFound("provider account disappeared".into()).auto_route_retryable());
        assert!(!AppError::BadRequest("bad request".into()).auto_route_retryable());
        assert!(!AppError::Unauthorized.auto_route_retryable());
        assert!(!AppError::Forbidden("local policy".into()).auto_route_retryable());
    }

    #[tokio::test]
    async fn upstream_failures_do_not_disclose_credentials_or_internal_urls() {
        let response = AppError::Upstream(
            "request to http://user:private@127.0.0.1:20129/?key=secret-provider-key failed".into(),
        )
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, json!({"error":"Upstream request failed"}));
    }
}
