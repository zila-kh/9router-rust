use std::{cmp::Ordering, time::Instant};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    error::AppError,
    gateway, providers,
    state::AppState,
    streaming,
    translate::{self, Format},
};

const DEFAULT_PAIRS: u64 = 5;
const MAX_PAIRS: u64 = 10;
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Wrapper,
    Native,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Wrapper => "wrapper",
            Self::Native => "native",
        }
    }
}

#[derive(Clone, Debug)]
struct Sample {
    pair: u64,
    warmup: bool,
    mode: Mode,
    ttft_ms: Option<u64>,
    total_ms: u64,
    output_tokens: Option<u64>,
    end_to_end_tps: Option<f64>,
    decode_tps: Option<f64>,
    error: Option<String>,
}

impl Sample {
    fn json(&self) -> Value {
        json!({
            "pair": self.pair,
            "warmup": self.warmup,
            "mode": self.mode.label(),
            "ttftMs": self.ttft_ms,
            "totalMs": self.total_ms,
            "outputTokens": self.output_tokens,
            "endToEndTps": self.end_to_end_tps,
            "decodeTps": self.decode_tps,
            "error": self.error,
        })
    }
}

#[derive(Debug)]
struct Prepared {
    requested_model: String,
    provider: String,
    upstream_model: String,
    connection: Value,
    native_format: Format,
    canonical: Value,
    pairs: u64,
}

pub fn targets(state: &AppState) -> Result<Response<Body>, AppError> {
    let mut targets = Vec::new();
    for connection in state.db.provider_connections(None, Some(true))? {
        let provider = connection
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if provider.is_empty() {
            continue;
        }
        let format = provider_format(provider);
        let executable = crate::auto_router::rust_gateway_supports_provider(provider);
        let eligible = executable && format != Format::OpenAi;
        let reason = if !executable {
            Some("This provider needs a transport executor that is not available in Rust.")
        } else if format == Format::OpenAi {
            Some("This provider already uses the OpenAI wire format, so there is no wrapper/native difference.")
        } else {
            None
        };
        let connection_id = connection
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let connection_name = connection
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| connection.get("email").and_then(Value::as_str))
            .unwrap_or(connection_id);
        for model in providers::models_for(provider) {
            let Some(model_id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            if model
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "llm")
            {
                continue;
            }
            let alias = providers::provider_entry(provider)
                .and_then(|entry| entry.get("alias"))
                .and_then(Value::as_str)
                .unwrap_or(provider);
            targets.push(json!({
                "type": "model",
                "id": format!("{alias}/{model_id}"),
                "name": model.get("name").and_then(Value::as_str).unwrap_or(model_id),
                "provider": provider,
                "nativeFormat": format_name(format),
                "benchmarkEligible": eligible,
                "benchmarkReason": reason,
                "connections": [{"id": connection_id, "name": connection_name}],
            }));
        }
    }

    let mut merged = serde_json::Map::new();
    for target in targets {
        let id = target
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(existing) = merged.get_mut(&id) {
            if let (Some(dst), Some(src)) = (
                existing
                    .get_mut("connections")
                    .and_then(Value::as_array_mut),
                target.get("connections").and_then(Value::as_array),
            ) {
                dst.extend(src.iter().cloned());
            }
        } else {
            merged.insert(id, target);
        }
    }
    let mut result: Vec<Value> = merged.into_values().collect();
    result.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
    for combo in state.db.combos()? {
        let Some(name) = combo.get("name").and_then(Value::as_str) else {
            continue;
        };
        result.push(json!({
            "type": "combo",
            "id": name,
            "name": name,
            "benchmarkEligible": false,
            "benchmarkReason": "Combos can select different models, so they cannot produce a pinned native comparison.",
            "connections": [],
        }));
    }
    Ok(json_response(json!({"targets": result})))
}

