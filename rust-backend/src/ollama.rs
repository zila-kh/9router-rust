//! Compatibility discovery contract from the pinned upstream snapshot.
use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
};
use serde_json::json;

pub fn tags(method: &Method) -> Response<Body> {
    let (status, body) = match *method {
        Method::GET => (StatusCode::OK, Body::from(json!({"models": [
            {"name":"llama3.2","modified_at":"2025-12-26T00:00:00Z","size":2000000000u64,"digest":"abc123def456","details":{"format":"gguf","family":"llama","parameter_size":"3B","quantization_level":"Q4_K_M"}},
            {"name":"qwen2.5","modified_at":"2025-12-26T00:00:00Z","size":4000000000u64,"digest":"def456abc123","details":{"format":"gguf","family":"qwen","parameter_size":"7B","quantization_level":"Q4_K_M"}}
        ]}).to_string())),
        Method::OPTIONS => (StatusCode::OK, Body::empty()),
        _ => (StatusCode::METHOD_NOT_ALLOWED, Body::empty()),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, OPTIONS".parse().unwrap(),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "*".parse().unwrap());
    if method == Method::GET {
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        headers.insert(header::ALLOW, "GET, OPTIONS".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    #[tokio::test]
    async fn discovery_matches_pinned_upstream() {
        let response = tags(&Method::GET);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["models"].as_array().unwrap().len(), 2);
        assert_eq!(value["models"][0]["name"], "llama3.2");
        assert_eq!(value["models"][1]["size"], 4000000000u64);
        assert_eq!(
            value["models"][1]["details"]["quantization_level"],
            "Q4_K_M"
        );
    }

    #[tokio::test]
    async fn preflight_is_empty_and_mutations_are_rejected() {
        let response = tags(&Method::OPTIONS);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            "GET, OPTIONS"
        );
        assert!(to_bytes(response.into_body(), 8192)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(tags(&Method::POST).status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
