//! Firebase Cloud Messaging (FCM) HTTP v1 sender (#576). This is **not** Microsoft
//! Graph — it lives in this crate only to reuse its `ring` (RS256 JWT signing) and
//! `reqwest` dependencies. It authenticates with a Google service-account: sign a
//! JWT with the account's RSA private key, exchange it at Google's OAuth token
//! endpoint for a short-lived access token, then POST a notification to the FCM v1
//! endpoint. Behind the `http` feature.
//!
//! SECURITY: the service-account private key + the access token are secrets — never
//! logged or surfaced.

#[cfg(feature = "http")]
use base64::Engine as _;
#[cfg(feature = "http")]
use serde_json::json;
use serde_json::Value;

/// A parsed Google service-account credential (the Firebase Admin SDK JSON).
#[derive(Clone)]
pub struct ServiceAccount {
    pub client_email: String,
    pub private_key_pem: String,
    pub token_uri: String,
    pub project_id: String,
}

impl ServiceAccount {
    /// Parse the service-account JSON. The private key stays in memory only.
    pub fn from_json(s: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(s).map_err(|e| format!("service-account JSON: {e}"))?;
        let get = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("service-account: missing '{k}'"))
        };
        Ok(ServiceAccount {
            client_email: get("client_email")?,
            private_key_pem: get("private_key")?,
            token_uri: get("token_uri")?,
            project_id: get("project_id")?,
        })
    }
}

#[cfg(feature = "http")]
fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// PEM (PKCS#8 `BEGIN PRIVATE KEY`) → DER bytes.
#[cfg(feature = "http")]
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("private key base64: {e}"))
}

/// Sign a Google OAuth assertion JWT (RS256) valid for one hour from `now_unix`.
#[cfg(feature = "http")]
fn sign_assertion(sa: &ServiceAccount, now_unix: u64) -> Result<String, String> {
    use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = b64url(
        format!(
            r#"{{"iss":"{}","scope":"https://www.googleapis.com/auth/firebase.messaging","aud":"{}","iat":{},"exp":{}}}"#,
            sa.client_email,
            sa.token_uri,
            now_unix,
            now_unix + 3600
        )
        .as_bytes(),
    );
    let signing_input = format!("{header}.{claims}");
    let der = pem_to_der(&sa.private_key_pem)?;
    let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("RSA key: {e}"))?;
    let rng = ring::rand::SystemRandom::new();
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .map_err(|e| format!("JWT sign: {e}"))?;
    Ok(format!("{signing_input}.{}", b64url(&sig)))
}

#[cfg(feature = "http")]
fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

/// Exchange the signed assertion for a short-lived OAuth2 access token.
#[cfg(feature = "http")]
fn access_token(sa: &ServiceAccount, now_unix: u64) -> Result<String, String> {
    let jwt = sign_assertion(sa, now_unix)?;
    let resp = client()
        .post(&sa.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("token HTTP {}: {}", status.as_u16(), v));
    }
    v.get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "token response missing access_token".into())
}

/// Send an FCM v1 notification to a device registration token. Returns the FCM
/// message name on success.
#[cfg(feature = "http")]
pub fn fcm_send(
    sa: &ServiceAccount,
    device_token: &str,
    title: &str,
    body: &str,
    now_unix: u64,
) -> Result<String, String> {
    let access = access_token(sa, now_unix)?;
    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        sa.project_id
    );
    let payload = json!({
        "message": {
            "token": device_token,
            "notification": { "title": title, "body": body },
            "android": { "priority": "high" }
        }
    });
    let resp = client()
        .post(&url)
        .bearer_auth(&access)
        .json(&payload)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("FCM HTTP {}: {}", status.as_u16(), v));
    }
    v.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "FCM response missing name".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_service_account_fields() {
        let sa = ServiceAccount::from_json(
            r#"{"type":"service_account","project_id":"isyncyou",
                "client_email":"x@isyncyou.iam.gserviceaccount.com",
                "private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
                "token_uri":"https://oauth2.googleapis.com/token"}"#,
        )
        .unwrap();
        assert_eq!(sa.project_id, "isyncyou");
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
        assert!(sa.client_email.ends_with(".gserviceaccount.com"));
    }

    #[test]
    fn missing_field_errors() {
        assert!(ServiceAccount::from_json(r#"{"project_id":"x"}"#).is_err());
    }

    #[cfg(feature = "http")]
    #[test]
    fn pem_strips_headers_and_decodes() {
        // "AAAA" base64 = 3 zero bytes.
        let der =
            pem_to_der("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n").unwrap();
        assert_eq!(der, vec![0u8, 0, 0]);
    }

    #[cfg(feature = "http")]
    #[test]
    fn b64url_is_unpadded_urlsafe() {
        // bytes 0xfb 0xff → url-safe alphabet uses '-'/'_' (std base64 would be "+/8").
        assert_eq!(b64url(b"\xfb\xff"), "-_8");
    }

    /// Live: sends a real FCM push. Run with the service-account + a device token:
    /// `ISY_FCM_SA=… ISY_FCM_TOKEN=… cargo test -p isyncyou-graph --features http \
    ///   live_fcm_send -- --ignored --nocapture`
    #[cfg(feature = "http")]
    #[test]
    #[ignore = "live: needs ISY_FCM_SA + ISY_FCM_TOKEN env; sends a real push"]
    fn live_fcm_send() {
        let sa_path = std::env::var("ISY_FCM_SA").expect("ISY_FCM_SA");
        let sa = ServiceAccount::from_json(&std::fs::read_to_string(sa_path).unwrap()).unwrap();
        let token = std::env::var("ISY_FCM_TOKEN").expect("ISY_FCM_TOKEN");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name =
            fcm_send(&sa, &token, "iSyncYou", "Backup complete — E2E test", now).expect("fcm_send");
        eprintln!("FCM message name: {name}");
        assert!(name.contains("/messages/"));
    }
}
