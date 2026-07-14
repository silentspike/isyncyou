// Task 2 freezes the identity contract; Task 4 wires it into candidate activation.
#![allow(dead_code)]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use isyncyou_agent::ProductProviderId;
use ring::{digest, signature};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(crate) const CODEX_OIDC_ISSUER: &str = "https://auth.openai.com";
pub(crate) const CODEX_OIDC_ALGORITHM: &str = "RS256";
pub(crate) const OIDC_CLOCK_SKEW_SECONDS: i64 = 60;
pub(crate) const OAUTH_ATTEMPT_MAX_AGE_SECONDS: i64 = 8 * 60;
const SUBJECT_MAX_BYTES: usize = 256;
const DISCOVERY_MAX_BYTES: usize = 32 * 1024;
const JWKS_MAX_BYTES: usize = 256 * 1024;
const JWKS_MAX_KEYS: usize = 16;
const JWKS_CACHE_TTL_MIN_SECONDS: u64 = 5 * 60;
const JWKS_CACHE_TTL_MAX_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchCapability {
    VerifiedSubject,
    Unavailable,
}

pub(crate) fn switch_capability(
    provider: ProductProviderId,
    subject_digest: Option<&str>,
) -> SwitchCapability {
    match (provider, subject_digest) {
        (ProductProviderId::Codex, Some(value)) if valid_digest(value) => {
            SwitchCapability::VerifiedSubject
        }
        _ => SwitchCapability::Unavailable,
    }
}

