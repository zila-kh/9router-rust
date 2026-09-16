//! Native voice catalog for the public `/v1/audio/voices` route and the
//! dashboard `/api/media-providers/tts/**` voice pickers.
//!
//! Upstream reads voices from five provider-specific sources:
//!
//! - `edge-tts`: the live Bing read-aloud voice list (no auth, 24h cache);
//! - `local-device`: the host voice list of the machine running the backend
//!   (Windows SAPI through `powershell.exe`, macOS `say`, cached per process);
//! - `elevenlabs` / `deepgram` / `inworld`: provider APIs called with the
//!   stored active connection key.
//!
//! Language grouping, per-language de-duplication, error payloads, and the
//! `{object:"list"}` projection mirror the pinned upstream handlers so
//! compatibility mode and strict native mode return the same documents.
//!
//! `langName`/`countryName` come from embedded tables generated from Node's
//! `Intl.DisplayNames`, the source upstream uses. Region-tagged language codes
//! use the generic `Language (Region)` form instead of the handful of
//! idiomatic CLDR names (e.g. `pt-BR` -> "Brazilian Portuguese"); those fields
//! are display-only metadata that no dashboard consumer reads.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};

use crate::{error::AppError, inference_media::json_response, state::AppState};

/// Providers accepted by `GET /v1/audio/voices`, in upstream `PROVIDER_API` order.
pub const PUBLIC_VOICE_PROVIDERS: [&str; 5] = [
    "elevenlabs",
    "deepgram",
    "inworld",
    "edge-tts",
    "local-device",
];

const VOICE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const EDGE_TTS_VOICES_URL: &str = "https://speech.platform.bing.com/consumer/speech/synthesize/readaloud/voices/list?trustedclienttoken=6A5AA1D4EAFF4E9FB37E23D68491D6F4";
/// `UA` from upstream `open-sse/handlers/ttsProviders/_base.js`.
const EDGE_TTS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";
/// The SAPI probe upstream runs through `powershell.exe` (`fetchVoicesWin`).
const WINDOWS_VOICES_SCRIPT: &str = "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.GetInstalledVoices() | ForEach-Object { $v = $_.VoiceInfo; [PSCustomObject]@{ Name=$v.Name; Culture=$v.Culture.Name; Gender=$v.Gender } } | ConvertTo-Json -Compress";

type VoiceCache = Mutex<HashMap<String, (Instant, Vec<Value>)>>;

static VOICE_CACHE: Lazy<VoiceCache> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
pub struct VoiceError {
    status: StatusCode,
    message: String,
}

impl VoiceError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceSource {
    EdgeTts,
    LocalDevice,
    ElevenLabs,
    Deepgram,
    Inworld,
}

impl VoiceSource {
    fn known(provider: &str) -> Option<Self> {
        match provider {
            "edge-tts" => Some(Self::EdgeTts),
            "local-device" => Some(Self::LocalDevice),
            "elevenlabs" => Some(Self::ElevenLabs),
            "deepgram" => Some(Self::Deepgram),
            "inworld" => Some(Self::Inworld),
            _ => None,
        }
    }

    /// The generic internal route only serves sources that need no stored
    /// credential; the credentialed providers have dedicated routes.
    fn generic(provider: &str) -> Option<Self> {
        match Self::known(provider) {
            Some(Self::EdgeTts) => Some(Self::EdgeTts),
            Some(Self::LocalDevice) => Some(Self::LocalDevice),
            _ => None,
        }
    }

    /// Label used by the upstream "No <provider> connection found" error.
    fn connection_label(self) -> &'static str {
        match self {
            Self::ElevenLabs => "ElevenLabs",
            Self::Deepgram => "Deepgram",
            Self::Inworld => "Inworld",
            Self::EdgeTts | Self::LocalDevice => "",
        }
    }
}

/// `GET /v1/audio/voices?provider={p}[&lang=xx]` (upstream OpenAI-style list).
pub async fn handle_public(
    state: &AppState,
    method: &Method,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let params = query_params(query);
    let provider = params.get("provider").map(String::as_str).unwrap_or("");
    let Some(source) = VoiceSource::known(provider) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": {
                "message": provider_hint(),
                "type": "invalid_request_error",
            }}),
        );
    };
    let lang = non_empty(params.get("lang"));
    match load_voices(state, source).await {
        Ok(voices) => {
            let groups = group_by_lang(&voices);
            json_response(
                StatusCode::OK,
                json!({
                    "object": "list",
                    "data": public_data(&groups, provider, lang),
                }),
            )
        }
        Err(error) => json_response(
            error.status,
            json!({"error": {"message": error.message, "type": "server_error"}}),
        ),
    }
}