pub fn start(state: AppState, body: Value) -> Result<Response<Body>, AppError> {
    let prepared = prepare(&state, &body)?;
    let stream = async_stream::stream! {
        let mut scored = Vec::new();
        let total_pairs = prepared.pairs + 1;
        for sequence in 0..total_pairs {
            let warmup = sequence == 0;
            let pair = if warmup { 0 } else { sequence };
            let order = execution_order(pair, warmup);
            for mode in order {
                yield Ok::<Bytes, std::io::Error>(event("progress", json!({
                    "pair": pair,
                    "warmup": warmup,
                    "mode": mode.label(),
                    "message": format!("{} pair {}", if warmup { "Warm-up" } else { "Benchmark" }, if warmup { 1 } else { pair }),
                })));
                let sample = run_sample(&state, &prepared, mode, pair, warmup).await;
                yield Ok(event("sample", sample.json()));
                if let Some(message) = sample.error.as_deref() {
                    yield Ok(event("error", json!({
                        "pair": pair,
                        "warmup": warmup,
                        "mode": mode.label(),
                        "error": message,
                        "fatal": false,
                    })));
                }
                if !warmup {
                    scored.push(sample);
                }
            }
        }
        yield Ok(event("complete", json!({
            "model": prepared.requested_model,
            "provider": prepared.provider,
            "nativeFormat": format_name(prepared.native_format),
            "pairs": prepared.pairs,
            "requestCount": (prepared.pairs + 1) * 2,
            "samples": scored.iter().map(Sample::json).collect::<Vec<_>>(),
            "summary": summary(&scored),
        })));
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn prepare(state: &AppState, body: &Value) -> Result<Prepared, AppError> {
    let requested_model = required_string(body, "model")?;
    if state.db.combo_by_name(&requested_model)?.is_some() {
        return Err(AppError::BadRequest(
            "Benchmark requires an explicit model; combos are not eligible".into(),
        ));
    }
    let connection_id = required_string(body, "connectionId")?;
    let prompt = required_string(body, "prompt")?;
    let pairs = body
        .get("pairs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_PAIRS);
    if !(1..=MAX_PAIRS).contains(&pairs) {
        return Err(AppError::BadRequest(
            "pairs must be between 1 and 10".into(),
        ));
    }
    let resolved = providers::resolve_model(state, &requested_model)?;
    if !crate::auto_router::rust_gateway_supports_provider(&resolved.provider) {
        return Err(AppError::BadRequest(format!(
            "provider {} is not executable by the native Rust gateway",
            resolved.provider
        )));
    }
    let native_format = provider_format(&resolved.provider);
    if native_format == Format::OpenAi {
        return Err(AppError::BadRequest(
            "selected provider already uses the OpenAI wire format".into(),
        ));
    }
    let connection = state
        .db
        .provider_connections(Some(&resolved.provider), Some(true))?
        .into_iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(&connection_id))
        .ok_or_else(|| {
            AppError::BadRequest("connectionId is not an active account for this model".into())
        })?;

    let mut messages = Vec::new();
    if let Some(system) = body
        .get("system")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(json!({"role":"system","content":system}));
    }
    messages.push(json!({"role":"user","content":prompt}));
    let max_output = body
        .get("maxOutputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(256)
        .clamp(1, 8192);
    let temperature = body
        .get("temperature")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 2.0);
    let mut canonical = json!({
        "model": requested_model,
        "messages": messages,
        "max_tokens": max_output,
        "stream": true,
    });
    // ChatGPT's Codex transport rejects public sampling controls. Both legs
    // still receive the same canonical request; the selected account's native
    // backend owns sampling for this provider.
    if resolved.provider != "codex" {
        canonical["temperature"] = json!(temperature);
    }
    Ok(Prepared {
        requested_model,
        provider: resolved.provider,
        upstream_model: resolved.model,
        connection,
        native_format,
        canonical,
        pairs,
    })
}

async fn run_sample(
    state: &AppState,
    prepared: &Prepared,
    mode: Mode,
    pair: u64,
    warmup: bool,
) -> Sample {
    let started = Instant::now();
    let caller = if mode == Mode::Wrapper {
        Format::OpenAi
    } else {
        prepared.native_format
    };
    let response = gateway::execute_connection(
        state,
        &HeaderMap::new(),
        caller,
        true,
        prepared.canonical.clone(),
        &prepared.provider,
        &prepared.upstream_model,
        prepared.connection.clone(),
        false,
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return failed_sample(pair, warmup, mode, started, error.to_string());
        }
    };
    if !response.status().is_success() {
        return failed_sample(
            pair,
            warmup,
            mode,
            started,
            format!("HTTP {}", response.status()),
        );
    }

    let mut body = response.into_body().into_data_stream();
    let mut captured = Vec::new();
    let mut observer = TextObserver::new(caller);
    let mut ttft = None;
    let mut last_text = None;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                if captured.len().saturating_add(bytes.len()) > MAX_CAPTURE_BYTES {
                    return failed_sample(
                        pair,
                        warmup,
                        mode,
                        started,
                        "response exceeded 16 MiB benchmark limit".into(),
                    );
                }
                captured.extend_from_slice(&bytes);
                if observer.feed(&bytes) {
                    let elapsed = started.elapsed();
                    ttft.get_or_insert(elapsed);
                    last_text = Some(elapsed);
                }
            }
            Err(error) => {
                return failed_sample(pair, warmup, mode, started, error.to_string());
            }
        }
    }
    let total = started.elapsed();
    let usage = streaming::reduce_stream(&captured, caller, &prepared.upstream_model)
        .and_then(|native| translate::normalize_response(native, caller))
        .ok()
        .and_then(|canonical| canonical.get("usage").cloned());
    let output_tokens = usage
        .as_ref()
        .and_then(|value| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .filter(|count| *count > 0);
    let total_seconds = total.as_secs_f64();
    let end_to_end_tps = output_tokens
        .filter(|_| total_seconds > 0.0)
        .map(|tokens| tokens as f64 / total_seconds);
    let decode_seconds = ttft
        .zip(last_text)
        .map(|(first, last)| last.saturating_sub(first).as_secs_f64())
        .filter(|seconds| *seconds > 0.0);
    let decode_tps = output_tokens
        .zip(decode_seconds)
        .map(|(tokens, seconds)| tokens as f64 / seconds);
    Sample {
        pair,
        warmup,
        mode,
        ttft_ms: ttft.map(|duration| duration.as_millis() as u64),
        total_ms: total.as_millis() as u64,
        output_tokens,
        end_to_end_tps,
        decode_tps,
        error: None,
    }
}