pub(crate) fn subject_digest(
    provider: ProductProviderId,
    issuer: &str,
    subject: &str,
) -> Result<String, IdentityError> {
    if issuer.is_empty() || subject.is_empty() || subject.len() > SUBJECT_MAX_BYTES {
        return Err(IdentityError::InvalidSubject);
    }
    let mut input = Vec::new();
    for field in [
        b"isyncyou/product-subject-digest/v1".as_slice(),
        provider.wire().as_bytes(),
        issuer.as_bytes(),
        subject.as_bytes(),
    ] {
        let len = u32::try_from(field.len()).map_err(|_| IdentityError::InvalidSubject)?;
        input.extend_from_slice(&len.to_be_bytes());
        input.extend_from_slice(field);
    }
    Ok(URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, &input).as_ref()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProductCredentialIdentityV3 {
    pub subject_digest: Option<String>,
    pub session_id_digest: Option<String>,
}

impl ProductCredentialIdentityV3 {
    pub(crate) fn validate(&self) -> Result<(), IdentityError> {
        for value in [&self.subject_digest, &self.session_id_digest]
            .into_iter()
            .flatten()
        {
            if !valid_digest(value) {
                return Err(IdentityError::InvalidDigest);
            }
        }
        Ok(())
    }

    pub(crate) fn merge_into(&self, object: &mut Map<String, Value>) {
        object.insert(
            "subject_digest".into(),
            self.subject_digest
                .clone()
                .map_or(Value::Null, Value::String),
        );
        object.insert(
            "session_id_digest".into(),
            self.session_id_digest
                .clone()
                .map_or(Value::Null, Value::String),
        );
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityError {
    InvalidToken,
    InvalidHeader,
    InvalidSignature,
    InvalidIssuer,
    InvalidAudience,
    InvalidAuthorizedParty,
    InvalidTime,
    InvalidAttempt,
    InvalidNonce,
    InvalidAccessTokenHash,
    InvalidSubject,
    InvalidDigest,
    InvalidDiscovery,
    InvalidJwks,
    UnknownKey,
    SizeLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OidcAttemptBinding<'a> {
    pub expected_state: &'a str,
    pub callback_state: &'a str,
    pub expected_nonce: Option<&'a str>,
    pub started_at_seconds: i64,
    pub now_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedCodexSubject {
    pub subject_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct JwksResponse<'a> {
    pub body: &'a [u8],
    pub content_type: &'a str,
    pub redirected: bool,
    pub source_origin: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct JwtHeader {
    alg: String,
    kid: String,
    #[serde(default)]
    typ: Option<String>,
    #[serde(default)]
    jku: Option<Value>,
    #[serde(default)]
    x5u: Option<Value>,
    #[serde(default)]
    jwk: Option<Value>,
    #[serde(default)]
    crit: Option<Vec<String>>,
    #[serde(default)]
    b64: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    #[serde(default)]
    azp: Option<String>,
    sub: String,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    at_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct JwksDocument {
    keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    kid: String,
    n: String,
    e: String,
    #[serde(default)]
    r#use: Option<String>,
    #[serde(default)]
    key_ops: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedJwk {
    n: Vec<u8>,
    e: Vec<u8>,
}

pub(crate) fn validate_codex_subject<F>(
    token: &str,
    access_token: &str,
    client_id: &str,
    attempt: &OidcAttemptBinding<'_>,
    initial_jwks: JwksResponse<'_>,
    refresh_jwks: F,
) -> Result<ValidatedCodexSubject, IdentityError>
where
    F: FnMut() -> Result<JwksResponse<'static>, IdentityError>,
{
    validate_codex_subject_with_verifier(
        token,
        access_token,
        client_id,
        attempt,
        initial_jwks,
        refresh_jwks,
        |key, signing_input, signature_bytes| {
            signature::RsaPublicKeyComponents {
                n: &key.n,
                e: &key.e,
            }
            .verify(
                &signature::RSA_PKCS1_2048_8192_SHA256,
                signing_input,
                signature_bytes,
            )
            .map_err(|_| IdentityError::InvalidSignature)
        },
    )
}

fn validate_codex_subject_with_verifier<F, V>(
    token: &str,
    access_token: &str,
    client_id: &str,
    attempt: &OidcAttemptBinding<'_>,
    initial_jwks: JwksResponse<'_>,
    mut refresh_jwks: F,
    verify_signature: V,
) -> Result<ValidatedCodexSubject, IdentityError>
where
    F: FnMut() -> Result<JwksResponse<'static>, IdentityError>,
    V: Fn(&ParsedJwk, &[u8], &[u8]) -> Result<(), IdentityError>,
{
    if attempt.expected_state.is_empty()
        || !constant_time_eq(
            attempt.expected_state.as_bytes(),
            attempt.callback_state.as_bytes(),
        )
        || attempt.now_seconds < attempt.started_at_seconds
        || attempt.now_seconds - attempt.started_at_seconds > OAUTH_ATTEMPT_MAX_AGE_SECONDS
    {
        return Err(IdentityError::InvalidAttempt);
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(IdentityError::InvalidToken);
    }
    let header_bytes = decode_segment(parts[0])?;
    let claims_bytes = decode_segment(parts[1])?;
    let signature_bytes = decode_segment(parts[2])?;
    let header: JwtHeader = parse_strict_json(&header_bytes)?;
    if header.alg != CODEX_OIDC_ALGORITHM
        || header.kid.is_empty()
        || header.kid.len() > 128
        || header.jku.is_some()
        || header.x5u.is_some()
        || header.jwk.is_some()
        || header.crit.as_ref().is_some_and(|value| !value.is_empty())
        || header.b64 == Some(false)
        || header.typ.as_deref().is_some_and(|value| value != "JWT")
    {
        return Err(IdentityError::InvalidHeader);
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let mut keys = parse_jwks(initial_jwks)?;
    let key = match keys.remove(&header.kid) {
        Some(key) => key,
        None => {
            keys = parse_jwks(refresh_jwks()?)?;
            keys.remove(&header.kid).ok_or(IdentityError::UnknownKey)?
        }
    };
    verify_signature(&key, signing_input.as_bytes(), &signature_bytes)?;
    let claims: Claims = parse_strict_json(&claims_bytes)?;
    validate_claims(&claims, access_token, client_id, attempt)?;
    Ok(ValidatedCodexSubject {
        subject_digest: subject_digest(ProductProviderId::Codex, &claims.iss, &claims.sub)?,
    })
}

fn validate_claims(
    claims: &Claims,
    access_token: &str,
    client_id: &str,
    attempt: &OidcAttemptBinding<'_>,
) -> Result<(), IdentityError> {
    if claims.iss != CODEX_OIDC_ISSUER {
        return Err(IdentityError::InvalidIssuer);
    }
    let audiences: Vec<&str> = match &claims.aud {
        Audience::One(value) => vec![value],
        Audience::Many(values) => values.iter().map(String::as_str).collect(),
    };
    if audiences.len() != 1 || audiences[0] != client_id {
        return Err(IdentityError::InvalidAudience);
    }
    if claims
        .azp
        .as_deref()
        .is_some_and(|value| value != client_id)
    {
        return Err(IdentityError::InvalidAuthorizedParty);
    }
    if claims.exp <= attempt.now_seconds - OIDC_CLOCK_SKEW_SECONDS
        || claims.iat > attempt.now_seconds + OIDC_CLOCK_SKEW_SECONDS
        || claims.iat < attempt.started_at_seconds - OIDC_CLOCK_SKEW_SECONDS
        || claims
            .nbf
            .is_some_and(|value| value > attempt.now_seconds + OIDC_CLOCK_SKEW_SECONDS)
    {
        return Err(IdentityError::InvalidTime);
    }
    if claims.sub.is_empty() || claims.sub.len() > SUBJECT_MAX_BYTES {
        return Err(IdentityError::InvalidSubject);
    }
    match (attempt.expected_nonce, claims.nonce.as_deref()) {
        (Some(expected), Some(actual))
            if constant_time_eq(expected.as_bytes(), actual.as_bytes()) => {}
        (Some(_), _) => return Err(IdentityError::InvalidNonce),
        (None, _) => {}
    }
    if let Some(expected) = &claims.at_hash {
        let hash = digest::digest(&digest::SHA256, access_token.as_bytes());
        let actual = URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16]);
        if !constant_time_eq(expected.as_bytes(), actual.as_bytes()) {
            return Err(IdentityError::InvalidAccessTokenHash);
        }
    }
    Ok(())
}

fn parse_jwks(response: JwksResponse<'_>) -> Result<BTreeMap<String, ParsedJwk>, IdentityError> {
    if response.redirected
        || response.source_origin != CODEX_OIDC_ISSUER
        || !is_json_content_type(response.content_type)
        || response.body.len() > JWKS_MAX_BYTES
    {
        return Err(IdentityError::InvalidJwks);
    }
    let document: JwksDocument = parse_strict_json(response.body)?;
    if document.keys.is_empty() || document.keys.len() > JWKS_MAX_KEYS {
        return Err(IdentityError::InvalidJwks);
    }
    let mut keys = BTreeMap::new();
    for key in document.keys {
        if key.kty != "RSA"
            || key.kid.is_empty()
            || key.kid.len() > 128
            || key.r#use.as_deref() != Some("sig")
            || key
                .key_ops
                .as_ref()
                .is_some_and(|ops| !ops.iter().any(|op| op == "verify"))
        {
            return Err(IdentityError::InvalidJwks);
        }
        let parsed = ParsedJwk {
            n: decode_bounded_b64(&key.n, 1024)?,
            e: decode_bounded_b64(&key.e, 8)?,
        };
        if keys.insert(key.kid, parsed).is_some() {
            return Err(IdentityError::InvalidJwks);
        }
    }
    Ok(keys)
}

pub(crate) fn validate_codex_discovery(
    body: &[u8],
    content_type: &str,
    redirected: bool,
) -> Result<String, IdentityError> {
    if redirected || body.len() > DISCOVERY_MAX_BYTES || !is_json_content_type(content_type) {
        return Err(IdentityError::InvalidDiscovery);
    }
    let value: Value = parse_strict_json(body)?;
    let object = value.as_object().ok_or(IdentityError::InvalidDiscovery)?;
    if object.get("issuer").and_then(Value::as_str) != Some(CODEX_OIDC_ISSUER) {
        return Err(IdentityError::InvalidDiscovery);
    }
    let jwks_uri = object
        .get("jwks_uri")
        .and_then(Value::as_str)
        .ok_or(IdentityError::InvalidDiscovery)?;
    if !jwks_uri.starts_with("https://auth.openai.com/") || jwks_uri.contains(['?', '#', '@']) {
        return Err(IdentityError::InvalidDiscovery);
    }
    let algorithms = object
        .get("id_token_signing_alg_values_supported")
        .and_then(Value::as_array)
        .ok_or(IdentityError::InvalidDiscovery)?;
    if algorithms.len() != 1 || algorithms[0].as_str() != Some(CODEX_OIDC_ALGORITHM) {
        return Err(IdentityError::InvalidDiscovery);
    }
    Ok(jwks_uri.to_string())
}

pub(crate) fn bounded_jwks_cache_ttl(seconds: Option<u64>) -> u64 {
    seconds
        .unwrap_or(JWKS_CACHE_TTL_MIN_SECONDS)
        .clamp(JWKS_CACHE_TTL_MIN_SECONDS, JWKS_CACHE_TTL_MAX_SECONDS)
}

fn is_json_content_type(value: &str) -> bool {
    matches!(
        value.split(';').next().map(str::trim),
        Some("application/json")
    )
}

fn decode_segment(value: &str) -> Result<Vec<u8>, IdentityError> {
    if value.len() > 512 * 1024 {
        return Err(IdentityError::SizeLimit);
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| IdentityError::InvalidToken)
}

fn decode_bounded_b64(value: &str, max: usize) -> Result<Vec<u8>, IdentityError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| IdentityError::InvalidJwks)?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(IdentityError::InvalidJwks);
    }
    Ok(bytes)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[0u8; 32]);
    let tag = ring::hmac::sign(&key, left);
    ring::hmac::verify(&key, right, tag.as_ref()).is_ok()
}

fn parse_strict_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, IdentityError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let strict = StrictValueSeed
        .deserialize(&mut deserializer)
        .map_err(|_| IdentityError::InvalidToken)?;
    deserializer
        .end()
        .map_err(|_| IdentityError::InvalidToken)?;
    serde_json::from_value(strict).map_err(|_| IdentityError::InvalidToken)
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object members")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_string()))
    }
    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        StrictValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        let mut names = BTreeSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !names.insert(name.clone()) {
                return Err(A::Error::custom("duplicate JSON member"));
            }
            values.insert(name, map.next_value_seed(StrictValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_ID: &str = "app-client";

    fn attempt() -> OidcAttemptBinding<'static> {
        OidcAttemptBinding {
            expected_state: "state",
            callback_state: "state",
            expected_nonce: Some("nonce"),
            started_at_seconds: 900,
            now_seconds: 1_000,
        }
    }

    fn header(extra: &str) -> String {
        format!(r#"{{"alg":"RS256","kid":"kid-1","typ":"JWT"{extra}}}"#)
    }

    fn claims(extra: &str) -> String {
        format!(
            r#"{{"iss":"https://auth.openai.com","aud":"{CLIENT_ID}","sub":"subject-1","exp":2000,"iat":950,"nonce":"nonce"{extra}}}"#
        )
    }

    fn token(header: &str, claims: &str) -> String {
        format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(claims),
            URL_SAFE_NO_PAD.encode(b"signature")
        )
    }

    fn jwks(kid: &str, extra_key_fields: &str) -> Vec<u8> {
        format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"{kid}","n":"AQAB","e":"AQAB","use":"sig"{extra_key_fields}}}]}}"#
        )
        .into_bytes()
    }

    fn response(body: &[u8]) -> JwksResponse<'_> {
        JwksResponse {
            body,
            content_type: "application/json",
            redirected: false,
            source_origin: CODEX_OIDC_ISSUER,
        }
    }

    fn validate_with(
        header_json: &str,
        claims_json: &str,
        jwks_body: &[u8],
        binding: &OidcAttemptBinding<'_>,
        signature_valid: bool,
    ) -> Result<ValidatedCodexSubject, IdentityError> {
        validate_codex_subject_with_verifier(
            &token(header_json, claims_json),
            "access-token",
            CLIENT_ID,
            binding,
            response(jwks_body),
            || Err(IdentityError::UnknownKey),
            move |_, _, _| {
                if signature_valid {
                    Ok(())
                } else {
                    Err(IdentityError::InvalidSignature)
                }
            },
        )
    }

    #[test]
    fn credential_v3_subject_digest_is_domain_provider_and_issuer_separated() {
        let a = subject_digest(ProductProviderId::Codex, CODEX_OIDC_ISSUER, "subject").unwrap();
        let b = subject_digest(ProductProviderId::Claude, CODEX_OIDC_ISSUER, "subject").unwrap();
        let c =
            subject_digest(ProductProviderId::Codex, "https://other.invalid", "subject").unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn claude_switch_identity_is_unavailable_without_signed_subject_contract() {
        assert_eq!(
            switch_capability(ProductProviderId::Claude, Some(&"a".repeat(43))),
            SwitchCapability::Unavailable
        );
        assert_eq!(
            switch_capability(ProductProviderId::Codex, None),
            SwitchCapability::Unavailable
        );
    }

    #[test]
    fn credential_v3_never_serializes_email_or_display_name() {
        let identity = ProductCredentialIdentityV3 {
            subject_digest: Some("a".repeat(43)),
            session_id_digest: None,
        };
        let bytes = serde_json::to_vec(&identity).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("email"));
        assert!(!text.contains("display"));
        assert!(!text.contains('@'));
    }

    #[test]
    fn codex_discovery_never_replaces_reviewed_token_or_revoke_endpoint() {
        let body = br#"{"issuer":"https://auth.openai.com","jwks_uri":"https://auth.openai.com/.well-known/jwks.json","token_endpoint":"https://auth.openai.com/api/accounts/oauth/token","revocation_endpoint":"https://auth.openai.com/api/accounts/oauth/revoke","id_token_signing_alg_values_supported":["RS256"]}"#;
        assert_eq!(
            validate_codex_discovery(body, "application/json", false).unwrap(),
            "https://auth.openai.com/.well-known/jwks.json"
        );
        assert_eq!(
            isyncyou_agent::oauth::CodexOAuthConfig::default().token_url,
            "https://auth.openai.com/oauth/token"
        );
        assert_eq!(bounded_jwks_cache_ttl(None), 5 * 60);
        assert_eq!(bounded_jwks_cache_ttl(Some(1)), 5 * 60);
        assert_eq!(bounded_jwks_cache_ttl(Some(60 * 60)), 60 * 60);
        assert_eq!(bounded_jwks_cache_ttl(Some(u64::MAX)), 24 * 60 * 60);
    }

    #[test]
    fn strict_json_rejects_duplicate_members_and_trailing_data() {
        assert_eq!(
            parse_strict_json::<Value>(br#"{"sub":"a","sub":"b"}"#),
            Err(IdentityError::InvalidToken)
        );
        assert_eq!(
            parse_strict_json::<Value>(br#"{}{}"#),
            Err(IdentityError::InvalidToken)
        );
    }

    #[test]
    fn codex_subject_requires_valid_signature_issuer_audience_expiry_and_attempt() {
        let keys = jwks("kid-1", "");
        assert!(validate_with(&header(""), &claims(""), &keys, &attempt(), true).is_ok());
        assert_eq!(
            validate_with(&header(""), &claims(""), &keys, &attempt(), false),
            Err(IdentityError::InvalidSignature)
        );
        let wrong_issuer = claims("").replace(CODEX_OIDC_ISSUER, "https://invalid.example");
        assert_eq!(
            validate_with(&header(""), &wrong_issuer, &keys, &attempt(), true),
            Err(IdentityError::InvalidIssuer)
        );
        let wrong_audience = claims("").replace(CLIENT_ID, "other-client");
        assert_eq!(
            validate_with(&header(""), &wrong_audience, &keys, &attempt(), true),
            Err(IdentityError::InvalidAudience)
        );
        let expired = claims("").replace("\"exp\":2000", "\"exp\":900");
        assert_eq!(
            validate_with(&header(""), &expired, &keys, &attempt(), true),
            Err(IdentityError::InvalidTime)
        );
        let mut wrong_attempt = attempt();
        wrong_attempt.callback_state = "other";
        assert_eq!(
            validate_with(&header(""), &claims(""), &keys, &wrong_attempt, true),
            Err(IdentityError::InvalidAttempt)
        );
    }

    #[test]
    fn codex_subject_rejects_unapproved_discovery_jwks_origin_algorithm_and_kid() {
        let keys = jwks("kid-1", "");
        assert_eq!(
            validate_with(
                &header("").replace("RS256", "HS256"),
                &claims(""),
                &keys,
                &attempt(),
                true
            ),
            Err(IdentityError::InvalidHeader)
        );
        assert_eq!(
            validate_with(
                &header("").replace("kid-1", "missing"),
                &claims(""),
                &keys,
                &attempt(),
                true
            ),
            Err(IdentityError::UnknownKey)
        );
        let mut wrong_origin = response(&keys);
        wrong_origin.source_origin = "https://other.invalid";
        assert_eq!(parse_jwks(wrong_origin), Err(IdentityError::InvalidJwks));
        assert_eq!(
            validate_codex_discovery(
                br#"{"issuer":"https://other.invalid"}"#,
                "application/json",
                false
            ),
            Err(IdentityError::InvalidDiscovery)
        );
    }

    #[test]
    fn codex_subject_rejects_non_rs256_duplicate_json_extra_audience_and_invalid_azp() {
        let keys = jwks("kid-1", "");
        let duplicate = r#"{"alg":"RS256","kid":"kid-1","kid":"kid-2"}"#;
        assert_eq!(
            validate_with(duplicate, &claims(""), &keys, &attempt(), true),
            Err(IdentityError::InvalidToken)
        );
        let extra_audience = claims("").replace(
            &format!("\"aud\":\"{CLIENT_ID}\""),
            &format!("\"aud\":[\"{CLIENT_ID}\",\"other\"]"),
        );
        assert_eq!(
            validate_with(&header(""), &extra_audience, &keys, &attempt(), true),
            Err(IdentityError::InvalidAudience)
        );
        assert_eq!(
            validate_with(
                &header(""),
                &claims(r#","azp":"other""#),
                &keys,
                &attempt(),
                true
            ),
            Err(IdentityError::InvalidAuthorizedParty)
        );
    }

    #[test]
    fn codex_subject_rejects_future_nbf_empty_or_oversized_sub() {
        let keys = jwks("kid-1", "");
        assert_eq!(
            validate_with(
                &header(""),
                &claims(r#","nbf":2000"#),
                &keys,
                &attempt(),
                true
            ),
            Err(IdentityError::InvalidTime)
        );
        let empty = claims("").replace("subject-1", "");
        assert_eq!(
            validate_with(&header(""), &empty, &keys, &attempt(), true),
            Err(IdentityError::InvalidSubject)
        );
        let oversized = claims("").replace("subject-1", &"s".repeat(257));
        assert_eq!(
            validate_with(&header(""), &oversized, &keys, &attempt(), true),
            Err(IdentityError::InvalidSubject)
        );
    }

    #[test]
    fn codex_jwks_rejects_redirect_content_type_size_key_count_duplicate_kid_and_wrong_key_use() {
        let keys = jwks("kid-1", "");
        let mut redirected = response(&keys);
        redirected.redirected = true;
        assert_eq!(parse_jwks(redirected), Err(IdentityError::InvalidJwks));
        let mut wrong_type = response(&keys);
        wrong_type.content_type = "text/plain";
        assert_eq!(parse_jwks(wrong_type), Err(IdentityError::InvalidJwks));
        let huge = vec![b' '; JWKS_MAX_BYTES + 1];
        assert_eq!(parse_jwks(response(&huge)), Err(IdentityError::InvalidJwks));
        let duplicate = br#"{"keys":[{"kty":"RSA","kid":"x","n":"AQAB","e":"AQAB","use":"sig"},{"kty":"RSA","kid":"x","n":"AQAB","e":"AQAB","use":"sig"}]}"#;
        assert_eq!(
            parse_jwks(response(duplicate)),
            Err(IdentityError::InvalidJwks)
        );
        assert_eq!(
            parse_jwks(response(&jwks("kid-1", r#","key_ops":["sign"]"#))),
            Err(IdentityError::InvalidJwks)
        );
        let many = format!(
            "{{\"keys\":[{}]}}",
            (0..17)
                .map(|i| format!(
                    r#"{{"kty":"RSA","kid":"k{i}","n":"AQAB","e":"AQAB","use":"sig"}}"#
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(
            parse_jwks(response(many.as_bytes())),
            Err(IdentityError::InvalidJwks)
        );
    }

    #[test]
    fn codex_unknown_kid_refreshes_jwks_exactly_once_then_fails_closed() {
        let initial = jwks("old", "");
        let refreshed = Box::leak(jwks("kid-1", "").into_boxed_slice());
        let mut calls = 0;
        let result = validate_codex_subject_with_verifier(
            &token(&header(""), &claims("")),
            "access-token",
            CLIENT_ID,
            &attempt(),
            response(&initial),
            || {
                calls += 1;
                Ok(response(refreshed))
            },
            |_, _, _| Ok(()),
        );
        assert!(result.is_ok());
        assert_eq!(calls, 1);
        let missing = Box::leak(jwks("still-missing", "").into_boxed_slice());
        let mut failed_calls = 0;
        let result = validate_codex_subject_with_verifier(
            &token(&header(""), &claims("")),
            "access-token",
            CLIENT_ID,
            &attempt(),
            response(&initial),
            || {
                failed_calls += 1;
                Ok(response(missing))
            },
            |_, _, _| Ok(()),
        );
        assert_eq!(result, Err(IdentityError::UnknownKey));
        assert_eq!(failed_calls, 1);
    }

    #[test]
    fn codex_subject_validates_fixed_skew_nonce_and_optional_at_hash() {
        let keys = jwks("kid-1", "");
        let hash = digest::digest(&digest::SHA256, b"access-token");
        let at_hash = URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16]);
        assert!(validate_with(
            &header(""),
            &claims(&format!(r#","at_hash":"{at_hash}""#)),
            &keys,
            &attempt(),
            true
        )
        .is_ok());
        assert_eq!(
            validate_with(
                &header(""),
                &claims(r#","at_hash":"wrong""#),
                &keys,
                &attempt(),
                true
            ),
            Err(IdentityError::InvalidAccessTokenHash)
        );
        let wrong_nonce = claims("").replace("\"nonce\":\"nonce\"", "\"nonce\":\"other\"");
        assert_eq!(
            validate_with(&header(""), &wrong_nonce, &keys, &attempt(), true),
            Err(IdentityError::InvalidNonce)
        );
        let skew_exp = claims("").replace("\"exp\":2000", "\"exp\":941");
        assert!(validate_with(&header(""), &skew_exp, &keys, &attempt(), true).is_ok());
    }

    #[test]
    fn codex_subject_rejects_jku_x5u_embedded_jwk_crit_and_unencoded_payload() {
        let keys = jwks("kid-1", "");
        for extra in [
            r#","jku":"https://evil.invalid""#,
            r#","x5u":"https://evil.invalid""#,
            r#","jwk":{}"#,
            r#","crit":["exp"]"#,
            r#","b64":false"#,
        ] {
            assert!(matches!(
                validate_with(&header(extra), &claims(""), &keys, &attempt(), true),
                Err(IdentityError::InvalidHeader)
            ));
        }
    }
}