/// `GET /api/media-providers/tts/voices?provider={p}` (generic) and
/// `GET /api/media-providers/tts/{provider}/voices` (dedicated) for the
/// dashboard voice picker.
pub async fn handle_internal(
    state: &AppState,
    method: &Method,
    subpath: &str,
    query: Option<&str>,
) -> Result<Response<Body>, AppError> {
    if method != Method::GET {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method Not Allowed"}),
        );
    }
    let params = query_params(query);
    let lang = non_empty(params.get("lang"));
    let subpath = subpath.trim_matches('/');

    if subpath == "voices" {
        let provider = non_empty(params.get("provider")).unwrap_or("edge-tts");
        let Some(source) = VoiceSource::generic(provider) else {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("Provider '{provider}' does not support voice listing")}),
            );
        };
        return match load_voices(state, source).await {
            Ok(voices) => {
                let filtered: Vec<Value> = match lang {
                    Some(code) => voices
                        .iter()
                        .filter(|voice| voice.get("lang").and_then(Value::as_str) == Some(code))
                        .cloned()
                        .collect(),
                    None => voices,
                };
                json_response(StatusCode::OK, generic_payload(&filtered))
            }
            Err(error) => json_response(error.status, json!({"error": error.message})),
        };
    }

    let Some(provider) = subpath.strip_suffix("/voices") else {
        return json_response(StatusCode::NOT_FOUND, json!({"error": "Not Found"}));
    };
    if matches!(provider, "minimax" | "minimax-cn") {
        return json_response(
            StatusCode::NOT_IMPLEMENTED,
            json!({"error": "Native MiniMax voice listing is not implemented yet"}),
        );
    }
    let Some(source) = VoiceSource::known(provider) else {
        return json_response(StatusCode::NOT_FOUND, json!({"error": "Not Found"}));
    };
    match load_voices(state, source).await {
        Ok(voices) => {
            let groups = group_by_lang(&voices);
            json_response(StatusCode::OK, dedicated_payload(&groups, lang))
        }
        Err(error) => json_response(error.status, json!({"error": error.message})),
    }
}

async fn load_voices(state: &AppState, source: VoiceSource) -> Result<Vec<Value>, VoiceError> {
    match source {
        VoiceSource::EdgeTts => load_edge_tts(state).await,
        VoiceSource::LocalDevice => Ok(load_local_device()),
        VoiceSource::ElevenLabs => load_elevenlabs(state).await,
        VoiceSource::Deepgram => load_deepgram(state).await,
        VoiceSource::Inworld => load_inworld(state).await,
    }
}

async fn load_edge_tts(state: &AppState) -> Result<Vec<Value>, VoiceError> {
    if let Some(cached) = cache_lookup("edge-tts", Some(VOICE_CACHE_TTL)) {
        return Ok(cached);
    }
    let response = state
        .http
        .get(EDGE_TTS_VOICES_URL)
        .header(header::USER_AGENT, EDGE_TTS_UA)
        .send()
        .await
        .map_err(|error| {
            VoiceError::bad_gateway(format!("Edge TTS voices fetch failed: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(VoiceError::bad_gateway(format!(
            "Edge TTS voices fetch failed: {}",
            response.status().as_u16()
        )));
    }
    let raw: Value = response.json().await.map_err(|error| {
        VoiceError::bad_gateway(format!("Edge TTS voices fetch failed: {error}"))
    })?;
    let voices = edge_tts_entries(&raw);
    cache_store("edge-tts", &voices);
    Ok(voices)
}

fn load_local_device() -> Vec<Value> {
    if let Some(cached) = cache_lookup("local-device", None) {
        return cached;
    }
    let voices = local_device_voices();
    cache_store("local-device", &voices);
    voices
}

async fn load_elevenlabs(state: &AppState) -> Result<Vec<Value>, VoiceError> {
    let source = VoiceSource::ElevenLabs;
    let api_key = active_connection_key(state, "elevenlabs", source.connection_label())?;
    let key = cache_key("elevenlabs", Some(&api_key));
    if let Some(cached) = cache_lookup(&key, Some(VOICE_CACHE_TTL)) {
        return Ok(cached);
    }
    let response = state
        .http
        .get("https://api.elevenlabs.io/v1/voices")
        .header("xi-api-key", &api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .map_err(|error| {
            VoiceError::bad_gateway(format!("ElevenLabs voices fetch failed: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(VoiceError::bad_gateway(format!(
            "ElevenLabs voices fetch failed: {}",
            response.status().as_u16()
        )));
    }
    let raw: Value = response.json().await.map_err(|error| {
        VoiceError::bad_gateway(format!("ElevenLabs voices fetch failed: {error}"))
    })?;
    let voices = elevenlabs_entries(&raw);
    cache_store(&key, &voices);
    Ok(voices)
}

async fn load_deepgram(state: &AppState) -> Result<Vec<Value>, VoiceError> {
    let source = VoiceSource::Deepgram;
    let api_key = active_connection_key(state, "deepgram", source.connection_label())?;
    let key = cache_key("deepgram", Some(&api_key));
    if let Some(cached) = cache_lookup(&key, Some(VOICE_CACHE_TTL)) {
        return Ok(cached);
    }
    let response = state
        .http
        .get("https://api.deepgram.com/v1/models")
        .header(header::AUTHORIZATION, format!("Token {api_key}"))
        .send()
        .await
        .map_err(|error| {
            VoiceError::bad_gateway(format!("Deepgram voices fetch failed: {error}"))
        })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(VoiceError::bad_gateway(format!(
            "Deepgram API {status}: {}",
            upstream_error_text(&text)
        )));
    }
    let raw: Value = response.json().await.map_err(|error| {
        VoiceError::bad_gateway(format!("Deepgram voices fetch failed: {error}"))
    })?;
    let voices = deepgram_entries(&raw);
    cache_store(&key, &voices);
    Ok(voices)
}

