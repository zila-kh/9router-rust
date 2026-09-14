//! Canonicalize routing characters before any authorization or proxy decision.
//! Reserved escapes (notably encoded slashes in model IDs) remain encoded.
use crate::error::AppError;
use axum::http::Uri;

fn invalid_path() -> AppError {
    AppError::BadRequest("Invalid or ambiguous request path".into())
}

pub fn canonical_path(path: &str) -> Result<String, AppError> {
    if !path.starts_with('/') {
        return Err(invalid_path());
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16));
            let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16));
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(invalid_path());
            };
            let decoded = (hi * 16 + lo) as u8;
            if decoded == b'\\' || decoded.is_ascii_control() {
                return Err(invalid_path());
            }
            if decoded.is_ascii_alphanumeric() || b"-._~".contains(&decoded) {
                out.push(decoded);
            } else {
                out.extend_from_slice(&[
                    b'%',
                    bytes[i + 1].to_ascii_uppercase(),
                    bytes[i + 2].to_ascii_uppercase(),
                ]);
            }
            i += 3;
        } else {
            if byte == b'\\' || byte.is_ascii_control() {
                return Err(invalid_path());
            }
            out.push(byte);
            i += 1;
        }
    }
    let path = String::from_utf8(out).map_err(|_| invalid_path())?;
    if path.contains("//") || path.split('/').any(|s| matches!(s, "." | "..")) {
        return Err(invalid_path());
    }
    Ok(if path == "/" {
        path
    } else {
        path.trim_end_matches('/').to_string()
    })
}

pub fn canonical_uri(uri: &Uri) -> Result<Uri, AppError> {
    let path = canonical_path(uri.path())?;
    let path_and_query = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse().map_err(|_| invalid_path())?);
    Uri::from_parts(parts).map_err(|_| invalid_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn shared_path_fixtures() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../assets/request-path-cases.json")).unwrap();
        for case in cases {
            let input = case["input"].as_str().unwrap();
            let result = canonical_path(input);
            if case["error"] == true {
                assert!(result.is_err(), "{input}");
            } else {
                assert_eq!(
                    result.unwrap(),
                    case["expected"].as_str().unwrap(),
                    "{input}"
                );
            }
        }
    }

    #[test]
    fn query_is_never_decoded_or_rewritten() {
        let uri: Uri = "/%61pi/oauth/codex/exchange?code=a%2Fb+z&state=%252e&key=a%26b"
            .parse()
            .unwrap();
        assert_eq!(
            canonical_uri(&uri).unwrap().to_string(),
            "/api/oauth/codex/exchange?code=a%2Fb+z&state=%252e&key=a%26b"
        );
    }
}