fn failed_sample(pair: u64, warmup: bool, mode: Mode, started: Instant, error: String) -> Sample {
    Sample {
        pair,
        warmup,
        mode,
        ttft_ms: None,
        total_ms: started.elapsed().as_millis() as u64,
        output_tokens: None,
        end_to_end_tps: None,
        decode_tps: None,
        error: Some(error),
    }
}

fn summary(samples: &[Sample]) -> Value {
    let wrapper: Vec<&Sample> = samples
        .iter()
        .filter(|sample| !sample.warmup && sample.mode == Mode::Wrapper && sample.error.is_none())
        .collect();
    let native: Vec<&Sample> = samples
        .iter()
        .filter(|sample| !sample.warmup && sample.mode == Mode::Native && sample.error.is_none())
        .collect();
    let wrapper_json = mode_summary(&wrapper);
    let native_json = mode_summary(&native);
    let delta = |key: &str| {
        let wrapper_value = wrapper_json
            .get(key)
            .and_then(|metric| metric.get("median"))
            .and_then(Value::as_f64);
        let native_value = native_json
            .get(key)
            .and_then(|metric| metric.get("median"))
            .and_then(Value::as_f64);
        match (wrapper_value, native_value) {
            (Some(wrapper_value), Some(native_value)) => json!({
                "absolute": wrapper_value - native_value,
                "percent": if native_value == 0.0 { Value::Null } else { json!((wrapper_value - native_value) / native_value * 100.0) },
            }),
            _ => json!({"absolute":null,"percent":null}),
        }
    };
    json!({
        "wrapper": wrapper_json,
        "native": native_json,
        "overhead": {
            "ttftMs": delta("ttftMs"),
            "totalMs": delta("totalMs"),
            "outputTokens": delta("outputTokens"),
            "endToEndTps": delta("endToEndTps"),
            "decodeTps": delta("decodeTps"),
        },
        "failures": samples.iter().filter(|sample| sample.error.is_some()).count(),
    })
}