async fn load_inworld(state: &AppState) -> Result<Vec<Value>, VoiceError> {
    let source = VoiceSource::Inworld;
    let api_key = active_connection_key(state, "inworld", source.connection_label())?;
    let key = cache_key("inworld", Some(&api_key));
    if let Some(cached) = cache_lookup(&key, Some(VOICE_CACHE_TTL)) {
        return Ok(cached);
    }
    let response = state
        .http
        .get("https://api.inworld.ai/tts/v1/voices")
        .header(header::AUTHORIZATION, format!("Basic {api_key}"))
        .send()
        .await
        .map_err(|error| {
            VoiceError::bad_gateway(format!("Inworld voices fetch failed: {error}"))
        })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(VoiceError::bad_gateway(format!(
            "Inworld API {status}: {}",
            upstream_error_text(&text)
        )));
    }
    let raw: Value = response.json().await.map_err(|error| {
        VoiceError::bad_gateway(format!("Inworld voices fetch failed: {error}"))
    })?;
    let voices = inworld_entries(&raw);
    cache_store(&key, &voices);
    Ok(voices)
}

/// Upstream `res.text() || "Failed"`, with the body clipped for log safety.
fn upstream_error_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "Failed".into();
    }
    trimmed.chars().take(2048).collect()
}

fn active_connection_key(
    state: &AppState,
    provider: &str,
    label: &str,
) -> Result<String, VoiceError> {
    let connections = state
        .db
        .provider_connections(Some(provider), Some(true))
        .map_err(|error| VoiceError::bad_gateway(error.to_string()))?;
    connections
        .first()
        .and_then(|connection| connection.get("apiKey"))
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .ok_or_else(|| VoiceError::bad_request(format!("No {label} connection found")))
}

fn query_params(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|text| !text.is_empty())
}

fn provider_hint() -> String {
    format!(
        "provider must be one of: {}",
        PUBLIC_VOICE_PROVIDERS.join(", ")
    )
}

/// Upstream `AI_PROVIDERS[provider]?.alias || provider`, where the dashboard
/// constants set `alias = uiAlias || alias` from the provider registry.
fn provider_alias(provider: &str) -> String {
    crate::providers::provider_entry(provider)
        .and_then(|entry| entry.get("uiAlias").or_else(|| entry.get("alias")))
        .and_then(Value::as_str)
        .filter(|alias| !alias.is_empty())
        .unwrap_or(provider)
        .to_string()
}

fn cache_key(provider: &str, secret: Option<&str>) -> String {
    match secret {
        Some(secret) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            secret.hash(&mut hasher);
            format!("{provider}:{:016x}", hasher.finish())
        }
        None => provider.to_string(),
    }
}

fn cache_lookup(key: &str, ttl: Option<Duration>) -> Option<Vec<Value>> {
    let cache = VOICE_CACHE.lock().ok()?;
    let (stored_at, voices) = cache.get(key)?;
    match ttl {
        Some(ttl) if stored_at.elapsed() >= ttl => None,
        _ => Some(voices.clone()),
    }
}

fn cache_store(key: &str, voices: &[Value]) {
    if let Ok(mut cache) = VOICE_CACHE.lock() {
        cache.insert(key.to_string(), (Instant::now(), voices.to_vec()));
    }
}

/// Raw Bing list -> the generic-route voice shape (`fetchVoices` + route map).
fn edge_tts_entries(raw: &Value) -> Vec<Value> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("ShortName").and_then(Value::as_str)?;
            let friendly = item
                .get("FriendlyName")
                .and_then(Value::as_str)
                .unwrap_or(id);
            let name = friendly
                .replacen("Microsoft ", "", 1)
                .replace(" Online (Natural) - ", " (");
            let locale = item.get("Locale").and_then(Value::as_str).unwrap_or("");
            let mut parts = locale.splitn(2, '-');
            let lang = parts.next().unwrap_or("");
            let country = parts.next().unwrap_or("");
            Some(json!({
                "id": id,
                "name": name,
                "locale": locale,
                "lang": lang,
                "country": country,
                "countryName": country_name(if country.is_empty() { lang } else { country }),
                "langName": language_name(lang),
                "gender": item.get("Gender").and_then(Value::as_str).unwrap_or(""),
            }))
        })
        .collect()
}

/// Upstream `api/media-providers/tts/elevenlabs/voices`: every voice is listed
/// under its primary language plus each additional verified language.
fn elevenlabs_entries(raw: &Value) -> Vec<Value> {
    let Some(items) = raw.get("voices").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in items {
        let Some(id) = item.get("voice_id").and_then(Value::as_str) else {
            continue;
        };
        let name = item.get("name").and_then(Value::as_str).unwrap_or(id);
        let gender = item
            .pointer("/labels/gender")
            .and_then(Value::as_str)
            .unwrap_or("");
        let primary = item
            .pointer("/labels/language")
            .and_then(Value::as_str)
            .filter(|language| !language.is_empty())
            .unwrap_or("en");
        let free_users_allowed = item.get("category").and_then(Value::as_str) == Some("premade")
            || item.get("is_owner") == Some(&Value::Bool(true));
        let mut langs = vec![primary.to_string()];
        if let Some(verified) = item.get("verified_languages").and_then(Value::as_array) {
            for entry in verified {
                if let Some(code) = entry.get("language").and_then(Value::as_str) {
                    if !code.is_empty() && code != primary && !langs.iter().any(|lang| lang == code)
                    {
                        langs.push(code.to_string());
                    }
                }
            }
        }
        for lang in langs {
            entries.push(json!({
                "id": id,
                "name": name,
                "gender": gender,
                "lang": lang,
                "free_users_allowed": free_users_allowed,
            }));
        }
    }
    entries
}

