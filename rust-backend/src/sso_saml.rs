//! Native SAML 2.0 SSO routes (`/api/auth/saml/**`), ported from the pinned
//! upstream `src/app/api/auth/saml/**` route files and `lib/auth/saml.js`,
//! which delegate to `@node-saml/node-saml` 5.1.0. Redirect contracts, the
//! login-limiter coupling, the InResponseTo replay check, and the session
//! claims all mirror upstream; signature verification lives in
//! [`crate::sso_saml_xml`].
//!
//! One deliberate deviation: upstream's `validatePostResponseAsync` returns a
//! null profile for `NoPassive`/`LogoutResponse` messages, which the route
//! then treats as a successful login. This port rejects those messages
//! instead of minting a session from them.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Map, Value};
use std::io::Write as _;

use crate::{
    auth,
    error::AppError,
    login_limiter,
    sso::{
        clear_cookie_header, redirect_response, request_origin, saml_base_url, set_cookie_header,
        urlencode,
    },
    sso_saml_xml,
    state::AppState,
};

const SAML_STATE_COOKIE: &str = "saml_state";
const DEFAULT_SAML_ISSUER: &str = "urn:9router:sp";
const ACCEPTED_CLOCK_SKEW_MS: i64 = 60_000;
const HTTP_POST_BINDING: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";

pub struct SamlRuntimeConfig {
    pub entry_point: String,
    pub issuer: String,
    pub cert: String,
    pub attribute_email: String,
    pub attribute_name: String,
}