fn mode_summary(samples: &[&Sample]) -> Value {
    json!({
        "count": samples.len(),
        "ttftMs": stats(samples.iter().filter_map(|sample| sample.ttft_ms.map(|value| value as f64)).collect()),
        "totalMs": stats(samples.iter().map(|sample| sample.total_ms as f64).collect()),
        "outputTokens": stats(samples.iter().filter_map(|sample| sample.output_tokens.map(|value| value as f64)).collect()),
        "endToEndTps": stats(samples.iter().filter_map(|sample| sample.end_to_end_tps).collect()),
        "decodeTps": stats(samples.iter().filter_map(|sample| sample.decode_tps).collect()),
    })
}

fn stats(mut values: Vec<f64>) -> Value {
    values.retain(|value| value.is_finite());
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    if values.is_empty() {
        return json!({"median":null,"p95":null});
    }
    let median = if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    };
    let p95_index = ((values.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    json!({"median":median,"p95":values[p95_index.min(values.len() - 1)]})
}

struct TextObserver {
    format: Format,
    buffer: String,
}

impl TextObserver {
    fn new(format: Format) -> Self {
        Self {
            format,
            buffer: String::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> bool {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut saw_text = false;
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].trim().to_string();
            self.buffer.drain(..=index);
            let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(&line);
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if serde_json::from_str::<Value>(payload)
                .ok()
                .is_some_and(|value| event_has_text(self.format, &value))
            {
                saw_text = true;
            }
        }
        saw_text
    }
}

fn event_has_text(format: Format, value: &Value) -> bool {
    match format {
        Format::OpenAi => value
            .pointer("/choices/0/delta/content")
            .or_else(|| value.pointer("/choices/0/message/content"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Format::Responses => {
            value.get("type").and_then(Value::as_str) == Some("response.output_text.delta")
                && value
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
        }
        Format::Claude => value
            .pointer("/delta/text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Format::Gemini => value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
            }),
    }
}

fn execution_order(pair: u64, warmup: bool) -> [Mode; 2] {
    if !warmup && pair.is_multiple_of(2) {
        [Mode::Native, Mode::Wrapper]
    } else {
        [Mode::Wrapper, Mode::Native]
    }
}

fn provider_format(provider: &str) -> Format {
    let transport = providers::transport(provider);
    Format::from_provider(
        transport
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("openai"),
    )
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::OpenAi => "openai",
        Format::Claude => "claude",
        Format::Gemini => "gemini",
        Format::Responses => "responses",
    }
}

fn required_string(body: &Value, field: &str) -> Result<String, AppError> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest(format!("{field} is required")))
}