/// Upstream `api/media-providers/tts/deepgram/voices`: each Deepgram TTS model
/// is one voice, keyed by `canonical_name`.
fn deepgram_entries(raw: &Value) -> Vec<Value> {
    let Some(items) = raw.get("tts").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in items {
        let canonical = item
            .get("canonical_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let fallback = item.get("name").and_then(Value::as_str).unwrap_or("");
        let id = if !canonical.is_empty() {
            canonical
        } else {
            fallback
        };
        if id.is_empty() {
            continue;
        }
        let name = if !fallback.is_empty() { fallback } else { id };
        let gender = item
            .pointer("/metadata/tags")
            .and_then(Value::as_array)
            .and_then(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .find(|tag| *tag == "masculine" || *tag == "feminine")
            })
            .unwrap_or("");
        let langs: Vec<String> = match item.get("languages").and_then(Value::as_array) {
            Some(languages) if !languages.is_empty() => languages
                .iter()
                .filter_map(|language| language.as_str().map(str::to_string))
                .collect(),
            _ => vec![canonical
                .rsplit('-')
                .next()
                .filter(|suffix| !suffix.is_empty())
                .unwrap_or("en")
                .to_string()],
        };
        for lang in langs {
            entries.push(json!({
                "id": id,
                "name": name,
                "gender": gender,
                "lang": lang,
            }));
        }
    }
    entries
}

/// Upstream `api/media-providers/tts/inworld/voices`.
fn inworld_entries(raw: &Value) -> Vec<Value> {
    let Some(items) = raw.get("voices").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in items {
        let Some(id) = item.get("voiceId").and_then(Value::as_str) else {
            continue;
        };
        let name = item
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(id);
        let gender = item.get("gender").and_then(Value::as_str).unwrap_or("");
        let langs: Vec<String> = match item.get("languages").and_then(Value::as_array) {
            Some(languages) if !languages.is_empty() => languages
                .iter()
                .filter_map(|language| language.as_str().map(str::to_string))
                .collect(),
            _ => vec!["en".to_string()],
        };
        for lang in langs {
            entries.push(json!({
                "id": id,
                "name": name,
                "gender": gender,
                "lang": lang,
            }));
        }
    }
    entries
}

/// Host voices of the machine running the backend, mirroring upstream
/// `fetchVoicesWin`/`fetchVoicesMac` merged with the generic route mapping.
fn local_device_voices() -> Vec<Value> {
    #[cfg(target_os = "windows")]
    {
        windows_voices()
    }
    #[cfg(not(target_os = "windows"))]
    {
        macos_voices()
    }
}

#[cfg(target_os = "windows")]
fn windows_voices() -> Vec<Value> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            WINDOWS_VOICES_SCRIPT,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let parsed: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
            windows_voice_entries(&parsed)
        }
        _ => Vec::new(),
    }
}

#[cfg(not(target_os = "windows"))]
fn macos_voices() -> Vec<Value> {
    match Command::new("say").args(["-v", "?"]).output() {
        Ok(output) if output.status.success() => {
            macos_voice_entries(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    }
}

fn windows_voice_entries(parsed: &Value) -> Vec<Value> {
    let items = match parsed {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![parsed.clone()],
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("Name").and_then(Value::as_str)?;
            let culture = item
                .get("Culture")
                .and_then(Value::as_str)
                .filter(|culture| !culture.is_empty())
                .unwrap_or("en-US");
            let mut parts = culture.splitn(2, '-');
            let lang = parts.next().unwrap_or("");
            let country = parts.next().unwrap_or("");
            let gender = match item.get("Gender") {
                Some(Value::Number(number)) => match number.as_i64() {
                    Some(1) => "Male",
                    Some(2) => "Female",
                    _ => "",
                },
                Some(Value::String(value)) if value == "Male" || value == "Female" => value,
                _ => "",
            };
            Some(json!({
                "id": name,
                "name": name,
                // The fetcher rewrites the first `-` to `_` and the route
                // rewrites it back, so the served locale is the culture name.
                "locale": culture,
                "lang": lang,
                "country": country,
                "countryName": country_name(if country.is_empty() { lang } else { country }),
                "langName": language_name(lang),
                "gender": gender,
            }))
        })
        .collect()
}

static SAY_VOICE_LINE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"^(\S.*?)\s{2,}([a-z]{2}_[A-Z]{2})").expect("static say-voice regex")
});

fn macos_voice_entries(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| {
            let captures = SAY_VOICE_LINE.captures(line)?;
            let name = captures.get(1)?.as_str().trim();
            let locale = captures.get(2)?.as_str().trim();
            let mut parts = locale.splitn(2, '_');
            let lang = parts.next().unwrap_or("");
            let country = parts.next().unwrap_or("");
            Some(json!({
                "id": name,
                "name": name,
                "locale": locale.replacen('_', "-", 1),
                "lang": lang,
                "country": country,
                "countryName": country_name(if country.is_empty() { lang } else { country }),
                "langName": language_name(lang),
                "gender": "",
            }))
        })
        .collect()
}

/// Insertion-ordered language buckets with upstream per-language id de-duplication.
fn group_by_lang(voices: &[Value]) -> Vec<(String, Vec<Value>)> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for voice in voices {
        let code = voice
            .get("lang")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = voice.get("id").and_then(Value::as_str).unwrap_or("");
        match groups.iter().position(|(existing, _)| *existing == code) {
            Some(index) => {
                let bucket = &mut groups[index].1;
                let duplicate = bucket
                    .iter()
                    .any(|existing| existing.get("id").and_then(Value::as_str) == Some(id));
                if !duplicate {
                    bucket.push(voice.clone());
                }
            }
            None => groups.push((code, vec![voice.clone()])),
        }
    }
    groups
}