pub fn is_saml_configured(settings: &Value) -> bool {
    !settings
        .get("samlEntryPoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
        && !settings
            .get("samlCert")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
}

pub fn saml_runtime_config(settings: &Value) -> SamlRuntimeConfig {
    SamlRuntimeConfig {
        entry_point: str_setting(settings, "samlEntryPoint").trim().to_string(),
        issuer: first_non_empty(&[&str_setting(settings, "samlIssuer"), DEFAULT_SAML_ISSUER]),
        cert: str_setting(settings, "samlCert").trim().to_string(),
        attribute_email: first_non_empty(&[&str_setting(settings, "samlAttributeEmail"), "email"]),
        attribute_name: first_non_empty(&[&str_setting(settings, "samlAttributeName"), "name"]),
    }
}

fn str_setting(settings: &Value, key: &str) -> String {
    settings
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /api/auth/saml/start
// ---------------------------------------------------------------------------

pub async fn handle_start(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let origin = saml_base_url(headers, uri, &settings);
    let config = saml_runtime_config(&settings);
    if !is_saml_configured(&settings) {
        return Ok(redirect_response(
            &format!("{origin}/login?error=saml_not_configured"),
            Vec::new(),
        ));
    }
    match build_authorize_url(&config, &origin) {
        Ok((authorize_url, request_id)) => Ok(redirect_response(
            &authorize_url,
            vec![set_cookie_header(
                headers,
                SAML_STATE_COOKIE,
                &request_id,
                600,
            )],
        )),
        Err(message) => Ok(redirect_response(
            &format!("{origin}/login?error={}", urlencode(&message)),
            Vec::new(),
        )),
    }
}

fn generate_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("_{hex}")
}

/// Builds the deflated `SAMLRequest` redirect URL and the AuthnRequest ID,
/// mirroring node-saml's `generateAuthorizeRequestAsync` +
/// `_requestToUrlAsync` with the options the upstream app passes.
fn build_authorize_url(
    config: &SamlRuntimeConfig,
    origin: &str,
) -> Result<(String, String), String> {
    let callback_url = format!("{origin}/api/auth/saml/acs");
    let request_id = generate_request_id();
    let instant = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let xml = format!(
        "<samlp:AuthnRequest xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" ID=\"{request_id}\" Version=\"2.0\" IssueInstant=\"{instant}\" ProtocolBinding=\"{HTTP_POST_BINDING}\" Destination=\"{destination}\" AssertionConsumerServiceURL=\"{callback}\"><saml:Issuer xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\">{issuer}</saml:Issuer><samlp:NameIDPolicy xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" AllowCreate=\"true\" Format=\"urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress\"/><samlp:RequestedAuthnContext xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" Comparison=\"exact\"><saml:AuthnContextClassRef xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\">urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></samlp:RequestedAuthnContext></samlp:AuthnRequest>",
        destination = xml_escape(&config.entry_point),
        callback = xml_escape(&callback_url),
        issuer = xml_escape(&config.issuer),
    );

    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    encoder
        .write_all(xml.as_bytes())
        .map_err(|e| e.to_string())?;
    let deflated = encoder.finish().map_err(|e| e.to_string())?;
    let saml_request = STANDARD.encode(deflated);

    let mut url =
        url::Url::parse(&config.entry_point).map_err(|e| format!("Invalid samlEntryPoint: {e}"))?;
    url.query_pairs_mut()
        .append_pair("SAMLRequest", &saml_request);
    Ok((url.to_string(), request_id))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// POST /api/auth/saml/acs
// ---------------------------------------------------------------------------

pub async fn handle_acs(
    state: &AppState,
    peer: std::net::SocketAddr,
    headers: &HeaderMap,
    uri: &Uri,
    raw: &[u8],
) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let origin = saml_base_url(headers, uri, &settings);
    let ip = auth::rate_limit_ip(peer, headers);

    let lock = login_limiter::check_lock(ip);
    if lock.locked {
        return Ok(redirect_response(
            &format!(
                "{origin}/login?error={}",
                urlencode(&format!(
                    "Too many failed attempts. Try again in {}s.",
                    lock.retry_after_secs
                ))
            ),
            Vec::new(),
        ));
    }

    let stored_request_id = auth::cookie(headers, SAML_STATE_COOKIE).unwrap_or_default();
    let clear_state = clear_cookie_header(headers, SAML_STATE_COOKIE);

    let saml_response = url::form_urlencoded::parse(raw)
        .find(|(key, _)| key == "SAMLResponse")
        .map(|(_, value)| value.into_owned());

    let Some(saml_response) = saml_response else {
        login_limiter::record_failure(ip);
        return Ok(redirect_response(
            &format!("{origin}/login?error=saml_missing_response"),
            vec![clear_state],
        ));
    };

    if !is_saml_configured(&settings) {
        login_limiter::record_failure(ip);
        return Ok(redirect_response(
            &format!("{origin}/login?error=saml_not_configured"),
            vec![clear_state],
        ));
    }

    let config = saml_runtime_config(&settings);
    match validate_saml_response(&saml_response, Some(&stored_request_id), &config) {
        Ok(profile) => {
            let saml_email = pick_saml_email(&profile, &config);
            let saml_name = pick_saml_display_name(&profile, &config)
                .unwrap_or_else(|| "SAML user".to_string());
            login_limiter::record_success(ip);
            let mut claims = Map::new();
            claims.insert("saml".into(), Value::Bool(true));
            claims.insert(
                "samlEmail".into(),
                saml_email.map(Value::String).unwrap_or(Value::Null),
            );
            claims.insert("samlName".into(), Value::String(saml_name));
            let token = auth::create_session_token(state, claims)?;
            Ok(redirect_response(
                &format!("{origin}/dashboard"),
                vec![clear_state, auth::session_cookie_header(headers, &token)],
            ))
        }
        Err(message) => {
            login_limiter::record_failure(ip);
            Ok(redirect_response(
                &format!("{origin}/login?error={}", urlencode(&message)),
                vec![clear_state],
            ))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SamlProfile {
    pub fields: Vec<(String, String)>,
}

impl SamlProfile {
    pub fn get(&self, key: &str) -> Option<&String> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Port of upstream `validateSamlResponse` + node-saml
/// `validatePostResponseAsync` with the options the app configures
/// (`wantAssertionsSigned`, `wantAuthnResponseSigned`, 60s clock skew,
/// audience = SP issuer, no decryption, cache-free InResponseTo checking).
pub fn validate_saml_response(
    saml_response: &str,
    expected_request_id: Option<&str>,
    config: &SamlRuntimeConfig,
) -> Result<SamlProfile, String> {
    if config.cert.is_empty() {
        return Err("IdP X.509 Certificate (samlCert) is missing or not configured".into());
    }

    let xml_bytes = STANDARD
        .decode(saml_response.trim())
        .map_err(|e| format!("Invalid SAMLResponse base64: {e}"))?;
    let xml = String::from_utf8(xml_bytes)
        .map_err(|_| -> String { "SAMLResponse is not UTF-8".into() })?;
    let doc = sso_saml_xml::XmlDocument::parse(&xml)?;
    let response_element = doc
        .root_element()
        .ok_or("SAMLResponse contains no XML root")?;

    if let Some(expected) = expected_request_id.filter(|value| !value.is_empty()) {
        let in_response_to = doc
            .attrs_of(response_element)
            .iter()
            .find(|a| a.prefix.is_none() && a.local == "InResponseTo")
            .map(|a| a.value.clone());
        if in_response_to.as_deref() != Some(expected) {
            return Err(format!(
                "InResponseTo mismatch: expected {}, received {}",
                expected,
                in_response_to.unwrap_or_else(|| "none".into())
            ));
        }
    }

    let public_key =
        sso_saml_xml::parse_certificate_pem(&sso_saml_xml::format_x509_certificate(&config.cert))?;

    // Top-level response signature is mandatory (wantAuthnResponseSigned).
    let response_verified = sso_saml_xml::get_verified_xml(&doc, response_element, &public_key)?;
    if response_verified.is_none() {
        return Err("Invalid document signature".into());
    }

    let child_elements = doc.child_elements(response_element);
    let assertions: Vec<usize> = child_elements
        .iter()
        .copied()
        .filter(|&child| doc.element_local(child) == "Assertion")
        .collect();
    let encrypted_assertions = child_elements
        .iter()
        .copied()
        .filter(|&child| doc.element_local(child) == "EncryptedAssertion")
        .collect::<Vec<_>>();

    if assertions.len() + encrypted_assertions.len() > 1 {
        return Err("Invalid signature: multiple assertions".into());
    }

    let mut assertion_verified: Option<String> = None;
    if assertions.len() == 1 {
        assertion_verified = sso_saml_xml::get_verified_xml(&doc, assertions[0], &public_key)?;
        if assertion_verified.is_none() {
            return Err("Invalid signature".into());
        }
    }
    if encrypted_assertions.len() == 1 {
        return Err("No decryption key for encrypted SAML response".into());
    }

    if assertions.is_empty() {
        // No assertion: reproduce the upstream status handling, rejecting the
        // NoPassive/LogoutResponse success paths upstream turns into logins.
        if doc.element_local(response_element) == "Response" {
            return Err(status_error_message(&doc, response_element));
        }
        return Err("Unknown SAML response message".into());
    }

    let verified_xml = response_verified
        .or(assertion_verified)
        .ok_or("Invalid signature")?;
    let verified_doc = sso_saml_xml::XmlDocument::parse(&verified_xml)?;
    let verified_root = verified_doc
        .root_element()
        .ok_or("Cannot obtain assertion from signed data")?;
    let assertion_element = if verified_doc.element_local(verified_root) == "Response" {
        let mut assertion = verified_doc
            .child_elements(verified_root)
            .into_iter()
            .filter(|&child| verified_doc.element_local(child) == "Assertion")
            .collect::<Vec<_>>();
        if assertion.len() != 1 {
            return Err("Cannot obtain assertion from signed data".into());
        }
        assertion.remove(0)
    } else {
        verified_root
    };

    process_signed_assertion(&verified_doc, assertion_element, config)
}

/// Port of node-saml's status handling for responses without an assertion:
/// `Responder`/`NoPassive` and non-`Success` codes become descriptive errors.
fn status_error_message(doc: &sso_saml_xml::XmlDocument, response: usize) -> String {
    let status = doc
        .child_elements(response)
        .into_iter()
        .find(|&child| doc.element_local(child) == "Status");
    let Some(status) = status else {
        return "Missing SAML assertion".into();
    };
    let status_code = doc
        .child_elements(status)
        .into_iter()
        .find(|&child| doc.element_local(child) == "StatusCode");
    let Some(status_code) = status_code else {
        return "Missing SAML assertion".into();
    };
    let value = doc
        .attrs_of(status_code)
        .iter()
        .find(|a| a.local == "Value")
        .map(|a| a.value.clone())
        .unwrap_or_default();
    let last_segment =
        |value: &str| -> String { value.rsplit(':').next().unwrap_or(value).to_string() };
    if last_segment(&value) != "Success" {
        let status_message = doc
            .child_elements(status)
            .into_iter()
            .find(|&child| doc.element_local(child) == "StatusMessage")
            .map(|child| doc.text_content(child));
        let nested = doc
            .child_elements(status_code)
            .into_iter()
            .find(|&child| doc.element_local(child) == "StatusCode")
            .and_then(|child| {
                doc.attrs_of(child)
                    .iter()
                    .find(|a| a.local == "Value")
                    .map(|a| a.value.clone())
            })
            .map(|value| last_segment(&value));
        let message = status_message
            .filter(|message| !message.trim().is_empty())
            .or(nested)
            .unwrap_or_else(|| "unspecified".into());
        return format!(
            "SAML provider returned {} error: {}",
            last_segment(&value),
            message
        );
    }
    "Missing SAML assertion".into()
}

fn parse_saml_timestamp(value: &str) -> Result<i64, String> {
    let trimmed = value.trim();
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|parsed| parsed.timestamp_millis())
        .map_err(|_| format!("Invalid SAML timestamp: {trimmed}"))
}

fn check_timestamps(
    now_ms: i64,
    not_before: Option<&str>,
    not_on_or_after: Option<&str>,
) -> Result<(), String> {
    if let Some(not_before) = not_before.filter(|value| !value.trim().is_empty()) {
        let not_before_ms = parse_saml_timestamp(not_before)?;
        if now_ms + ACCEPTED_CLOCK_SKEW_MS < not_before_ms {
            return Err("SAML assertion not yet valid".into());
        }
    }
    if let Some(not_on_or_after) = not_on_or_after.filter(|value| !value.trim().is_empty()) {
        let not_on_or_after_ms = parse_saml_timestamp(not_on_or_after)?;
        if now_ms - ACCEPTED_CLOCK_SKEW_MS >= not_on_or_after_ms {
            return Err("SAML assertion expired: clocks skewed too much".into());
        }
    }
    Ok(())
}

fn process_signed_assertion(
    doc: &sso_saml_xml::XmlDocument,
    assertion: usize,
    config: &SamlRuntimeConfig,
) -> Result<SamlProfile, String> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let push_field = |fields: &mut Vec<(String, String)>, name: String, value: String| {
        if !fields.iter().any(|(existing, _)| existing == &name) {
            fields.push((name, value));
        }
    };

    if let Some(issuer) = doc
        .child_elements(assertion)
        .into_iter()
        .find(|&child| doc.element_local(child) == "Issuer")
    {
        let text = doc.text_content(issuer);
        if !text.is_empty() {
            push_field(&mut fields, "issuer".into(), text);
        }
    }
    if let Some(authn_statement) = doc
        .child_elements(assertion)
        .into_iter()
        .find(|&child| doc.element_local(child) == "AuthnStatement")
    {
        if let Some(session_index) = doc
            .attrs_of(authn_statement)
            .iter()
            .find(|a| a.local == "SessionIndex")
            .map(|a| a.value.clone())
        {
            push_field(&mut fields, "sessionIndex".into(), session_index);
        }
    }
    if let Some(subject) = doc
        .child_elements(assertion)
        .into_iter()
        .find(|&child| doc.element_local(child) == "Subject")
    {
        if let Some(name_id) = doc
            .child_elements(subject)
            .into_iter()
            .find(|&child| doc.element_local(child) == "NameID")
        {
            let value = doc.text_content(name_id);
            if !value.is_empty() {
                push_field(&mut fields, "nameID".into(), value);
                if let Some(format) = doc
                    .attrs_of(name_id)
                    .iter()
                    .find(|a| a.local == "Format")
                    .map(|a| a.value.clone())
                {
                    push_field(&mut fields, "nameIDFormat".into(), format);
                }
            }
        }
    }

    // Conditions: timestamps and audience.
    let conditions = doc
        .child_elements(assertion)
        .into_iter()
        .filter(|&child| doc.element_local(child) == "Conditions")
        .collect::<Vec<_>>();
    if conditions.len() > 1 {
        return Err("Unable to process multiple conditions in SAML assertion".into());
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    if conditions.len() == 1 {
        let condition = conditions[0];
        let attr = |name: &str| {
            doc.attrs_of(condition)
                .iter()
                .find(|a| a.local == name)
                .map(|a| a.value.clone())
        };
        check_timestamps(
            now_ms,
            attr("NotBefore").as_deref(),
            attr("NotOnOrAfter").as_deref(),
        )?;
    }

    // Upstream validates the audience against `options.audience`, which
    // defaults to the SP issuer, on every assertion; a missing Conditions
    // element fails the same way an empty one does.
    let audience_restrictions = conditions
        .first()
        .map(|&condition| {
            doc.child_elements(condition)
                .into_iter()
                .filter(|&child| doc.element_local(child) == "AudienceRestriction")
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if audience_restrictions.is_empty() {
        return Err("SAML assertion has no AudienceRestriction".into());
    }
    for restriction in &audience_restrictions {
        let audiences: Vec<String> = doc
            .child_elements(*restriction)
            .into_iter()
            .filter(|&child| doc.element_local(child) == "Audience")
            .map(|child| doc.text_content(child))
            .collect();
        if audiences.is_empty() {
            return Err("SAML assertion AudienceRestriction has no Audience value".into());
        }
        if !audiences.iter().any(|audience| audience == &config.issuer) {
            return Err(format!(
                "SAML assertion audience mismatch. Expected: {} Received: {}",
                config.issuer,
                audiences.join(", ")
            ));
        }
    }

    // Attribute statements: attribute names merge into the profile unless a
    // reserved field (nameID, issuer, ...) already claimed the name.
    for statement in doc
        .child_elements(assertion)
        .into_iter()
        .filter(|&child| doc.element_local(child) == "AttributeStatement")
    {
        for attribute in doc
            .child_elements(statement)
            .into_iter()
            .filter(|&child| doc.element_local(child) == "Attribute")
        {
            let Some(name) = doc
                .attrs_of(attribute)
                .iter()
                .find(|a| a.local == "Name")
                .map(|a| a.value.clone())
            else {
                continue;
            };
            let values: Vec<String> = doc
                .child_elements(attribute)
                .into_iter()
                .filter(|&child| doc.element_local(child) == "AttributeValue")
                .filter(|&value| doc.child_elements(value).is_empty())
                .map(|value| doc.text_content(value))
                .collect();
            // Multi-value attributes reduce to the first value, matching what
            // the upstream claim pickers read out of the array.
            if let Some(value) = values.into_iter().next() {
                push_field(&mut fields, name, value);
            }
        }
    }

    let oid_mail = fields
        .iter()
        .find(|(name, _)| name == "urn:oid:0.9.2342.19200300.100.1.3")
        .map(|(_, value)| value.clone());
    if let Some(mail) = oid_mail {
        push_field(&mut fields, "mail".into(), mail);
    }
    let mail = fields
        .iter()
        .find(|(name, _)| name == "mail")
        .map(|(_, value)| value.clone());
    if let Some(mail) = mail {
        push_field(&mut fields, "email".into(), mail);
    }

    Ok(SamlProfile { fields })
}

fn first_claim<'a>(profile: &'a SamlProfile, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        profile
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    })
}

/// Port of upstream `pickSamlEmail`.
pub fn pick_saml_email(profile: &SamlProfile, config: &SamlRuntimeConfig) -> Option<String> {
    if let Some(custom) = profile
        .get(&config.attribute_email)
        .filter(|v| !v.is_empty())
    {
        return Some(custom.clone());
    }
    first_claim(
        profile,
        &[
            "email",
            "emailAddress",
            "mail",
            "nameID",
            "nameId",
            "upn",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/nameidentifier",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/upn",
        ],
    )
    .map(str::to_string)
}

/// Port of upstream `pickSamlDisplayName`.
pub fn pick_saml_display_name(profile: &SamlProfile, config: &SamlRuntimeConfig) -> Option<String> {
    if let Some(custom) = profile
        .get(&config.attribute_name)
        .filter(|v| !v.is_empty())
    {
        return Some(custom.clone());
    }
    if let Some(value) = first_claim(
        profile,
        &[
            "displayName",
            "name",
            "cn",
            "commonName",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name",
            "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/givenname",
        ],
    ) {
        return Some(value.to_string());
    }
    let given = profile.get("givenName").cloned();
    let surname = profile
        .get("sn")
        .or_else(|| profile.get("surname"))
        .cloned();
    if given.is_some() || surname.is_some() {
        let combined = format!(
            "{} {}",
            given.unwrap_or_default(),
            surname.unwrap_or_default()
        )
        .trim()
        .to_string();
        if !combined.is_empty() {
            return Some(combined);
        }
    }
    pick_saml_email(profile, config)
}

// ---------------------------------------------------------------------------
// GET /api/auth/saml/metadata
// ---------------------------------------------------------------------------

/// Port of node-saml `generateServiceProviderMetadata` for the app's
/// configuration: no signing/decryption keys, wantAssertionsSigned, default
/// NameID format, single POST assertion consumer service.
pub fn generate_saml_metadata(config: &SamlRuntimeConfig, origin: &str) -> String {
    let callback_url = format!("{origin}/api/auth/saml/acs");
    let request_id = generate_request_id();
    format!(
        "<EntityDescriptor xmlns=\"urn:oasis:names:tc:SAML:2.0:metadata\" xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" entityID=\"{entity}\" ID=\"{request_id}\"><SPSSODescriptor protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\" AuthnRequestsSigned=\"false\" WantAssertionsSigned=\"true\"><NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</NameIDFormat><AssertionConsumerService index=\"1\" isDefault=\"true\" Binding=\"{HTTP_POST_BINDING}\" Location=\"{callback}\"/></SPSSODescriptor></EntityDescriptor>",
        entity = xml_escape(&config.issuer),
        callback = xml_escape(&callback_url),
    )
}

pub async fn handle_metadata(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response<Body>, AppError> {
    let settings = state.db.settings()?;
    let origin = request_origin(headers, uri);
    let config = saml_runtime_config(&settings);
    let xml = generate_saml_metadata(&config, &origin);
    let mut response = Response::new(Body::from(xml));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

// ---------------------------------------------------------------------------
// POST /api/auth/saml/test
// ---------------------------------------------------------------------------

pub async fn handle_test(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    body: &Value,
) -> Result<Response<Body>, AppError> {
    // canAccessTestRoute: public when dashboard login is disabled, otherwise a
    // valid dashboard session cookie is required.
    let settings = state.db.settings()?;
    let allowed = settings.get("requireLogin").and_then(Value::as_bool) == Some(false)
        || auth::cookie(headers, "auth_token")
            .map(|token| auth::verify_session_token(state, &token))
            .unwrap_or(false);
    if !allowed {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let body = body.as_object().cloned().unwrap_or_default();
    let body_str = |key: &str| body.get(key).and_then(Value::as_str).unwrap_or("");
    let saml_entry_point = first_non_empty(&[
        body_str("samlEntryPoint"),
        &str_setting(&settings, "samlEntryPoint"),
    ]);
    let saml_issuer = first_non_empty(&[
        body_str("samlIssuer"),
        &str_setting(&settings, "samlIssuer"),
        DEFAULT_SAML_ISSUER,
    ]);
    let saml_cert = match body.get("samlCert") {
        // Upstream uses hasOwnProperty: an explicitly empty body value wins.
        Some(Value::String(value)) => value.trim().to_string(),
        _ => str_setting(&settings, "samlCert").trim().to_string(),
    };

    if saml_entry_point.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Single Sign-On Service URL (samlEntryPoint) is required",
        ));
    }
    if url::Url::parse(&saml_entry_point).is_err() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Single Sign-On Service URL must be a valid URL",
        ));
    }
    if saml_issuer.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "SP Entity ID / Issuer (samlIssuer) is required",
        ));
    }
    if saml_cert.is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "IdP X.509 Certificate (samlCert) is required",
        ));
    }
    if sso_saml_xml::format_x509_certificate(&saml_cert).is_empty() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Invalid IdP X.509 Certificate format",
        ));
    }

    let origin = request_origin(headers, uri);
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "samlEntryPoint": saml_entry_point,
            "samlIssuer": saml_issuer,
            "certValid": true,
            "acsUrl": format!("{origin}/api/auth/saml/acs"),
            "metadataUrl": format!("{origin}/api/auth/saml/metadata"),
            "message": "SAML 2.0 configuration verified successfully."
        }),
    ))
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(serde_json::to_vec(&value).unwrap_or_default()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, json!({ "error": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FIXTURES: &str = include_str!("sso_fixtures.json");

    fn fixtures() -> Value {
        serde_json::from_str(FIXTURES).expect("fixture document")
    }

    fn config_with(issuer: &str, cert: &str) -> SamlRuntimeConfig {
        SamlRuntimeConfig {
            entry_point: "https://idp.example.com/sso".into(),
            issuer: issuer.into(),
            cert: cert.into(),
            attribute_email: "email".into(),
            attribute_name: "name".into(),
        }
    }

    #[test]
    fn formats_raw_base64_into_standard_64_column_pem_block() {
        let raw = "MIIC1234567890123456789012345678901234567890123456789012345678901234567890";
        let formatted = sso_saml_xml::format_x509_certificate(raw);
        assert!(formatted.contains("-----BEGIN CERTIFICATE-----"));
        assert!(formatted.contains("-----END CERTIFICATE-----"));
        assert!(
            formatted.contains("MIIC123456789012345678901234567890123456789012345678901234567890")
        );
        assert!(formatted.contains("\n1234567890\n"));
    }

    #[test]
    fn formats_existing_pem_and_rejects_empty_inputs() {
        let raw = "-----BEGIN CERTIFICATE-----\nMIIC1234\n-----END CERTIFICATE-----";
        let formatted = sso_saml_xml::format_x509_certificate(raw);
        assert_eq!(
            formatted.matches("BEGIN CERTIFICATE").count(),
            1,
            "armor headers are cleaned, not duplicated"
        );
        assert!(sso_saml_xml::format_x509_certificate("").is_empty());
        assert!(sso_saml_xml::format_x509_certificate("   ").is_empty());
    }

    #[test]
    fn configured_requires_entry_point_and_cert() {
        assert!(is_saml_configured(&json!({
            "samlEntryPoint": "https://idp.example.com/sso",
            "samlCert": "dummy-cert"
        })));
        assert!(!is_saml_configured(&json!({
            "samlEntryPoint": "https://idp.example.com/sso"
        })));
        assert!(!is_saml_configured(&json!({"samlCert": "dummy-cert"})));
        assert!(!is_saml_configured(&json!({})));
    }

    #[test]
    fn metadata_contains_entity_id_acs_binding_and_want_assertions() {
        let settings = json!({
            "samlEntryPoint": "https://idp.example.com/sso",
            "samlIssuer": "urn:9router:sp",
            "samlCert": "MIIC1234"
        });
        let config = saml_runtime_config(&settings);
        let xml = generate_saml_metadata(&config, "https://localhost:20127");
        assert!(xml.contains("entityID=\"urn:9router:sp\""), "{xml}");
        assert!(
            xml.contains("Location=\"https://localhost:20127/api/auth/saml/acs\""),
            "{xml}"
        );
        assert!(xml.contains("WantAssertionsSigned=\"true\""), "{xml}");
    }

    #[test]
    fn in_response_to_mismatch_replays_are_rejected() {
        let fixtures = fixtures();
        for name in ["inresponseto-missing", "inresponseto-wrong"] {
            let fixture = fixtures["saml"]
                .as_array()
                .expect("saml fixtures")
                .iter()
                .find(|f| f["name"] == name)
                .expect("fixture")
                .clone();
            let config = config_with("urn:9router:sp", fixture["cert"].as_str().unwrap());
            let error = validate_saml_response(
                fixture["samlResponse"].as_str().unwrap(),
                fixture["expectedRequestId"].as_str(),
                &config,
            )
            .expect_err("InResponseTo mismatch must reject");
            assert!(error.contains("InResponseTo mismatch"), "{error}");
        }
    }

    #[test]
    fn missing_certificate_is_rejected() {
        let fixtures = fixtures();
        let fixture = &fixtures["saml"][0];
        let config = config_with("urn:9router:sp", "");
        let error = validate_saml_response(
            fixture["samlResponse"].as_str().unwrap(),
            Some("req-123"),
            &config,
        )
        .expect_err("missing cert");
        assert!(error.contains("Certificate"), "{error}");
    }

    #[test]
    fn signed_fixtures_match_the_upstream_verifier_contract() {
        let fixtures = fixtures();
        for fixture in fixtures["saml"].as_array().expect("saml fixtures") {
            let name = fixture["name"].as_str().unwrap();
            let expected = fixture["expect"].as_str().unwrap();
            if expected == "inresponseto-mismatch" {
                continue;
            }
            let issuer = if name == "ok-issuer-only-audience" {
                "urn:custom:sp"
            } else {
                "urn:9router:sp"
            };
            let config = config_with(issuer, fixture["cert"].as_str().unwrap());
            let result =
                validate_saml_response(fixture["samlResponse"].as_str().unwrap(), None, &config);
            if expected == "accept" {
                let profile = result.unwrap_or_else(|error| panic!("{name}: {error}"));
                if name.starts_with("ok-") && name != "ok-issuer-only-audience" {
                    assert_eq!(
                        profile.get("email").map(String::as_str),
                        Some("jane@example.com"),
                        "{name}"
                    );
                    assert_eq!(
                        profile.get("displayName").map(String::as_str),
                        Some("Jane Doe"),
                        "{name}"
                    );
                    assert_eq!(
                        profile.get("nameID").map(String::as_str),
                        Some("user@example.com"),
                        "{name}"
                    );
                }
            } else {
                assert!(result.is_err(), "{name}: expected rejection");
            }
        }
    }

    #[test]
    fn custom_saml_issuer_is_used_as_expected_audience() {
        let fixtures = fixtures();
        let fixture = fixtures["saml"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "ok-issuer-only-audience")
            .unwrap();
        let config = config_with("urn:custom:sp", fixture["cert"].as_str().unwrap());
        assert!(
            validate_saml_response(fixture["samlResponse"].as_str().unwrap(), None, &config)
                .is_ok()
        );
    }

    #[test]
    fn claim_pickers_match_upstream_priority() {
        let profile = SamlProfile {
            fields: vec![
                ("email".into(), "user@example.com".into()),
                ("displayName".into(), "Jane Doe".into()),
                (
                    "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress".into(),
                    "custom@example.com".into(),
                ),
                ("customEmail".into(), "custom-email@example.com".into()),
                ("customName".into(), "Custom User".into()),
            ],
        };
        let default_config = config_with("urn:9router:sp", "cert");
        assert_eq!(
            pick_saml_email(&profile, &default_config).as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            pick_saml_display_name(&profile, &default_config).as_deref(),
            Some("Jane Doe")
        );

        let custom_config = SamlRuntimeConfig {
            attribute_email: "customEmail".into(),
            attribute_name: "customName".into(),
            ..config_with("urn:9router:sp", "cert")
        };
        assert_eq!(
            pick_saml_email(&profile, &custom_config).as_deref(),
            Some("custom-email@example.com")
        );
        assert_eq!(
            pick_saml_display_name(&profile, &custom_config).as_deref(),
            Some("Custom User")
        );

        let ws_claim = SamlProfile {
            fields: vec![(
                "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress".into(),
                "custom@example.com".into(),
            )],
        };
        assert_eq!(
            pick_saml_email(&ws_claim, &default_config).as_deref(),
            Some("custom@example.com")
        );

        let combined = SamlProfile {
            fields: vec![
                ("givenName".into(), "Alice".into()),
                ("surname".into(), "Smith".into()),
            ],
        };
        assert_eq!(
            pick_saml_display_name(&combined, &default_config).as_deref(),
            Some("Alice Smith")
        );
        let email_only = SamlProfile {
            fields: vec![("email".into(), "user@example.com".into())],
        };
        assert_eq!(
            pick_saml_display_name(&email_only, &default_config).as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn authorize_url_carries_the_deflated_authn_request() {
        let config = config_with("urn:9router:sp", "cert");
        let (url, request_id) =
            build_authorize_url(&config, "http://localhost:20128").expect("authorize url");
        assert!(request_id.starts_with('_'));
        assert_eq!(request_id.len(), 41);
        assert!(url.starts_with("https://idp.example.com/sso?SAMLRequest="));
        let encoded = &url[url.find("SAMLRequest=").unwrap() + "SAMLRequest=".len()..];
        let decoded = STANDARD
            .decode(urldecode(encoded))
            .expect("base64 SAMLRequest");
        let mut decoder = flate2::read::DeflateDecoder::new(decoded.as_slice());
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut xml).expect("inflate");
        assert!(xml.contains(&format!("ID=\"{request_id}\"")), "{xml}");
        assert!(
            xml.contains(
                "AssertionConsumerServiceURL=\"http://localhost:20128/api/auth/saml/acs\""
            ),
            "{xml}"
        );
        assert!(xml.contains(">urn:9router:sp</saml:Issuer>"), "{xml}");
        assert!(
            xml.contains("Format=\"urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress\""),
            "{xml}"
        );
    }

    fn urldecode(value: &str) -> Vec<u8> {
        url::form_urlencoded::parse(format!("k={value}").as_bytes())
            .next()
            .map(|(_, v)| v.to_string().into_bytes())
            .unwrap_or_default()
    }
}