fn event(name: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

fn json_response(value: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Db};

    fn test_state() -> (tempfile::TempDir, AppState, String) {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("benchmark.sqlite");
        let db = Db::open(&db_path).unwrap();
        db.create_connection(json!({
            "id":"benchmark-connection",
            "provider":"codex",
            "authType":"oauth",
            "accessToken":"secret",
            "isActive":true
        }))
        .unwrap();
        let model = providers::models_for("codex")
            .into_iter()
            .find_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
            .expect("codex catalog model");
        let alias = providers::provider_entry("codex")
            .and_then(|entry| entry.get("alias"))
            .and_then(Value::as_str)
            .unwrap_or("codex");
        let config = Config {
            listen: "127.0.0.1:20128".parse().unwrap(),
            ui_origin: "http://127.0.0.1:1".into(),
            data_dir: temp.path().to_path_buf(),
            db_path,
            upstream_timeout_secs: 1,
            stream_first_chunk_timeout: std::time::Duration::from_secs(1),
            stream_stall_timeout: std::time::Duration::from_secs(1),
            ui_only_header_secret: "test-secret".into(),
            legacy_backend_origin: None,
            compat_api_enabled: true,
        };
        (
            temp,
            AppState::new(config, db).unwrap(),
            format!("{alias}/{model}"),
        )
    }

    #[test]
    fn alternating_order_balances_the_scored_pairs() {
        assert_eq!(execution_order(1, false), [Mode::Wrapper, Mode::Native]);
        assert_eq!(execution_order(2, false), [Mode::Native, Mode::Wrapper]);
        assert_eq!(execution_order(0, true), [Mode::Wrapper, Mode::Native]);
    }

    #[test]
    fn median_and_nearest_rank_p95_are_stable() {
        assert_eq!(stats(vec![5.0, 1.0, 3.0])["median"], json!(3.0));
        assert_eq!(stats(vec![5.0, 1.0, 3.0])["p95"], json!(5.0));
        assert_eq!(stats(vec![1.0, 3.0])["median"], json!(2.0));
    }

    #[test]
    fn detects_first_text_in_each_native_stream_format() {
        let cases = [
            (
                Format::OpenAi,
                r#"{"choices":[{"delta":{"content":"hello"}}]}"#,
            ),
            (
                Format::Responses,
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            ),
            (
                Format::Claude,
                r#"{"type":"content_block_delta","delta":{"text":"hello"}}"#,
            ),
            (
                Format::Gemini,
                r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#,
            ),
        ];
        for (format, payload) in cases {
            let mut observer = TextObserver::new(format);
            assert!(
                observer.feed(format!("data: {payload}\n").as_bytes()),
                "{format:?}"
            );
        }
    }

    #[test]
    fn missing_usage_produces_no_token_rates() {
        let sample = Sample {
            pair: 1,
            warmup: false,
            mode: Mode::Wrapper,
            ttft_ms: Some(10),
            total_ms: 20,
            output_tokens: None,
            end_to_end_tps: None,
            decode_tps: None,
            error: None,
        };
        let value = mode_summary(&[&sample]);
        assert!(value["endToEndTps"]["median"].is_null());
        assert!(value["decodeTps"]["median"].is_null());
    }

    #[test]
    fn summary_excludes_warmup_samples() {
        let warmup = Sample {
            pair: 0,
            warmup: true,
            mode: Mode::Wrapper,
            ttft_ms: Some(1),
            total_ms: 1,
            output_tokens: Some(100),
            end_to_end_tps: Some(100.0),
            decode_tps: Some(100.0),
            error: None,
        };
        let scored = Sample {
            pair: 1,
            warmup: false,
            mode: Mode::Wrapper,
            ttft_ms: Some(20),
            total_ms: 40,
            output_tokens: Some(2),
            end_to_end_tps: Some(50.0),
            decode_tps: Some(60.0),
            error: None,
        };
        let value = summary(&[warmup, scored]);
        assert_eq!(value["wrapper"]["count"], 1);
        assert_eq!(value["wrapper"]["ttftMs"]["median"], 20.0);
    }

    #[test]
    fn preparation_pins_the_selected_connection_and_validates_bounds() {
        let (_temp, state, model) = test_state();
        let prepared = prepare(
            &state,
            &json!({
                "model":model,
                "connectionId":"benchmark-connection",
                "prompt":"hello",
                "pairs":5
            }),
        )
        .unwrap();
        assert_eq!(prepared.connection["id"], "benchmark-connection");
        assert_eq!(prepared.native_format, Format::Responses);

        let error = prepare(
            &state,
            &json!({"model":model,"connectionId":"benchmark-connection","prompt":"hello","pairs":0}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pairs must be between 1 and 10"));
    }

    #[test]
    fn combos_and_connections_from_other_accounts_are_rejected() {
        let (_temp, state, model) = test_state();
        state
            .db
            .upsert_combo(json!({"name":"combo-bench","kind":"llm","models":[model.clone()]}))
            .unwrap();
        let combo_error = prepare(
            &state,
            &json!({"model":"combo-bench","connectionId":"benchmark-connection","prompt":"hello"}),
        )
        .unwrap_err();
        assert!(combo_error.to_string().contains("explicit model"));

        let connection_error = prepare(
            &state,
            &json!({"model":model,"connectionId":"another-account","prompt":"hello"}),
        )
        .unwrap_err();
        assert!(connection_error
            .to_string()
            .contains("not an active account"));
    }
}