/// `{code, name, voices}` objects shared by `languages` and `byLang`.
fn language_objects(groups: &[(String, Vec<Value>)]) -> Vec<Value> {
    let mut languages: Vec<Value> = groups
        .iter()
        .map(|(code, voices)| {
            json!({
                "code": code,
                "name": language_name(code),
                "voices": voices,
            })
        })
        .collect();
    languages.sort_by(|left, right| {
        let left = left.get("name").and_then(Value::as_str).unwrap_or("");
        let right = right.get("name").and_then(Value::as_str).unwrap_or("");
        left.cmp(right)
    });
    languages
}

fn language_map(languages: &[Value]) -> Value {
    let mut by_lang = Map::new();
    for language in languages {
        if let Some(code) = language.get("code").and_then(Value::as_str) {
            by_lang.insert(code.to_string(), language.clone());
        }
    }
    Value::Object(by_lang)
}

/// Upstream generic route body: `{voices, languages, byLang}`.
fn generic_payload(voices: &[Value]) -> Value {
    let languages = language_objects(&group_by_lang(voices));
    json!({
        "voices": voices,
        "languages": languages,
        "byLang": language_map(&languages),
    })
}

/// Upstream dedicated-route body: `{voices}` when filtered, else `{languages, byLang}`.
fn dedicated_payload(groups: &[(String, Vec<Value>)], lang: Option<&str>) -> Value {
    match lang {
        Some(code) => {
            let voices = groups
                .iter()
                .find(|(existing, _)| existing == code)
                .map(|(_, voices)| voices.clone())
                .unwrap_or_default();
            json!({"voices": voices})
        }
        None => {
            let languages = language_objects(groups);
            json!({
                "languages": languages,
                "byLang": language_map(&languages),
            })
        }
    }
}

/// Upstream public projection over the internal payload.
fn public_data(groups: &[(String, Vec<Value>)], provider: &str, lang: Option<&str>) -> Vec<Value> {
    let alias = provider_alias(provider);
    let selected: Vec<&Value> = match lang {
        Some(code) => groups
            .iter()
            .find(|(existing, _)| existing == code)
            .map(|(_, voices)| voices.iter().collect())
            .unwrap_or_default(),
        None => groups
            .iter()
            .flat_map(|(_, voices)| voices.iter())
            .collect(),
    };
    selected
        .iter()
        .map(|voice| {
            let id = voice.get("id").and_then(Value::as_str).unwrap_or("");
            json!({
                "id": id,
                "name": voice.get("name").and_then(Value::as_str).unwrap_or(""),
                "lang": voice.get("lang").and_then(Value::as_str).unwrap_or(""),
                "gender": voice.get("gender").and_then(Value::as_str).unwrap_or(""),
                "model": format!("{alias}/{id}"),
            })
        })
        .collect()
}

fn country_name(code: &str) -> String {
    let key = code.to_uppercase();
    REGION_NAMES
        .get(key.as_str())
        .copied()
        .unwrap_or(code)
        .to_string()
}

/// Upstream `Intl.DisplayNames(["en"], {type: "language"})` for ISO-639-1 codes.
/// `Intl` resolves language/region subtags case-insensitively, so lookups do too.
fn language_name(code: &str) -> String {
    if let Some((base, region)) = code.split_once('-') {
        return format!("{} ({})", language_name(base), country_name(region));
    }
    let key = code.to_lowercase();
    LANGUAGE_NAMES
        .get(key.as_str())
        .copied()
        .unwrap_or(code)
        .to_string()
}

static LANGUAGE_NAMES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    LANGUAGE_NAME_DATA
        .split(',')
        .filter_map(|entry| entry.split_once('='))
        .collect()
});

static REGION_NAMES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    REGION_NAME_DATA
        .split(',')
        .filter_map(|entry| entry.split_once('='))
        .collect()
});

// Generated from `Intl.DisplayNames(["en"], { type: "language" })` over ISO-639-1.
static LANGUAGE_NAME_DATA: &str = "\
    aa=Afar,ab=Abkhazian,ae=Avestan,af=Afrikaans,ak=Akan,am=Amharic,an=Aragonese,as=Assamese,av=Avaric,ay=Aymara,az=Azerbaijani,ba=Bashkir,be=Belarusian,bg=Bulgarian,bh=Bhojpuri,bi=Bislama,bm=Bambara,\
    bn=Bangla,bo=Tibetan,br=Breton,bs=Bosnian,ca=Catalan,ce=Chechen,ch=Chamorro,co=Corsican,cr=Cree,cs=Czech,cu=Church Slavic,cv=Chuvash,cy=Welsh,da=Danish,de=German,dv=Divehi,dz=Dzongkha,ee=Ewe,el=Greek,\
    en=English,eo=Esperanto,es=Spanish,et=Estonian,eu=Basque,fa=Persian,ff=Fula,fi=Finnish,fj=Fijian,fo=Faroese,fr=French,fy=Western Frisian,ga=Irish,gd=Scottish Gaelic,gl=Galician,gn=Guarani,gu=Gujarati,\
    gv=Manx,ha=Hausa,he=Hebrew,hi=Hindi,ho=Hiri Motu,hr=Croatian,ht=Haitian Creole,hu=Hungarian,hy=Armenian,hz=Herero,ia=Interlingua,id=Indonesian,ie=Interlingue,ig=Igbo,ii=Sichuan Yi,ik=Inupiaq,io=Ido,\
    is=Icelandic,it=Italian,iu=Inuktitut,ja=Japanese,jv=Javanese,ka=Georgian,kg=Kongo,ki=Kikuyu,kj=Kuanyama,kk=Kazakh,kl=Kalaallisut,km=Khmer,kn=Kannada,ko=Korean,kr=Kanuri,ks=Kashmiri,ku=Kurdish,kv=Komi,\
    kw=Cornish,ky=Kyrgyz,la=Latin,lb=Luxembourgish,lg=Ganda,li=Limburgish,ln=Lingala,lo=Lao,lt=Lithuanian,lu=Luba-Katanga,lv=Latvian,mg=Malagasy,mh=Marshallese,mi=Māori,mk=Macedonian,ml=Malayalam,\
    mn=Mongolian,mr=Marathi,ms=Malay,mt=Maltese,my=Burmese,na=Nauru,nb=Norwegian Bokmål,nd=North Ndebele,ne=Nepali,ng=Ndonga,nl=Dutch,nn=Norwegian Nynorsk,no=Norwegian,nr=South Ndebele,nv=Navajo,\
    ny=Nyanja,oc=Occitan,oj=Ojibwa,om=Oromo,or=Odia,os=Ossetic,pa=Punjabi,pi=Pali,pl=Polish,ps=Pashto,pt=Portuguese,qu=Quechua,rm=Romansh,rn=Rundi,ro=Romanian,ru=Russian,rw=Kinyarwanda,sa=Sanskrit,\
    sc=Sardinian,sd=Sindhi,se=Northern Sami,sg=Sango,si=Sinhala,sk=Slovak,sl=Slovenian,sm=Samoan,sn=Shona,so=Somali,sq=Albanian,sr=Serbian,ss=Swati,st=Southern Sotho,su=Sundanese,sv=Swedish,sw=Swahili,\
    ta=Tamil,te=Telugu,tg=Tajik,th=Thai,ti=Tigrinya,tk=Turkmen,tl=Filipino,tn=Tswana,to=Tongan,tr=Turkish,ts=Tsonga,tt=Tatar,tw=Akan,ty=Tahitian,ug=Uyghur,uk=Ukrainian,ur=Urdu,uz=Uzbek,ve=Venda,\
    vi=Vietnamese,vo=Volapük,wa=Walloon,wo=Wolof,xh=Xhosa,yi=Yiddish,yo=Yoruba,za=Zhuang,zh=Chinese,zu=Zulu";

// Generated from `Intl.DisplayNames(["en"], { type: "region" })` over ISO-3166 alpha-2.
static REGION_NAME_DATA: &str = "\
    AD=Andorra,AE=United Arab Emirates,AF=Afghanistan,AG=Antigua & Barbuda,AI=Anguilla,AL=Albania,AM=Armenia,AO=Angola,AQ=Antarctica,AR=Argentina,AS=American Samoa,AT=Austria,\
    AU=Australia,AW=Aruba,AX=Åland Islands,AZ=Azerbaijan,BA=Bosnia & Herzegovina,BB=Barbados,BD=Bangladesh,BE=Belgium,BF=Burkina Faso,BG=Bulgaria,BH=Bahrain,BI=Burundi,BJ=Benin,\
    BL=St. Barthélemy,BM=Bermuda,BN=Brunei,BO=Bolivia,BQ=Caribbean Netherlands,BR=Brazil,BS=Bahamas,BT=Bhutan,BV=Bouvet Island,BW=Botswana,BY=Belarus,BZ=Belize,CA=Canada,\
    CC=Cocos (Keeling) Islands,CD=Congo - Kinshasa,CF=Central African Republic,CG=Congo - Brazzaville,CH=Switzerland,CI=Côte d’Ivoire,CK=Cook Islands,CL=Chile,CM=Cameroon,\
    CN=China,CO=Colombia,CR=Costa Rica,CU=Cuba,CV=Cape Verde,CW=Curaçao,CX=Christmas Island,CY=Cyprus,CZ=Czechia,DE=Germany,DJ=Djibouti,DK=Denmark,DM=Dominica,DO=Dominican Republic,\
    DZ=Algeria,EC=Ecuador,EE=Estonia,EG=Egypt,EH=Western Sahara,ER=Eritrea,ES=Spain,ET=Ethiopia,FI=Finland,FJ=Fiji,FK=Falkland Islands,FM=Micronesia,FO=Faroe Islands,FR=France,GA=Gabon,\
    GB=United Kingdom,GD=Grenada,GE=Georgia,GF=French Guiana,GG=Guernsey,GH=Ghana,GI=Gibraltar,GL=Greenland,GM=Gambia,GN=Guinea,GP=Guadeloupe,GQ=Equatorial Guinea,GR=Greece,\
    GS=South Georgia & South Sandwich Islands,GT=Guatemala,GU=Guam,GW=Guinea-Bissau,GY=Guyana,HK=Hong Kong SAR China,HM=Heard & McDonald Islands,HN=Honduras,HR=Croatia,HT=Haiti,\
    HU=Hungary,ID=Indonesia,IE=Ireland,IL=Israel,IM=Isle of Man,IN=India,IO=British Indian Ocean Territory,IQ=Iraq,IR=Iran,IS=Iceland,IT=Italy,JE=Jersey,JM=Jamaica,\
    JO=Jordan,JP=Japan,KE=Kenya,KG=Kyrgyzstan,KH=Cambodia,KI=Kiribati,KM=Comoros,KN=St. Kitts & Nevis,KP=North Korea,KR=South Korea,KW=Kuwait,KY=Cayman Islands,KZ=Kazakhstan,LA=Laos,\
    LB=Lebanon,LC=St. Lucia,LI=Liechtenstein,LK=Sri Lanka,LR=Liberia,LS=Lesotho,LT=Lithuania,LU=Luxembourg,LV=Latvia,LY=Libya,MA=Morocco,MC=Monaco,MD=Moldova,\
    ME=Montenegro,MF=St. Martin,MG=Madagascar,MH=Marshall Islands,MK=North Macedonia,ML=Mali,MM=Myanmar (Burma),MN=Mongolia,MO=Macao SAR China,MP=Northern Mariana Islands,MQ=Martinique,\
    MR=Mauritania,MS=Montserrat,MT=Malta,MU=Mauritius,MV=Maldives,MW=Malawi,MX=Mexico,MY=Malaysia,MZ=Mozambique,NA=Namibia,NC=New Caledonia,NE=Niger,NF=Norfolk Island,NG=Nigeria,\
    NI=Nicaragua,NL=Netherlands,NO=Norway,NP=Nepal,NR=Nauru,NU=Niue,NZ=New Zealand,OM=Oman,PA=Panama,PE=Peru,PF=French Polynesia,PG=Papua New Guinea,PH=Philippines,PK=Pakistan,\
    PL=Poland,PM=St. Pierre & Miquelon,PN=Pitcairn Islands,PR=Puerto Rico,PS=Palestinian Territories,PT=Portugal,PW=Palau,PY=Paraguay,QA=Qatar,RE=Réunion,RO=Romania,RS=Serbia,RU=Russia,RW=Rwanda,\
    SA=Saudi Arabia,SB=Solomon Islands,SC=Seychelles,SD=Sudan,SE=Sweden,SG=Singapore,SH=St. Helena,SI=Slovenia,SJ=Svalbard & Jan Mayen,SK=Slovakia,SL=Sierra Leone,SM=San Marino,SN=Senegal,\
    SO=Somalia,SR=Suriname,SS=South Sudan,ST=São Tomé & Príncipe,SV=El Salvador,SX=Sint Maarten,SY=Syria,SZ=Eswatini,TC=Turks & Caicos Islands,TD=Chad,TF=French Southern Territories,TG=Togo,\
    TH=Thailand,TJ=Tajikistan,TK=Tokelau,TL=Timor-Leste,TM=Turkmenistan,TN=Tunisia,TO=Tonga,TR=Türkiye,TT=Trinidad & Tobago,TV=Tuvalu,TW=Taiwan,TZ=Tanzania,UA=Ukraine,UG=Uganda,\
    UM=U.S. Outlying Islands,US=United States,UY=Uruguay,UZ=Uzbekistan,VA=Vatican City,VC=St. Vincent & Grenadines,VE=Venezuela,VG=British Virgin Islands,VI=U.S. Virgin Islands,VN=Vietnam,\
    VU=Vanuatu,WF=Wallis & Futuna,WS=Samoa,YE=Yemen,YT=Mayotte,ZA=South Africa,ZM=Zambia,ZW=Zimbabwe";

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn public_provider_hint_matches_upstream_message() {
        assert_eq!(
            provider_hint(),
            "provider must be one of: elevenlabs, deepgram, inworld, edge-tts, local-device"
        );
    }

    #[test]
    fn public_aliases_follow_the_provider_registry() {
        assert_eq!(provider_alias("elevenlabs"), "el");
        assert_eq!(provider_alias("deepgram"), "dg");
        assert_eq!(provider_alias("inworld"), "inworld");
        assert_eq!(provider_alias("edge-tts"), "edge-tts");
        assert_eq!(provider_alias("local-device"), "local-device");
    }

    #[test]
    fn voice_sources_split_between_public_and_generic_routes() {
        for provider in PUBLIC_VOICE_PROVIDERS {
            assert!(VoiceSource::known(provider).is_some(), "{provider}");
        }
        assert!(VoiceSource::known("gemini").is_none());
        assert_eq!(VoiceSource::generic("edge-tts"), Some(VoiceSource::EdgeTts));
        assert_eq!(
            VoiceSource::generic("local-device"),
            Some(VoiceSource::LocalDevice)
        );
        assert_eq!(VoiceSource::generic("elevenlabs"), None);
    }

    #[test]
    fn edge_tts_entries_clean_names_and_split_locales() {
        let raw = json!([{
            "ShortName": "es-MX-ThaliaNeural",
            "FriendlyName": "Microsoft Thalia Online (Natural) - Spanish (Mexico)",
            "Locale": "es-MX",
            "Gender": "Female",
        }]);
        let voices = edge_tts_entries(&raw);
        assert_eq!(voices.len(), 1);
        assert_eq!(
            voices[0],
            json!({
                "id": "es-MX-ThaliaNeural",
                "name": "Thalia (Spanish (Mexico)",
                "locale": "es-MX",
                "lang": "es",
                "country": "MX",
                "countryName": "Mexico",
                "langName": "Spanish",
                "gender": "Female",
            })
        );
    }

    #[test]
    fn elevenlabs_entries_cover_primary_and_verified_languages() {
        let raw = json!({"voices": [{
            "voice_id": "voice-1",
            "name": "Rachel",
            "labels": {"gender": "female", "language": "en"},
            "category": "premade",
            "verified_languages": [{"language": "es"}, {"language": "en"}],
        }]});
        let voices = elevenlabs_entries(&raw);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0]["lang"], "en");
        assert_eq!(voices[1]["lang"], "es");
        assert_eq!(voices[0]["free_users_allowed"], true);
        assert_eq!(voices[0]["name"], "Rachel");
        assert_eq!(voices[0]["gender"], "female");
    }

    #[test]
    fn deepgram_entries_infer_language_and_gender() {
        let raw = json!({"tts": [
            {
                "canonical_name": "aura-2-thalia-en",
                "name": "Thalia",
                "metadata": {"tags": ["masculine", "female"]},
            },
            {
                "canonical_name": "aura-2-orpheus-en",
                "name": "Orpheus",
                "languages": ["en", "es"],
            },
        ]});
        let voices = deepgram_entries(&raw);
        assert_eq!(voices.len(), 3);
        assert_eq!(
            voices[0],
            json!({"id": "aura-2-thalia-en", "name": "Thalia", "gender": "masculine", "lang": "en"})
        );
        assert_eq!(voices[1]["lang"], "en");
        assert_eq!(voices[2]["lang"], "es");
        assert_eq!(voices[2]["gender"], "");
    }

    #[test]
    fn inworld_entries_default_to_english() {
        let raw =
            json!({"voices": [{"voiceId": "Brian", "displayName": "Brian", "gender": "male"}]});
        let voices = inworld_entries(&raw);
        assert_eq!(
            voices,
            vec![json!({"id": "Brian", "name": "Brian", "gender": "male", "lang": "en"})]
        );
    }

    #[test]
    fn windows_voices_map_sapi_gender_values() {
        let raw = json!([
            {"Name": "Microsoft Zira Desktop", "Culture": "en-US", "Gender": 2},
            {"Name": "Microsoft Hedda", "Culture": "de", "Gender": "Male"},
        ]);
        let voices = windows_voice_entries(&raw);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0]["locale"], "en-US");
        assert_eq!(voices[0]["lang"], "en");
        assert_eq!(voices[0]["country"], "US");
        assert_eq!(voices[0]["countryName"], "United States");
        assert_eq!(voices[0]["langName"], "English");
        assert_eq!(voices[0]["gender"], "Female");
        assert_eq!(voices[1]["country"], "");
        assert_eq!(voices[1]["countryName"], "Germany");
        assert_eq!(voices[1]["gender"], "Male");
    }

    #[test]
    fn macos_voices_parse_say_output() {
        let stdout = "Alex                en_US    # Most people recognize me by my voice.\n\
                      Zosia               pl_PL    # Zosia (Enhanced)\n\
                      malformed line without locale\n";
        let voices = macos_voice_entries(stdout);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0]["id"], "Alex");
        assert_eq!(voices[0]["locale"], "en-US");
        assert_eq!(voices[0]["lang"], "en");
        assert_eq!(voices[0]["country"], "US");
        assert_eq!(voices[0]["countryName"], "United States");
        assert_eq!(voices[0]["gender"], "");
        assert_eq!(voices[1]["id"], "Zosia");
        assert_eq!(voices[1]["lang"], "pl");
    }

    #[test]
    fn group_by_lang_keeps_insertion_order_and_dedupes_ids() {
        let voices = vec![
            json!({"id": "a", "lang": "es"}),
            json!({"id": "b", "lang": "en"}),
            json!({"id": "a", "lang": "es"}),
            json!({"id": "c", "lang": "es"}),
        ];
        let groups = group_by_lang(&voices);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "es");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "en");
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn generic_payload_returns_languages_and_by_lang() {
        let voices = vec![
            json!({"id": "a", "name": "A", "lang": "es", "gender": "Female"}),
            json!({"id": "b", "name": "B", "lang": "en", "gender": "Male"}),
        ];
        let payload = generic_payload(&voices);
        assert_eq!(payload["voices"].as_array().map(Vec::len), Some(2));
        let languages = payload["languages"].as_array().expect("languages");
        assert_eq!(languages[0]["code"], "en");
        assert_eq!(languages[0]["name"], "English");
        assert_eq!(languages[1]["code"], "es");
        assert_eq!(payload["byLang"]["es"]["code"], "es");
        assert_eq!(payload["byLang"]["es"]["name"], "Spanish");
    }

    #[test]
    fn dedicated_payload_filters_requested_language() {
        let groups = group_by_lang(&[
            json!({"id": "a", "name": "A", "lang": "en"}),
            json!({"id": "b", "name": "B", "lang": "es"}),
        ]);
        let filtered = dedicated_payload(&groups, Some("es"));
        assert_eq!(filtered["voices"].as_array().map(Vec::len), Some(1));
        assert!(filtered.get("languages").is_none());

        let full = dedicated_payload(&groups, None);
        assert!(full.get("voices").is_none());
        assert_eq!(full["byLang"]["en"]["code"], "en");
    }

    #[test]
    fn public_data_projects_alias_models_and_language_filter() {
        let groups = group_by_lang(&[
            json!({"id": "abc", "name": "Rachel", "lang": "en", "gender": "female"}),
            json!({"id": "xyz", "name": "Monika", "lang": "es", "gender": "female"}),
        ]);
        let all = public_data(&groups, "elevenlabs", None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["model"], "el/abc");
        assert_eq!(all[0]["lang"], "en");
        assert_eq!(all[1]["model"], "el/xyz");

        let filtered = public_data(&groups, "deepgram", Some("es"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["model"], "dg/xyz");
    }

    #[test]
    fn language_and_region_names_follow_intl_display_names() {
        assert_eq!(language_name("en"), "English");
        assert_eq!(language_name("vi"), "Vietnamese");
        assert_eq!(language_name("zh-CN"), "Chinese (China)");
        assert_eq!(language_name("xx"), "xx");
        assert_eq!(country_name("MX"), "Mexico");
        assert_eq!(country_name("ZZ"), "ZZ");
    }
}
