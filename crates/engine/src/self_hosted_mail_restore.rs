//! Confirmed personal mail recovery through the ordinary JMAP write grant.
//! The selected MIME and exact intent are persisted in the encrypted provider
//! ledger before any remote write. No submission method is used.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use isyncyou_connectors::jmap::{JmapClient, JmapError, MailboxCreateResult};
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::{ProviderScope, Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use crate::provider_credential::ProviderCredentialStore;
use std::collections::HashMap;
use std::io::Read;

/// Platform implementation must sign with the already enrolled installation
/// key, never a newly generated replacement for an existing enrollment.
pub trait RecoveryDeviceSigner: Send + Sync {
    fn device_id(&self) -> &str;
    fn sign(&self, challenge: &[u8]) -> Result<String, String>;
}

/// Platform-owned installation key lookup; implementations must bind the
/// configured account and origin and must not create a replacement key.
pub trait RecoverySignerFactory: Send + Sync {
    fn load(&self, cfg: &Config, account: &str) -> Result<Box<dyn RecoveryDeviceSigner>, String>;
}

static RECOVERY_SIGNER_FACTORY: std::sync::OnceLock<std::sync::Arc<dyn RecoverySignerFactory>> = std::sync::OnceLock::new();

pub fn install_recovery_signer_factory(factory: std::sync::Arc<dyn RecoverySignerFactory>) -> Result<(), String> {
    RECOVERY_SIGNER_FACTORY.set(factory).map_err(|_| "Recovery signer factory already installed".into())
}

/// Host-only factory access. A signer is never exported to UI or model code.
pub fn load_registered_recovery_signer(cfg: &Config, account: &str) -> Result<Box<dyn RecoveryDeviceSigner>, String> {
    if let Some(factory) = RECOVERY_SIGNER_FACTORY.get() { return factory.load(cfg, account); }
    #[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
    return Ok(Box::new(load_linux_recovery_signer(cfg, account)?));
    #[cfg(not(all(target_os = "linux", feature = "desktop-keyring")))]
    Err("Recovery signer factory unavailable".into())
}

/// Linux installation identity. The private key is in the system keyring; the
/// protected provider ledger records the account/origin-bound public identity.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub struct LinuxRecoverySigner {
    installation: String,
    public_key: String,
    key: ring::signature::EcdsaKeyPair,
}

#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
impl RecoveryDeviceSigner for LinuxRecoverySigner {
    fn device_id(&self) -> &str { &self.installation }
    fn sign(&self, challenge: &[u8]) -> Result<String, String> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        if challenge.is_empty() || challenge.len() > 8192 { return Err("Invalid device challenge".into()); }
        let signature = self.key.sign(&ring::rand::SystemRandom::new(), challenge).map_err(|_| "Device signing unavailable")?;
        Ok(URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
impl LinuxRecoverySigner {
    pub fn public_key_der(&self) -> &str { &self.public_key }
}

/// Prove an existing Linux key to the recovery broker. The result is only a
/// pending candidate: an independently authorized management binding is still
/// required. Unknown outcomes are never submitted a second time automatically.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub fn prove_linux_recovery_device(cfg: &Config, account: &str, signer: &LinuxRecoverySigner,
                                   operation: &str) -> Result<LinuxRegistrationOutcome, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if operation.is_empty() || operation.len() > 128 || !operation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("Invalid device registration intent".into());
    }
    let (scope, slot) = linux_identity_scope(cfg, account)?;
    let store = crate::provider_store::open(cfg, account)?;
    let bound = linux_signer_from_record(&scope, &slot, &store)?;
    if bound.installation != signer.installation || bound.public_key != signer.public_key { return Err("Recovery device changed".into()); }
    crate::provider_auth::with_platform_credential_store(cfg, account, |credentials| {
    crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
    let native = JmapClient::discover(&scope.origin, token.to_owned()).map_err(|_| "Recovery identity unavailable")?;
    let origin = reqwest::Url::parse(&scope.origin).map_err(|_| "Invalid recovery origin")?;
    if origin.scheme() != "https" || origin.host_str().is_none() || origin.path() != "/" || !origin.username().is_empty() ||
        origin.password().is_some() || origin.query().is_some() || origin.fragment().is_some() { return Err("Invalid recovery origin".into()); }
    let der = URL_SAFE_NO_PAD.decode(&signer.public_key).map_err(|_| "Invalid device key")?;
    let fingerprint = format!("{:x}", Sha256::digest(der));
    let intent = json!({"action":"prove-recovery-device","native_account":native.account_id(),"device":signer.installation,
        "public_key":signer.public_key,"fingerprint":fingerprint}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous,state,result)) = store.provider_mutation(&scope, operation).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Device registration intent changed".into()); }
        return if state == "confirmed" {
            let saved = result.ok_or("Registration result missing")?;
            Ok(LinuxRegistrationOutcome::AlreadyCompleted(serde_json::from_str(&saved).map_err(|_| "Invalid registration result")?))
        } else { Ok(LinuxRegistrationOutcome::Unknown) };
    }
    if !store.claim_provider_mutation(&scope,operation,"recovery-device-proof",&intent,&digest).map_err(|e| e.to_string())? {
        return Ok(LinuxRegistrationOutcome::Unknown);
    }
    let http = reqwest::blocking::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30)).build().map_err(|_| "Recovery transport unavailable")?;
    let response = match http.post(origin.join("/recovery/enrollment/challenge").map_err(|_| "Invalid recovery route")?)
        .bearer_auth(&token).json(&json!({"device":signer.installation,"public_key":signer.public_key})).send() {
        Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown),
    };
    let response: Value = match RecoveryBroker::bounded(response,16384).and_then(|b| serde_json::from_slice(&b).map_err(|_| "Invalid device challenge".into())) {
        Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown),
    };
    let Some(id) = response.get("challenge_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let Some(bytes) = response.get("payload").and_then(Value::as_str).and_then(|s| URL_SAFE_NO_PAD.decode(s).ok()).filter(|b| b.len() <= 8192) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "Clock unavailable")?.as_secs();
    let expiry = payload.get("expires").and_then(Value::as_u64).unwrap_or(0);
    if payload.get("schema").and_then(Value::as_u64) != Some(1) || payload.get("action").and_then(Value::as_str) != Some("enroll-recovery-device") ||
        payload.get("challenge_id").and_then(Value::as_str) != Some(id) || payload.get("account").and_then(Value::as_str) != Some(native.account_id()) ||
        payload.get("subject").and_then(Value::as_str).is_none_or(str::is_empty) || payload.get("device").and_then(Value::as_str) != Some(signer.device_id()) ||
        payload.get("public_key").and_then(Value::as_str) != Some(signer.public_key_der()) || payload.get("fingerprint").and_then(Value::as_str) != Some(fingerprint.as_str()) ||
        response.get("fingerprint").and_then(Value::as_str) != Some(fingerprint.as_str()) || response.get("expires").and_then(Value::as_u64) != Some(expiry) || expiry < now || expiry > now + 900 {
        return Ok(LinuxRegistrationOutcome::Unknown);
    }
    let response = match http.post(origin.join("/recovery/enrollment/prove").map_err(|_| "Invalid recovery route")?)
        .bearer_auth(&token).json(&json!({"challenge_id":id,"signature":signer.sign(&bytes)?})).send() {
        Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown),
    };
    let result: Value = match RecoveryBroker::bounded(response,16384).and_then(|b| serde_json::from_slice(&b).map_err(|_| "Invalid device proof response".into())) {
        Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown),
    };
    if result.get("state").and_then(Value::as_str) != Some("awaiting_management") || result.get("device").and_then(Value::as_str) != Some(signer.device_id()) ||
        result.get("fingerprint").and_then(Value::as_str) != Some(fingerprint.as_str()) { return Ok(LinuxRegistrationOutcome::Unknown); }
    let receipt = json!({"challenge_id":id,"account":native.account_id(),"device":signer.installation,
        "fingerprint":fingerprint,"state":"awaiting_management"});
    if !store.finish_provider_mutation(&scope,operation,Some(&receipt.to_string())).map_err(|e| e.to_string())? { return Ok(LinuxRegistrationOutcome::Unknown); }
    Ok(LinuxRegistrationOutcome::Applied(receipt))
    })
    })
}

#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
fn linux_identity_scope(cfg: &Config, account: &str) -> Result<(ProviderScope, String), String> {
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        _ => return Err("Recovery account unavailable".into()),
    };
    // This initializes the existing desktop Secret Service store and ensures a
    // real account credential exists; no keyring or plaintext fallback.
    crate::provider_auth::self_hosted_access_token(cfg, account)?;
    let slot = format!("{:x}", Sha256::digest(format!("{account}\0{origin}").as_bytes()));
    Ok((ProviderScope { account_id:account.into(), origin, service:"recovery-device".into(), collection:"installation".into() }, slot))
}

#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
fn linux_signer_from_record(scope: &ProviderScope, slot: &str, store: &Store) -> Result<LinuxRecoverySigner, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::signature::KeyPair;
    let (_, intent, _, _, _) = store.provider_mutation_intent(scope, "linux-installation-v1").map_err(|e| e.to_string())?
        .ok_or("Recovery installation is not initialized")?;
    let record: Value = serde_json::from_str(&intent).map_err(|_| "Invalid recovery installation")?;
    let installation = record.get("installation").and_then(Value::as_str).filter(|s| s.len() >= 16 && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).ok_or("Invalid recovery installation")?;
    let public = record.get("public_key").and_then(Value::as_str).ok_or("Invalid recovery installation")?;
    let entry = keyring_core::Entry::new("org.silentspike.isyncyou.recovery-device", slot).map_err(|_| "System keyring unavailable")?;
    let encoded = entry.get_password().map_err(|_| "Recovery installation key unavailable")?;
    let private = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| "Invalid recovery installation key")?;
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &private, &ring::rand::SystemRandom::new())
        .map_err(|_| "Invalid recovery installation key")?;
    let mut der = vec![0x30,0x59,0x30,0x13,0x06,0x07,0x2a,0x86,0x48,0xce,0x3d,0x02,0x01,0x06,0x08,0x2a,0x86,0x48,0xce,0x3d,0x03,0x01,0x07,0x03,0x42,0x00];
    der.extend_from_slice(key.public_key().as_ref());
    if URL_SAFE_NO_PAD.encode(&der) != public { return Err("Recovery installation key mismatch".into()); }
    Ok(LinuxRecoverySigner { installation:installation.into(), public_key:public.into(), key })
}

/// Load only an existing key; absence is an error and never creates a replacement.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub fn load_linux_recovery_signer(cfg: &Config, account: &str) -> Result<LinuxRecoverySigner, String> {
    let (scope, slot) = linux_identity_scope(cfg, account)?;
    let store = crate::provider_store::open(cfg, account)?;
    linux_signer_from_record(&scope, &slot, &store)
}

/// Explicit one-time local initialization. A ledger claim precedes keyring
/// write, so a crash or concurrent initializer fails closed without replacing
/// an existing key. This does not claim that the server registration is active.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub fn initialize_linux_recovery_signer(cfg: &Config, account: &str) -> Result<LinuxRecoverySigner, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::{rand::SecureRandom, signature::KeyPair};
    let (scope, slot) = linux_identity_scope(cfg, account)?;
    let store = crate::provider_store::open(cfg, account)?;
    if store.provider_mutation(&scope, "linux-installation-v1").map_err(|e| e.to_string())?.is_some() {
        return linux_signer_from_record(&scope, &slot, &store);
    }
    let entry = keyring_core::Entry::new("org.silentspike.isyncyou.recovery-device", &slot).map_err(|_| "System keyring unavailable")?;
    match entry.get_password() { Err(keyring_core::Error::NoEntry) => {}, Ok(_) => return Err("Unbound recovery key already exists".into()), Err(_) => return Err("System keyring unavailable".into()) }
    let rng = ring::rand::SystemRandom::new();
    let private = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng).map_err(|_| "Device key generation unavailable")?;
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, private.as_ref(), &rng).map_err(|_| "Device key generation unavailable")?;
    let mut id = [0u8;24]; rng.fill(&mut id).map_err(|_| "Device identity unavailable")?;
    let installation = URL_SAFE_NO_PAD.encode(id);
    let mut der = vec![0x30,0x59,0x30,0x13,0x06,0x07,0x2a,0x86,0x48,0xce,0x3d,0x02,0x01,0x06,0x08,0x2a,0x86,0x48,0xce,0x3d,0x03,0x01,0x07,0x03,0x42,0x00];
    der.extend_from_slice(key.public_key().as_ref());
    let intent = json!({"installation":installation,"public_key":URL_SAFE_NO_PAD.encode(&der)}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if !store.claim_provider_mutation(&scope, "linux-installation-v1", "recovery-device", &intent, &digest).map_err(|e| e.to_string())? {
        return Err("Recovery installation initialization requires reconciliation".into());
    }
    entry.set_password(&URL_SAFE_NO_PAD.encode(private.as_ref())).map_err(|_| "Recovery installation key unavailable")?;
    linux_signer_from_record(&scope, &slot, &store)
}

#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub enum LinuxRegistrationOutcome {
    /// May contain provisional subscription credentials: retain only in native protected state.
    Applied(Value),
    AlreadyCompleted(Value),
    Unknown,
}

/// Execute one existing push-broker enrollment step. The host must actually
/// subscribe using the provisional credentials and present the received nonce
/// to `confirm`; a prepared identity alone is not an active registration.
/// `operation` must be durable and distinct for each effect. Unknown outcomes
/// stay in the ledger and are never replayed automatically.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
pub fn linux_recovery_registration_step(cfg: &Config, account: &str, signer: &LinuxRecoverySigner,
    endpoint: &str, action: &str, operation: &str, received_nonce: Option<&str>) -> Result<LinuxRegistrationOutcome, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if !matches!(action,"prepare" | "deliver" | "confirm") || operation.is_empty() || operation.len() > 128 ||
        !operation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        endpoint.is_empty() || endpoint.len() > 2048 || endpoint.chars().any(char::is_control) ||
        (action == "confirm") != received_nonce.is_some() ||
        received_nonce.is_some_and(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control)) {
        return Err("Invalid device registration intent".into());
    }
    let (scope, slot) = linux_identity_scope(cfg, account)?;
    let store = crate::provider_store::open(cfg, account)?;
    let bound = linux_signer_from_record(&scope, &slot, &store)?;
    if bound.installation != signer.installation || bound.public_key != signer.public_key { return Err("Recovery device changed".into()); }
    crate::provider_auth::with_platform_credential_store(cfg, account, |credentials| {
    crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
    let native = JmapClient::discover(&scope.origin, token.to_owned()).map_err(|_| "Recovery identity unavailable")?;
    let intent = json!({"action":action,"endpoint":endpoint,"device":signer.installation,"public_key":signer.public_key,
        "native_account":native.account_id(),"received_nonce":received_nonce}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, result)) = store.provider_mutation(&scope, operation).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Device registration intent changed".into()); }
        return if state == "confirmed" {
            let saved = result.ok_or("Registration result missing")?;
            Ok(LinuxRegistrationOutcome::AlreadyCompleted(serde_json::from_str(&saved).map_err(|_| "Invalid registration result")?))
        } else { Ok(LinuxRegistrationOutcome::Unknown) };
    }
    if !store.claim_provider_mutation(&scope, operation, "recovery-device-registration", &intent, &digest).map_err(|e| e.to_string())? {
        return Ok(LinuxRegistrationOutcome::Unknown);
    }
    let challenge_request = json!({"action":action,"installation_id":signer.installation,"endpoint":endpoint,"public_key":signer.public_key});
    let challenge = match crate::provider_push_broker::post_with_token(cfg,account,"challenge",&challenge_request,token) { Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown) };
    let Some(id) = challenge.get("challenge_id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let Some(bytes) = challenge.get("payload").and_then(Value::as_str).and_then(|s| URL_SAFE_NO_PAD.decode(s).ok()).filter(|b| b.len() <= 8192) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else { return Ok(LinuxRegistrationOutcome::Unknown); };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "Clock unavailable")?.as_secs();
    let expires = payload.get("expires").and_then(Value::as_u64).unwrap_or(0);
    if payload.get("version").and_then(Value::as_u64) != Some(1) || payload.get("challenge_id").and_then(Value::as_str) != Some(id) ||
        payload.get("account").and_then(Value::as_str) != Some(native.account_id()) || payload.get("action").and_then(Value::as_str) != Some(action) ||
        payload.get("installation_id").and_then(Value::as_str) != Some(signer.device_id()) || payload.get("endpoint").and_then(Value::as_str) != Some(endpoint) ||
        payload.get("public_key").and_then(Value::as_str) != Some(signer.public_key_der()) || expires < now || expires > now + 180 ||
        challenge.get("expires").and_then(Value::as_u64) != Some(expires) { return Ok(LinuxRegistrationOutcome::Unknown); }
    let mut request = json!({"challenge_id":id,"signature":signer.sign(&bytes)?});
    if let Some(nonce) = received_nonce { request["received_nonce"] = json!(nonce); }
    let response = match crate::provider_push_broker::post_with_token(cfg,account,action,&request,token) { Ok(value) => value, Err(_) => return Ok(LinuxRegistrationOutcome::Unknown) };
    let valid = match action {
        "prepare" => response.get("username").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) && response.get("password").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) && response.get("expires").and_then(Value::as_u64).is_some_and(|x| x > now),
        "deliver" => response.get("status").and_then(Value::as_str) == Some("sent"),
        "confirm" => response.get("status").and_then(Value::as_str) == Some("active"),
        _ => false,
    };
    if !valid || !store.finish_provider_mutation(&scope,operation,Some(&response.to_string())).map_err(|e| e.to_string())? { return Ok(LinuxRegistrationOutcome::Unknown); }
    Ok(LinuxRegistrationOutcome::Applied(response))
    })
    })
}

/// Native-only broker transport. The host retains its job-to-source binding in
/// protected storage; neither tokens nor returned object bytes go to the UI.
pub struct RecoveryBroker {
    http: reqwest::blocking::Client,
    origin: reqwest::Url,
    token: String,
    native_account: String,
}

pub struct RecoveredPayload {
    pub bytes: Vec<u8>,
    pub kind: String,
    pub encoding: String,
    pub sha256: String,
}

/// Metadata is authenticated by the broker and tied to a durably recorded
/// account/device/source job. Size and digest refer to returned encoded bytes.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BoundRecoveryStatus {
    pub state: String,
    pub kind: String,
    pub encoding: String,
    pub size: u64,
    pub sha256: String,
    pub selection_sha256: String,
}

fn parse_bound_status(value: &Value, selection_sha256: String) -> Result<BoundRecoveryStatus, String> {
    let state = value.get("state").and_then(Value::as_str).filter(|s| matches!(*s,"queued" | "extracting" | "available" | "expired")).ok_or("Invalid recovery state")?;
    let size = value.get("size").and_then(Value::as_u64).filter(|n| *n <= 100 * 1024 * 1024).ok_or("Invalid recovery size")?;
    let digest = value.get("digest").and_then(Value::as_str).filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).ok_or("Invalid recovery digest")?;
    let kind = value.get("kind").and_then(Value::as_str).filter(|s| matches!(*s,"event" | "task" | "contact" | "mail")).ok_or("Invalid recovery kind")?;
    let encoding = value.get("encoding").and_then(Value::as_str).filter(|s| matches!(*s,"raw" | "mime" | "isync-mime-1")).ok_or("Invalid recovery encoding")?;
    if (kind == "mail") != (encoding != "raw") { return Err("Recovery encoding mismatch".into()); }
    Ok(BoundRecoveryStatus { state:state.into(), kind:kind.into(), encoding:encoding.into(), size, sha256:digest.into(), selection_sha256 })
}

impl RecoveryBroker {
    fn connect_with_token(cfg: &Config, account: &str, token: &str) -> Result<Self, String> {
        let origin = match cfg.provider_accounts.get(account) {
            Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
            _ => return Err("Recovery account unavailable".into()),
        };
        let parsed = reqwest::Url::parse(origin).map_err(|_| "Invalid recovery origin")?;
        if parsed.scheme() != "https" || parsed.host_str().is_none() || parsed.path() != "/" ||
            !parsed.username().is_empty() || parsed.password().is_some() || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err("Invalid recovery origin".into());
        }
        let native = JmapClient::discover(origin, token.to_owned()).map_err(|_| "Recovery identity unavailable")?;
        let http = reqwest::blocking::Client::builder().redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30)).build().map_err(|_| "Recovery transport unavailable")?;
        Ok(Self { http, origin: parsed, token:token.into(), native_account: native.account_id().to_owned() })
    }

    fn bounded(response: reqwest::blocking::Response, limit: u64) -> Result<Vec<u8>, String> {
        if !response.status().is_success() { return Err("Recovery request unavailable".into()); }
        let mut bytes = Vec::new();
        response.take(limit + 1).read_to_end(&mut bytes).map_err(|_| "Recovery response unavailable")?;
        if bytes.len() as u64 > limit { return Err("Recovery response too large".into()); }
        Ok(bytes)
    }

    fn signed(&self, signer: &dyn RecoveryDeviceSigner, action: &str, effect: &Value)
        -> Result<(String, String), String> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let device = signer.device_id();
        if device.is_empty() || device.len() > 256 || device.chars().any(char::is_control) { return Err("Invalid recovery device".into()); }
        let challenge = self.http.post(self.origin.join("/recovery/challenge").map_err(|_| "Invalid recovery route")?)
            .bearer_auth(&self.token).json(&json!({"device":device,"action":action,"effect":effect})).send()
            .map_err(|_| "Recovery challenge unavailable")?;
        let response: Value = serde_json::from_slice(&Self::bounded(challenge, 16384)?).map_err(|_| "Invalid recovery challenge")?;
        let id = response.get("challenge_id").and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= 128).ok_or("Invalid recovery challenge")?;
        let encoded = response.get("payload").and_then(Value::as_str).ok_or("Invalid recovery challenge")?;
        let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| "Invalid recovery challenge")?;
        if bytes.len() > 8192 { return Err("Invalid recovery challenge".into()); }
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid recovery challenge")?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "Clock unavailable")?.as_secs();
        let expiry = payload.get("expires").and_then(Value::as_u64).ok_or("Invalid recovery challenge")?;
        if payload.get("schema").and_then(Value::as_u64) != Some(1) || payload.get("challenge_id").and_then(Value::as_str) != Some(id) ||
            payload.get("native_account").and_then(Value::as_str) != Some(self.native_account.as_str()) ||
            payload.get("subject").and_then(Value::as_str).is_none_or(str::is_empty) ||
            payload.get("device").and_then(Value::as_str) != Some(device) || payload.get("action").and_then(Value::as_str) != Some(action) ||
            payload.get("effect") != Some(effect) || expiry < now || expiry > now + 180 ||
            response.get("expires").and_then(Value::as_u64) != Some(expiry) { return Err("Recovery challenge mismatch".into()); }
        let signature = signer.sign(&bytes)?;
        if signature.is_empty() || signature.len() > 1024 || !signature.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("Invalid recovery signature".into()); }
        Ok((id.to_owned(), signature))
    }

    /// Read one bounded catalog page for the account authenticated by JMAP.
    pub fn catalog(&self, signer: &dyn RecoveryDeviceSigner, after: u64) -> Result<Value, String> {
        if after > i64::MAX as u64 { return Err("Invalid catalog cursor".into()); }
        let effect = json!({"catalog_after":after});
        let (challenge, signature) = self.signed(signer, "read", &effect)?;
        let url = self.origin.join(&format!("/recovery/catalog/{after}")).map_err(|_| "Invalid recovery route")?;
        let response = self.http.get(url).bearer_auth(&self.token).header("X-Recovery-Device", signer.device_id())
            .header("X-Recovery-Challenge", challenge).header("X-Recovery-Signature", signature).send().map_err(|_| "Recovery catalog unavailable")?;
        serde_json::from_slice(&Self::bounded(response, 256 * 1024)?).map_err(|_| "Invalid recovery catalog".into())
    }

    /// Challenge issuance does not create a recovery job and may fail before
    /// the local job claim. The create request follows the durable claim.
    fn prepare_job(&self, signer: &dyn RecoveryDeviceSigner, generation: &str, object: &str,
                   version: &str, confirmation: &str) -> Result<(Value, String, String), String> {
        if [generation, object, version, confirmation].iter().any(|v| v.is_empty() || v.len() > 512 || v.chars().any(char::is_control)) {
            return Err("Invalid recovery selection".into());
        }
        let effect = json!({"generation":generation,"object":object,"version":version,"destination":"recovered","confirmation":confirmation});
        let (challenge, signature) = self.signed(signer, "create", &effect)?;
        Ok((effect, challenge, signature))
    }

    fn create_job(&self, signer: &dyn RecoveryDeviceSigner, effect: &Value, challenge: String, signature: String) -> Result<Value, String> {
        let response = self.http.post(self.origin.join("/recovery/jobs").map_err(|_| "Invalid recovery route")?)
            .bearer_auth(&self.token).header("X-Recovery-Device", signer.device_id())
            .header("X-Recovery-Challenge", challenge).header("X-Recovery-Signature", signature).json(effect).send()
            .map_err(|_| "Recovery job outcome unknown")?;
        let data = Self::bounded(response, 16384).map_err(|_| "Recovery job outcome unknown")?;
        serde_json::from_slice(&data).map_err(|_| "Recovery job outcome unknown".into())
    }

    fn job_read(&self, signer: &dyn RecoveryDeviceSigner, id: &str, result: bool) -> Result<Vec<u8>, String> {
        // Canonical UUID spelling prevents path changes and binds the challenge.
        if id.len() != 36 || id.bytes().enumerate().any(|(i,b)| {
            if [8,13,18,23].contains(&i) { b != b'-' } else { !(b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
        }) { return Err("Invalid recovery job".into()); }
        let (challenge, signature) = self.signed(signer, "read", &json!({"id":id}))?;
        let suffix = if result { "/result" } else { "" };
        let url = self.origin.join(&format!("/recovery/jobs/{id}{suffix}")).map_err(|_| "Invalid recovery route")?;
        let response = self.http.get(url).bearer_auth(&self.token).header("X-Recovery-Device", signer.device_id())
            .header("X-Recovery-Challenge", challenge).header("X-Recovery-Signature", signature).send().map_err(|_| "Recovery result unavailable")?;
        Self::bounded(response, if result { 100 * 1024 * 1024 } else { 16384 })
    }

    pub fn job_status(&self, signer: &dyn RecoveryDeviceSigner, id: &str) -> Result<Value, String> {
        let value: Value = serde_json::from_slice(&self.job_read(signer, id, false)?).map_err(|_| "Invalid recovery status")?;
        if value.get("id").and_then(Value::as_str) != Some(id) { return Err("Recovery job mismatch".into()); }
        Ok(value)
    }

    /// Accept only bytes matching the immediately authenticated job status.
    fn result(&self, signer: &dyn RecoveryDeviceSigner, id: &str) -> Result<RecoveredPayload, String> {
        let status = self.job_status(signer, id)?;
        let status = parse_bound_status(&status, String::new())?;
        if status.state != "available" { return Err("Recovery result unavailable".into()); }
        let bytes = self.job_read(signer, id, true)?;
        if bytes.len() as u64 != status.size || format!("{:x}", Sha256::digest(&bytes)) != status.sha256 { return Err("Recovery result integrity failure".into()); }
        Ok(RecoveredPayload { bytes, kind:status.kind, encoding:status.encoding, sha256:status.sha256 })
    }
}

/// Read the account's catalog while serialized with refresh and logout.
pub fn recovery_catalog_with_store(cfg: &Config, account: &str, signer: &dyn RecoveryDeviceSigner,
                                   credentials: &dyn ProviderCredentialStore, after: u64) -> Result<Value, String> {
    crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
        RecoveryBroker::connect_with_token(cfg, account, token)?.catalog(signer, after)
    })
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RecoverySelection {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub generation: String,
    pub object: String,
    pub version: String,
    pub confirmation: String,
}

pub enum RecoveryJobOutcome { Created(String), Unknown }

/// Persist this exact discovery and operation identity in the encrypted host
/// intent before asking the user to approve creating the personal collection.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedRecoveryCollection {
    pub account_id: String,
    pub origin: String,
    pub entry: String,
    pub service: String,
    pub home: String,
    pub principal: String,
    pub target: String,
    pub operation_id: String,
    pub approval_sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryCollectionOutcome { Ready(String), Rejected, Conflict, Unknown }

fn collection_approval(op: &ApprovedRecoveryCollection) -> String {
    let effect = json!({"schema":"recovery-collection-approval-v1", "account_id":op.account_id,
        "origin":op.origin, "entry":op.entry, "service":op.service, "home":op.home,
        "principal":op.principal, "target":op.target, "operation_id":op.operation_id});
    format!("{:x}", Sha256::digest(effect.to_string().as_bytes()))
}

pub fn prepare_recovery_collection_with_store(cfg: &Config, account: &str, entry: &str, service: &str,
    credentials: &dyn ProviderCredentialStore) -> Result<ApprovedRecoveryCollection, String> {
    use ring::rand::SecureRandom;
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        _ => return Err("Recovery account unavailable".into()),
    };
    if !matches!(service, "calendar" | "contacts") { return Err("Unsupported recovery collection".into()); }
    let mut random = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut random).map_err(|_| "Recovery identity unavailable")?;
    let operation_id: String = random.iter().map(|b| format!("{b:02x}")).collect();
    crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
        let client = isyncyou_connectors::dav::DavClient::new(&origin, token.to_owned()).map_err(|e| e.to_string())?;
        let home = client.personal_home(entry, service).map_err(|e| e.to_string())?;
        let target = home.recovered_target(&operation_id).map_err(|e| e.to_string())?;
        let mut op = ApprovedRecoveryCollection { account_id:account.into(), origin:origin.clone(), entry:entry.into(),
            service:service.into(), home:home.home_url().into(), principal:home.principal_url().into(),
            target, operation_id, approval_sha256:String::new() };
        op.approval_sha256 = collection_approval(&op);
        Ok(op)
    })
}

pub fn ensure_recovery_collection_with_store(cfg: &Config, op: &ApprovedRecoveryCollection,
    approved_sha256: &str, credentials: &dyn ProviderCredentialStore) -> Result<RecoveryCollectionOutcome, String> {
    use isyncyou_connectors::dav::{DavClient, DavError};
    if op.approval_sha256 != approved_sha256 || collection_approval(op) != approved_sha256 ||
        !matches!(cfg.provider_accounts.get(&op.account_id), Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin) {
        return Err("Recovery collection approval or account changed".into());
    }
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &op.account_id, credentials, |token| {
        let client = DavClient::new(&op.origin, token.to_owned()).map_err(|e| e.to_string())?;
        let home = client.personal_home(&op.entry, &op.service).map_err(|e| e.to_string())?;
        if home.home_url() != op.home || home.principal_url() != op.principal || home.service() != op.service ||
            home.recovered_target(&op.operation_id).map_err(|e| e.to_string())? != op.target {
            return Err("Recovery collection identity changed".into());
        }
        let scope = ProviderScope { account_id:op.account_id.clone(), origin:op.origin.clone(),
            service:"recovery-collection".into(), collection:op.home.clone() };
        let intent = serde_json::to_string(op).map_err(|_| "Invalid collection intent")?;
        let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
        let reconcile = || -> Result<RecoveryCollectionOutcome, String> {
            match client.read_recovered_collection(&home, &op.operation_id) {
                Ok(true) => if store.finish_provider_mutation(&scope, &op.operation_id, Some(&op.target)).map_err(|e| e.to_string())? {
                    Ok(RecoveryCollectionOutcome::Ready(op.target.clone()))
                } else { Ok(RecoveryCollectionOutcome::Unknown) },
                _ => Ok(RecoveryCollectionOutcome::Unknown),
            }
        };
        if let Some((previous, state, result)) = store.provider_mutation(&scope, &op.operation_id).map_err(|e| e.to_string())? {
            if previous != digest { return Err("Recovery collection differs from recorded intent".into()); }
            return match state.as_str() {
                "confirmed" if result.as_deref() == Some(op.target.as_str()) =>
                    if matches!(client.read_recovered_collection(&home, &op.operation_id), Ok(true)) {
                        Ok(RecoveryCollectionOutcome::Ready(op.target.clone()))
                    } else { Ok(RecoveryCollectionOutcome::Unknown) },
                "rejected" => Ok(RecoveryCollectionOutcome::Rejected),
                "conflict" => Ok(RecoveryCollectionOutcome::Conflict),
                _ => reconcile(),
            };
        }
        match client.read_recovered_collection(&home, &op.operation_id) {
            Ok(false) => {},
            Ok(true) => return Ok(RecoveryCollectionOutcome::Conflict),
            Err(_) => return Ok(RecoveryCollectionOutcome::Unknown),
        }
        if !store.claim_provider_mutation(&scope, &op.operation_id, "recovery-collection", &intent, &digest).map_err(|e| e.to_string())? {
            return Ok(RecoveryCollectionOutcome::Unknown);
        }
        match client.create_recovered_collection(&home, &op.operation_id) {
            Ok(()) => reconcile(),
            Err(DavError::Http(code)) if (400..500).contains(&code) && code != 408 && code != 429 && code != 409 && code != 412 =>
                if store.finish_provider_mutation(&scope, &op.operation_id, None).map_err(|e| e.to_string())? { Ok(RecoveryCollectionOutcome::Rejected) } else { Ok(RecoveryCollectionOutcome::Unknown) },
            Err(DavError::Http(409 | 412)) | Err(DavError::Conflict) =>
                if store.conflict_provider_mutation(&scope, &op.operation_id).map_err(|e| e.to_string())? { Ok(RecoveryCollectionOutcome::Conflict) } else { Ok(RecoveryCollectionOutcome::Unknown) },
            Err(_) => reconcile(),
        }
    })
}

/// Journal the account, registered device and exact protected selection before
/// sending a create request. Re-entry only returns the recorded native ID; an
/// ambiguous dispatch with no ID remains unknown and is never submitted again.
pub fn create_bound_recovery_job(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                                 credentials: &dyn ProviderCredentialStore) -> Result<RecoveryJobOutcome, String> {
    if selection.operation_id.is_empty() || selection.operation_id.len() > 128 ||
        !selection.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&selection.generation, &selection.object, &selection.version].iter().any(|s| s.len() != 64 ||
            !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) ||
        selection.confirmation.is_empty() || selection.confirmation.len() > 512 || selection.confirmation.chars().any(char::is_control) ||
        signer.device_id().is_empty() || signer.device_id().len() > 256 || signer.device_id().chars().any(char::is_control) {
        return Err("Invalid recovery operation".into());
    }
    match cfg.provider_accounts.get(&selection.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &selection.origin => {},
        _ => return Err("Recovery account or origin changed".into()),
    }
    let store = crate::provider_store::open(cfg, &selection.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &selection.account_id, credentials, |token| {
        create_bound_recovery_job_with_token(cfg, selection, signer, &store, token)
    })
}

fn create_bound_recovery_job_with_token(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                                        store: &Store, token: &str) -> Result<RecoveryJobOutcome, String> {
    let broker = RecoveryBroker::connect_with_token(cfg, &selection.account_id, token)?;
    let scope = ProviderScope { account_id:selection.account_id.clone(), origin:selection.origin.clone(),
        service:"recovery".into(), collection:broker.native_account.clone() };
    let intent = json!({"selection":selection,"device":signer.device_id(),"native_account":broker.native_account}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, _, id)) = store.provider_mutation(&scope, &selection.operation_id).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Recovery selection differs from recorded intent".into()); }
        return Ok(match id { Some(id) => RecoveryJobOutcome::Created(id), None => RecoveryJobOutcome::Unknown });
    }
    let (effect, challenge, signature) = broker.prepare_job(signer, &selection.generation, &selection.object, &selection.version, &selection.confirmation)?;
    if !store.claim_provider_mutation(&scope, &selection.operation_id, "recovery-job", &intent, &digest).map_err(|e| e.to_string())? {
        return Ok(RecoveryJobOutcome::Unknown);
    }
    let response = match broker.create_job(signer, &effect, challenge, signature) {
        Ok(value) => value, Err(_) => return Ok(RecoveryJobOutcome::Unknown),
    };
    let Some(id) = response.get("id").and_then(Value::as_str) else { return Ok(RecoveryJobOutcome::Unknown); };
    if id.len() != 36 || id.bytes().enumerate().any(|(i,b)| if [8,13,18,23].contains(&i) { b != b'-' } else { !(b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }) {
        return Ok(RecoveryJobOutcome::Unknown);
    }
    if !store.checkpoint_provider_mutation(&scope, &selection.operation_id, &digest, id).map_err(|e| e.to_string())? {
        return Ok(RecoveryJobOutcome::Unknown);
    }
    Ok(RecoveryJobOutcome::Created(id.into()))
}

/// Read only the job that was durably tied to this exact account, device and
/// source selection. The returned payload still stays inside the native host.
fn bound_recovery_job_with_token(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                                 store: &Store, token: &str) -> Result<(RecoveryBroker, String), String> {
    match cfg.provider_accounts.get(&selection.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &selection.origin => {},
        _ => return Err("Recovery account or origin changed".into()),
    }
    let broker = RecoveryBroker::connect_with_token(cfg, &selection.account_id, token)?;
    let scope = ProviderScope { account_id:selection.account_id.clone(), origin:selection.origin.clone(),
        service:"recovery".into(), collection:broker.native_account.clone() };
    let intent = json!({"selection":selection,"device":signer.device_id(),"native_account":broker.native_account}).to_string();
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    let (recorded, _, id) = store.provider_mutation(&scope, &selection.operation_id).map_err(|e| e.to_string())?
        .ok_or("Recovery job not recorded")?;
    if recorded != digest { return Err("Recovery selection differs from recorded intent".into()); }
    Ok((broker, id.ok_or("Recovery job outcome unknown")?))
}

/// Obtain trustworthy kind, encoding, length and digest before reserving or
/// downloading bytes. The host must still hash every downloaded payload.
pub fn bound_recovery_status(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                             credentials: &dyn ProviderCredentialStore) -> Result<BoundRecoveryStatus, String> {
    let store = crate::provider_store::open(cfg, &selection.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &selection.account_id, credentials, |token| {
        let (broker, id) = bound_recovery_job_with_token(cfg, selection, signer, &store, token)?;
        parse_bound_status(&broker.job_status(signer, &id)?, recovery_selection_digest(selection, signer.device_id()))
    })
}

fn recovery_selection_digest(selection: &RecoverySelection, device: &str) -> String {
    format!("{:x}", Sha256::digest(json!({"schema":"recovery-selection-v1","selection":selection,"device":device}).to_string().as_bytes()))
}

pub fn read_bound_recovery_result(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                                  credentials: &dyn ProviderCredentialStore) -> Result<RecoveredPayload, String> {
    let store = crate::provider_store::open(cfg, &selection.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &selection.account_id, credentials, |token| {
        let (broker, id) = bound_recovery_job_with_token(cfg, selection, signer, &store, token)?;
        broker.result(signer, &id)
    })
}

/// Fetch a durably bound result and build the exact calendar import intent.
/// Caller supplies a persisted UID mapping and must confirm the returned hash.
pub fn prepare_bound_calendar_restore(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
                                      credentials: &dyn ProviderCredentialStore, entry: &str, collection: &str,
                                      uid_map: HashMap<String, String>) -> Result<(ApprovedCalendarRestore, CalendarRestoreSummary), String> {
    let payload = read_bound_recovery_result(cfg, selection, signer, credentials)?;
    calendar_restore_from_payload(selection, payload, entry, collection, uid_map)
}

fn calendar_restore_from_payload(selection: &RecoverySelection, payload: RecoveredPayload, entry: &str, collection: &str,
                                 uid_map: HashMap<String, String>) -> Result<(ApprovedCalendarRestore, CalendarRestoreSummary), String> {
    let kind = match (payload.kind.as_str(), payload.encoding.as_str()) {
        ("event", "raw") => CalendarRestoreKind::Event,
        ("task", "raw") => CalendarRestoreKind::Task,
        _ => return Err("Recovery result is not a calendar object".into()),
    };
    let op = ApprovedCalendarRestore { operation_id:selection.operation_id.clone(), account_id:selection.account_id.clone(),
        origin:selection.origin.clone(), entry:entry.into(), collection:collection.into(), source_generation:selection.generation.clone(),
        source_object:selection.object.clone(), source_version:selection.version.clone(), original_sha256:payload.sha256,
        original:payload.bytes, uid_map, kind };
    let summary = prepare_calendar_restore(&op)?;
    Ok((op, summary))
}

/// Prepare a single independent object with a fresh UID. Relations to objects
/// outside this payload fail closed; use an explicitly persisted batch mapping
/// when related objects are being restored together. Persist the returned intent
/// before confirmation: preparing again generates a different UID.
pub fn prepare_bound_calendar_restore_auto_uid(cfg: &Config, selection: &RecoverySelection, signer: &dyn RecoveryDeviceSigner,
    credentials: &dyn ProviderCredentialStore, entry: &str, collection: &str)
    -> Result<(ApprovedCalendarRestore, CalendarRestoreSummary), String> {
    use isyncyou_connectors::dav::edit::{resource_index, ResourceIndex};
    use ring::rand::SecureRandom;
    let payload = read_bound_recovery_result(cfg, selection, signer, credentials)?;
    let uid = match resource_index(&payload.bytes)? {
        ResourceIndex::Event { uid } => uid,
        ResourceIndex::Task(task) => {
            if task.parent_uid.is_some() { return Err("Related task needs a persisted recovery mapping".into()); }
            task.uid
        },
    };
    let mut random = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut random).map_err(|_| "Recovery identity unavailable")?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let uid_map = HashMap::from([(uid, format!("{suffix}@recovered.invalid"))]);
    calendar_restore_from_payload(selection, payload, entry, collection, uid_map)
}

/// Prepare a related set atomically in local memory before any remote write.
/// The host must persist *all* returned intents and obtain one confirmation for
/// the complete set before dispatching; each write retains its own journal.
pub fn prepare_bound_calendar_restore_batch(cfg: &Config, selections: &[RecoverySelection], signer: &dyn RecoveryDeviceSigner,
    credentials: &dyn ProviderCredentialStore, entry: &str, collection: &str)
    -> Result<(ApprovedCalendarBatch, Vec<CalendarRestoreSummary>), String> {
    validate_recovery_selection_set(selections)?;
    let mut statuses = Vec::with_capacity(selections.len());
    let mut total = 0usize;
    for selection in selections {
        let status = bound_recovery_status(cfg, selection, signer, credentials)?;
        if status.state != "available" || !matches!(status.kind.as_str(), "event" | "task") || status.encoding != "raw" || status.size > 16 * 1024 * 1024 {
            return Err("Recovery calendar object unavailable or exceeds capacity".into());
        }
        total = total.checked_add(status.size as usize).ok_or("Recovery batch exceeds capacity")?;
        if total > 256 * 1024 * 1024 { return Err("Recovery batch exceeds protected staging capacity".into()); }
        statuses.push(status);
    }
    let mut originals = Vec::with_capacity(selections.len());
    for (selection, status) in selections.iter().zip(statuses) {
        let payload = read_bound_recovery_result(cfg, selection, signer, credentials)?;
        if payload.bytes.len() as u64 != status.size || payload.sha256 != status.sha256 || payload.kind != status.kind || payload.encoding != status.encoding {
            return Err("Recovery result changed during preparation".into());
        }
        originals.push((selection, payload));
    }
    seal_calendar_batch(calendar_batch_from_payloads(originals, entry, collection)?)
}

fn validate_recovery_selection_set(selections: &[RecoverySelection]) -> Result<(), String> {
    if selections.is_empty() || selections.len() > 32 { return Err("Invalid recovery batch size".into()); }
    let first = &selections[0];
    let mut operations = std::collections::HashSet::new();
    if selections.iter().any(|s| s.account_id != first.account_id || s.origin != first.origin || !operations.insert(&s.operation_id)) {
        return Err("Recovery batch identity mismatch".into());
    }
    Ok(())
}

fn seal_calendar_batch(prepared: Vec<(ApprovedCalendarRestore, CalendarRestoreSummary)>)
    -> Result<(ApprovedCalendarBatch, Vec<CalendarRestoreSummary>), String> {
    let (items, summaries): (Vec<_>, Vec<_>) = prepared.into_iter().unzip();
    let approval_sha256 = calendar_batch_approval(&items)?;
    Ok((ApprovedCalendarBatch { items, approval_sha256 }, summaries))
}

/// Rebuild from protected native chunks after the host authenticated and
/// durably bound every status to its selection. No arbitrary UI bytes belong
/// on this seam. Check the original size/hash again on every reconstruction.
pub fn prepare_staged_calendar_batch(selections: &[RecoverySelection], statuses: &[BoundRecoveryStatus], signer: &dyn RecoveryDeviceSigner,
    payloads: Vec<Vec<u8>>, entry: &str, collection: &str)
    -> Result<(ApprovedCalendarBatch, Vec<CalendarRestoreSummary>), String> {
    validate_recovery_selection_set(selections)?;
    if statuses.len() != selections.len() || payloads.len() != selections.len() { return Err("Recovery staging identity mismatch".into()); }
    let mut originals = Vec::with_capacity(selections.len());
    let mut total = 0usize;
    for ((selection, status), bytes) in selections.iter().zip(statuses).zip(payloads) {
        if status.selection_sha256 != recovery_selection_digest(selection, signer.device_id()) || status.state != "available" || !matches!(status.kind.as_str(), "event" | "task") || status.encoding != "raw" ||
            bytes.len() > 16 * 1024 * 1024 || bytes.len() as u64 != status.size || format!("{:x}",Sha256::digest(&bytes)) != status.sha256 {
            return Err("Recovery staged payload mismatch".into());
        }
        total = total.checked_add(bytes.len()).ok_or("Recovery batch exceeds capacity")?;
        if total > 256 * 1024 * 1024 { return Err("Recovery batch exceeds protected staging capacity".into()); }
        originals.push((selection, RecoveredPayload {bytes, kind:status.kind.clone(), encoding:status.encoding.clone(), sha256:status.sha256.clone()}));
    }
    seal_calendar_batch(calendar_batch_from_payloads(originals, entry, collection)?)
}

fn calendar_batch_from_payloads(originals: Vec<(&RecoverySelection, RecoveredPayload)>, entry: &str, collection: &str)
    -> Result<Vec<(ApprovedCalendarRestore, CalendarRestoreSummary)>, String> {
    use isyncyou_connectors::dav::edit::{resource_index, ResourceIndex};
    use ring::rand::SecureRandom;
    let mut mapping = HashMap::new();
    let mut pending = Vec::with_capacity(originals.len());
    for (selection, payload) in originals {
        let (uid, parent) = match resource_index(&payload.bytes)? {
            ResourceIndex::Event { uid } => (uid, None),
            ResourceIndex::Task(task) => (task.uid, task.parent_uid),
        };
        if uid.is_empty() || uid.len() > 4096 || uid.chars().any(char::is_control) ||
            parent.as_ref().is_some_and(|value| value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control)) {
            return Err("Invalid recovery relationship identity".into());
        }
        let mut random = [0u8; 16];
        ring::rand::SystemRandom::new().fill(&mut random).map_err(|_| "Recovery identity unavailable")?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        if mapping.insert(uid.clone(), format!("{suffix}@recovered.invalid")).is_some() {
            return Err("Recovery batch contains duplicate source identity".into());
        }
        pending.push((selection, payload, uid, parent));
    }
    if pending.iter().any(|(_,_,_,parent)| parent.as_ref().is_some_and(|id| !mapping.contains_key(id))) {
        return Err("Recovery batch is missing a related parent".into());
    }
    let mut ordered = Vec::with_capacity(pending.len());
    let mut completed = std::collections::HashSet::new();
    while !pending.is_empty() {
        let index = pending.iter().position(|(_,_,_,parent)| parent.as_ref().is_none_or(|id| completed.contains(id)))
            .ok_or("Recovery batch contains a relationship cycle")?;
        let (selection,payload,uid,_) = pending.remove(index);
        completed.insert(uid);
        ordered.push(calendar_restore_from_payload(selection, payload, entry, collection, mapping.clone())?);
    }
    Ok(ordered)
}

/// Persist the whole structure in the protected confirmation ledger before
/// any native writes. The hash binds the order and every per-object effect.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedCalendarBatch {
    pub items: Vec<ApprovedCalendarRestore>,
    pub approval_sha256: String,
}

fn calendar_batch_approval(items: &[ApprovedCalendarRestore]) -> Result<String, String> {
    if items.is_empty() || items.len() > 32 { return Err("Invalid recovery batch size".into()); }
    let first = &items[0];
    let mut operations = std::collections::HashSet::new();
    let mut hashes = Vec::with_capacity(items.len());
    let mut total = 0usize;
    for item in items {
        total = total.checked_add(item.original.len()).ok_or("Recovery batch exceeds capacity")?;
        if total > 256 * 1024 * 1024 { return Err("Recovery batch exceeds protected staging capacity".into()); }
        if item.account_id != first.account_id || item.origin != first.origin || item.collection != first.collection ||
           item.uid_map != first.uid_map || !operations.insert(&item.operation_id) {
            return Err("Recovery batch intent mismatch".into());
        }
        hashes.push(prepare_calendar_restore(item)?.approval_sha256);
    }
    Ok(format!("{:x}", Sha256::digest(json!({"schema":"calendar-recovery-batch-v1","items":hashes}).to_string().as_bytes())))
}

#[derive(Debug, PartialEq, Eq)]
pub enum CalendarBatchOutcome {
    Confirmed(Vec<String>),
    Stopped { completed: Vec<String>, next_index: usize, reason: &'static str },
}

/// Stops at the first uncertain, conflicting or rejected item. A repeated call
/// reconciles already journaled writes and never blindly reissues them.
pub fn execute_calendar_restore_batch_with_store(cfg: &Config, batch: &ApprovedCalendarBatch, approved_sha256: &str,
    credentials: &dyn ProviderCredentialStore) -> Result<CalendarBatchOutcome, String> {
    if calendar_batch_approval(&batch.items)? != approved_sha256 || batch.approval_sha256 != approved_sha256 {
        return Err("Calendar batch approval does not match intent".into());
    }
    let first = batch.items.first().ok_or("Invalid recovery batch size")?;
    match cfg.provider_accounts.get(&first.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &first.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let store = crate::provider_store::open(cfg, &first.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &first.account_id, credentials, |token| {
        execute_calendar_batch(batch, approved_sha256, |item, hash| execute_calendar_restore_with_token(cfg, item, hash, &store, token))
    })
}

fn execute_calendar_batch<F>(batch: &ApprovedCalendarBatch, approved_sha256: &str, mut dispatch: F) -> Result<CalendarBatchOutcome, String>
where F: FnMut(&ApprovedCalendarRestore, &str) -> Result<crate::dav_live::RecoveryImportOutcome, String> {
    let digest = calendar_batch_approval(&batch.items)?;
    if digest != batch.approval_sha256 || digest != approved_sha256 { return Err("Calendar batch approval does not match intent".into()); }
    let mut completed = Vec::new();
    for (index,item) in batch.items.iter().enumerate() {
        let item_hash = prepare_calendar_restore(item)?.approval_sha256;
        let outcome = dispatch(item, &item_hash);
        match outcome {
            Ok(crate::dav_live::RecoveryImportOutcome::Confirmed { href }) => completed.push(href),
            Ok(crate::dav_live::RecoveryImportOutcome::Conflict) => return Ok(CalendarBatchOutcome::Stopped {completed,next_index:index,reason:"conflict"}),
            Ok(crate::dav_live::RecoveryImportOutcome::Rejected) => return Ok(CalendarBatchOutcome::Stopped {completed,next_index:index,reason:"rejected"}),
            Ok(crate::dav_live::RecoveryImportOutcome::Unknown) | Err(_) => return Ok(CalendarBatchOutcome::Stopped {completed,next_index:index,reason:"unknown"}),
        }
    }
    Ok(CalendarBatchOutcome::Confirmed(completed))
}

/// A protected original and its complete, stable import intent. Store this
/// only in the host's protected confirmation ledger; never expose its bytes to
/// the model or browser. The host must authenticate the catalog/result first.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedCalendarRestore {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub entry: String,
    pub collection: String,
    pub source_generation: String,
    pub source_object: String,
    pub source_version: String,
    pub original_sha256: String,
    #[serde(with = "recovery_payload_base64")]
    pub original: Vec<u8>,
    /// Persist before approval; use new UIDs and map any recovered relations.
    pub uid_map: HashMap<String, String>,
    pub kind: CalendarRestoreKind,
}

mod recovery_payload_base64 {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(value))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > 24 * 1024 * 1024 { return Err(serde::de::Error::custom("Recovery payload exceeds limit")); }
        let bytes = STANDARD.decode(encoded).map_err(serde::de::Error::custom)?;
        if bytes.len() > 16 * 1024 * 1024 { return Err(serde::de::Error::custom("Recovery payload exceeds limit")); }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CalendarRestoreKind { Event, Task }

/// A bounded effect description for human approval, containing no raw data.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CalendarRestoreSummary {
    pub kind: CalendarRestoreKind,
    pub original_sha256: String,
    pub projected_sha256: String,
    /// Bind human confirmation to this exact effect and source/destination.
    pub approval_sha256: String,
    pub effects: &'static str,
    pub components: Vec<CalendarRestorePreview>,
    pub removed_scheduling_method: bool,
    pub removed_scheduling_participants: bool,
    pub removed_alarms: bool,
    pub retained_timezone_definitions: bool,
    /// Untrusted, bounded jCal text; not an independently validated time-zone rule.
    pub timezone_definitions: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CalendarRestorePreview {
    pub kind: String,
    pub title: Option<String>,
    pub uid: String,
    pub start: Option<CalendarPreviewDate>,
    pub end: Option<CalendarPreviewDate>,
    pub due: Option<CalendarPreviewDate>,
    pub recurrence_id: Option<CalendarPreviewDate>,
    pub recurrence_rule: Option<String>,
    pub additional_dates: Vec<CalendarPreviewDate>,
    pub excluded_dates: Vec<CalendarPreviewDate>,
    pub relations: Vec<CalendarPreviewRelation>,
    pub has_attachments: bool,
    pub other_properties: Vec<CalendarPreviewProperty>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CalendarPreviewProperty {
    pub name: String,
    pub value: String,
    pub parameters: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CalendarPreviewRelation {
    pub uid: String,
    pub relation_type: String,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CalendarPreviewDate {
    pub value: String,
    pub tzid: Option<String>,
    pub value_type: Option<String>,
}

fn recovery_preview_multiline(value: &Value, max: usize) -> Result<String, String> {
    let value = value.as_str().ok_or("Invalid recovery preview")?;
    if value.len() > max || value.chars().any(|c| c.is_control() && c != '\r' && c != '\n') {
        return Err("Recovery preview cannot be displayed safely".into());
    }
    Ok(value.replace("\r\n", "\n").replace('\r', "\n"))
}

fn calendar_preview(original: &[u8], projected: &[u8]) -> Result<(Vec<CalendarRestorePreview>, bool, bool, bool, Vec<String>), String> {
    fn text(value: &Value, max: usize) -> Result<String, String> {
        let value = value.as_str().ok_or("Invalid recovery preview")?;
        if value.len() > max || value.chars().any(char::is_control) { return Err("Recovery preview cannot be displayed safely".into()); }
        Ok(value.into())
    }
    fn values<'a>(object: &'a Value, key: &str) -> Result<&'a [Value], String> {
        match object.get("properties").and_then(|v| v.get(key)) {
            None => Ok(&[]),
            Some(value) => value.as_array().map(Vec::as_slice).ok_or_else(|| "Invalid recovery preview".into()),
        }
    }
    fn single(object: &Value, key: &str, max: usize) -> Result<Option<String>, String> {
        let items = values(object,key)?;
        if items.len() > 1 { return Err("Ambiguous recovery preview property".into()); }
        items.first().map(|item| text(&item["value"],max)).transpose()
    }
    fn date_entry(item: &Value) -> Result<CalendarPreviewDate, String> {
        Ok(CalendarPreviewDate {
            value:text(&item["value"],4096)?,
            tzid:item.pointer("/parameters/TZID").map(|v| text(v,256)).transpose()?,
            value_type:item.pointer("/parameters/VALUE").map(|v| text(v,128)).transpose()?,
        })
    }
    fn date(object: &Value, key: &str) -> Result<Option<CalendarPreviewDate>, String> {
        let items = values(object,key)?;
        if items.len() > 1 { return Err("Ambiguous recovery preview date".into()); }
        items.first().map(date_entry).transpose()
    }
    fn dates(object: &Value, key: &str) -> Result<Vec<CalendarPreviewDate>, String> {
        let items = values(object,key)?;
        if items.len() > 32 { return Err("Recovery preview exceeds recurrence limit".into()); }
        items.iter().map(date_entry).collect()
    }
    fn safe_timezone(value: &Value, depth: usize, count: &mut usize) -> Result<(), String> {
        *count += 1;
        if depth > 16 || *count > 4096 { return Err("Recovery time-zone preview exceeds limit".into()); }
        match value {
            Value::String(item) => {
                if item.len() > 4096 || item.chars().any(char::is_control) || item.to_ascii_lowercase().starts_with("data:") {
                    return Err("Recovery time-zone value cannot be displayed safely".into());
                }
            },
            Value::Array(items) => {
                if items.first().and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case("attach")) {
                    return Err("Recovery time zone contains an undisplayed attachment".into());
                }
                if items.get(2).and_then(Value::as_str).is_some_and(|kind| kind.eq_ignore_ascii_case("binary")) {
                    return Err("Recovery time zone contains undisplayed binary data".into());
                }
                for item in items { safe_timezone(item,depth + 1,count)?; }
            },
            Value::Object(items) => {
                for (key,item) in items {
                    if key.len() > 128 || key.chars().any(char::is_control) { return Err("Invalid recovery time-zone parameter".into()); }
                    if (key.eq_ignore_ascii_case("encoding") && item.as_str().is_some_and(|v| v.eq_ignore_ascii_case("base64") || v.eq_ignore_ascii_case("b"))) ||
                       (key.eq_ignore_ascii_case("value") && item.as_str().is_some_and(|v| v.eq_ignore_ascii_case("binary"))) {
                        return Err("Recovery time zone contains undisplayed binary data".into());
                    }
                    safe_timezone(item,depth + 1,count)?;
                }
            },
            Value::Null | Value::Bool(_) | Value::Number(_) => {},
        }
        Ok(())
    }
    let output = isyncyou_connectors::dav::projection::calendar(projected)?;
    let source = isyncyou_connectors::dav::projection::calendar(original)?;
    let top = output.pointer("/jcal/1").and_then(Value::as_array).ok_or("Invalid recovery preview")?;
    if top.iter().any(|property| property.get(0).and_then(Value::as_str).is_none_or(|name| {
        !["version", "prodid", "calscale"].iter().any(|allowed| name.eq_ignore_ascii_case(allowed))
    })) { return Err("Recovery calendar contains undisplayed calendar-level properties".into()); }
    let mut timezones = Vec::new();
    for component in output.pointer("/jcal/2").and_then(Value::as_array).ok_or("Invalid recovery preview")? {
        if component.get(0).and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case("vtimezone")) {
            if timezones.len() >= 32 { return Err("Recovery preview exceeds time-zone limit".into()); }
            let mut count = 0;
            safe_timezone(component,0,&mut count)?;
            timezones.push(serde_json::to_string(component).map_err(|_| "Invalid recovery time-zone preview")?);
        }
    }
    let components = output.get("components").and_then(Value::as_array).ok_or("Invalid recovery preview")?;
    let source_components = source.get("components").and_then(Value::as_array).ok_or("Invalid recovery preview")?;
    if components.is_empty() || components.len() > 64 { return Err("Recovery preview exceeds component limit".into()); }
    let mut previews = Vec::with_capacity(components.len());
    for component in components {
        let related = values(component,"RELATED-TO")?;
        if related.len() > 32 { return Err("Recovery preview exceeds relation limit".into()); }
        let properties = component.get("properties").and_then(Value::as_object).ok_or("Invalid recovery preview")?;
        let mut other_properties = Vec::new();
        for (name, property_values) in properties {
            if ["UID","SUMMARY","DTSTART","DTEND","DUE","RECURRENCE-ID","RRULE","RDATE","EXDATE","RELATED-TO"]
                .iter().any(|known| name.eq_ignore_ascii_case(known)) { continue; }
            if name.is_empty() || name.len() > 128 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err("Recovery property cannot be displayed safely".into());
            }
            let items = property_values.as_array().ok_or("Invalid recovery preview")?;
            if items.len() > 32 || other_properties.len() + items.len() > 128 { return Err("Recovery preview exceeds property limit".into()); }
            for item in items {
                let params = item.get("parameters").and_then(Value::as_object).ok_or("Invalid recovery preview")?;
                if params.len() > 32 { return Err("Recovery preview exceeds parameter limit".into()); }
                let mut parameters = std::collections::BTreeMap::new();
                for (key, value) in params {
                    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                        return Err("Recovery parameter cannot be displayed safely".into());
                    }
                    let value = text(value,1024)?;
                    if (key.eq_ignore_ascii_case("VALUE") && value.eq_ignore_ascii_case("BINARY")) ||
                        (key.eq_ignore_ascii_case("ENCODING") && (value.eq_ignore_ascii_case("BASE64") || value.eq_ignore_ascii_case("B"))) {
                        return Err("Recovery property contains undisplayed binary data".into());
                    }
                    parameters.insert(key.clone(),value);
                }
                let value = if name.eq_ignore_ascii_case("DESCRIPTION") || name.eq_ignore_ascii_case("LOCATION") {
                    recovery_preview_multiline(&item["value"],4096)?
                } else { text(&item["value"],4096)? };
                if name.eq_ignore_ascii_case("ATTACH") && value.trim_start().to_ascii_lowercase().starts_with("data:") {
                    return Err("Recovery attachment contains undisplayed embedded data".into());
                }
                other_properties.push(CalendarPreviewProperty {name:name.clone(),value,parameters});
            }
        }
        previews.push(CalendarRestorePreview {
            kind:text(&component["kind"],32)?, title:single(component,"SUMMARY",1024)?,
            uid:text(&component["uid"],1024)?, start:date(component,"DTSTART")?, end:date(component,"DTEND")?,
            due:date(component,"DUE")?, recurrence_id:date(component,"RECURRENCE-ID")?,
            recurrence_rule:single(component,"RRULE",1024)?,
            additional_dates:dates(component,"RDATE")?,
            excluded_dates:dates(component,"EXDATE")?,
            has_attachments:!values(component,"ATTACH")?.is_empty(),
            other_properties,
            relations:related.iter().map(|item| Ok(CalendarPreviewRelation {
                uid:text(&item["value"],1024)?,
                relation_type:item.pointer("/parameters/RELTYPE").map(|v| text(v,128)).transpose()?.unwrap_or_else(|| "PARENT".into()),
            })).collect::<Result<_,String>>()?,
        });
    }
    if serde_json::to_vec(&(&previews,&timezones)).map_err(|_| "Invalid recovery preview")?.len() > 65536 {
        return Err("Recovery preview exceeds display capacity".into());
    }
    let participants = source_components.iter().any(|c| {
        c.get("properties").is_some_and(|p| p.get("ORGANIZER").is_some() || p.get("ATTENDEE").is_some())
    });
    let method = source.pointer("/jcal/1").and_then(Value::as_array).ok_or("Invalid recovery preview")?.iter().any(|property| {
        property.get(0).and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case("method"))
    });
    let alarms = isyncyou_connectors::dav::edit::resource_has_alarms(original)?;
    Ok((previews, method, participants, alarms, timezones))
}

/// Validate the exact source and calculate the effect the user must approve.
/// The caller authenticates the catalog binding and keeps the returned intent
/// and mapping unchanged through confirmation and any later reconciliation.
pub fn prepare_calendar_restore(op: &ApprovedCalendarRestore) -> Result<CalendarRestoreSummary, String> {
    use isyncyou_connectors::dav::edit::{resource_index, ResourceIndex};
    let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if op.operation_id.is_empty() || op.operation_id.len() > 128 ||
        !op.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&op.account_id, &op.origin, &op.entry, &op.collection].iter().any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)) ||
        [&op.source_generation, &op.source_object, &op.source_version, &op.original_sha256].iter().any(|s| !hash(s)) ||
        op.original.len() > 16 * 1024 * 1024 || format!("{:x}", Sha256::digest(&op.original)) != op.original_sha256 {
        return Err("Invalid calendar recovery intent".into());
    }
    let actual = match resource_index(&op.original)? {
        ResourceIndex::Event { .. } => CalendarRestoreKind::Event,
        ResourceIndex::Task(_) => CalendarRestoreKind::Task,
    };
    if actual != op.kind { return Err("Recovery object type mismatch".into()); }
    let projected = crate::dav_live::prepare_recovered_calendar(&op.original, &op.uid_map)?;
    let (components, removed_scheduling_method, removed_scheduling_participants, removed_alarms, timezone_definitions) = calendar_preview(&op.original, &projected)?;
    let retained_timezone_definitions = !timezone_definitions.is_empty();
    let projected_sha256 = format!("{:x}", Sha256::digest(&projected));
    let ordered_map: std::collections::BTreeMap<_, _> = op.uid_map.iter().collect();
    let approval = json!({"schema":"calendar-recovery-approval-v1", "operation_id":op.operation_id,
        "account_id":op.account_id, "origin":op.origin, "entry":op.entry, "collection":op.collection,
        "generation":op.source_generation, "object":op.source_object, "version":op.source_version,
        "original_sha256":op.original_sha256, "projected_sha256":projected_sha256,
        "uid_map":ordered_map, "kind":op.kind});
    let approval_sha256 = format!("{:x}", Sha256::digest(approval.to_string().as_bytes()));
    Ok(CalendarRestoreSummary { kind: actual, original_sha256: op.original_sha256.clone(),
        projected_sha256, approval_sha256,
        effects: "Create a personal recovered copy with new identity; scheduling participants and alarms are removed.",
        components, removed_scheduling_method, removed_scheduling_participants, removed_alarms, retained_timezone_definitions, timezone_definitions })
}

/// Execute only after confirmation of the exact prepared intent. Account and
/// origin are rechecked; the lower layer journals before PUT and reconciles an
/// unknown outcome by readback without issuing a second PUT.
pub fn execute_calendar_restore_with_store(cfg: &Config, op: &ApprovedCalendarRestore, approved_sha256: &str, credentials: &dyn ProviderCredentialStore)
    -> Result<crate::dav_live::RecoveryImportOutcome, String> {
    if prepare_calendar_restore(op)?.approval_sha256 != approved_sha256 {
        return Err("Calendar recovery approval does not match intent".into());
    }
    match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &op.account_id, credentials, |token| {
        execute_calendar_restore_with_token(cfg, op, approved_sha256, &store, token)
    })
}

fn execute_calendar_restore_with_token(cfg: &Config, op: &ApprovedCalendarRestore, approved_sha256: &str, store: &Store, token: &str)
    -> Result<crate::dav_live::RecoveryImportOutcome, String> {
    if prepare_calendar_restore(op)?.approval_sha256 != approved_sha256 {
        return Err("Calendar recovery approval does not match intent".into());
    }
    match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    crate::dav_live::import_recovered_with_token(cfg, &op.account_id, &op.origin, store, &op.entry, &op.collection, "calendar",
        &op.operation_id, &op.source_generation, &op.source_object, &op.source_version,
        &op.original, &op.original_sha256, &op.uid_map, None, token)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedMailRestore {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub mailbox_id: String,
    pub source_generation: String,
    pub source_object: String,
    pub source_version: String,
    pub original_sha256: String,
    pub original_mime: Vec<u8>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedBoundMailRestore {
    pub intent: ApprovedMailRestore,
    pub protected_sha256: String,
    pub encoding: String,
}

#[derive(serde::Serialize)]
pub struct MailRestoreSummary {
    pub approval_sha256: String,
    pub mime_sha256: String,
    pub mime_size: usize,
    /// Untrusted bounded headers and an explicit notice about omitted parts;
    /// only for the protected human review route.
    pub review_text: String,
}

fn recovery_mail_folder_operation(operation_id: &str) -> String {
    format!("recovery-folder-{:x}", Sha256::digest(operation_id.as_bytes()))
}

fn review_mail_headers(bytes: &[u8]) -> String {
    // Metadata is an untrusted preview only. The complete original MIME is
    // imported unchanged and may contain attachments or parts not shown here.
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n")
        .or_else(|| bytes.windows(2).position(|w| w == b"\n\n"));
    let Some(end) = end.filter(|n| *n <= 16 * 1024) else {
        return "Message headers unavailable for preview; original content and attachments are not shown.".into();
    };
    let header = String::from_utf8_lossy(&bytes[..end]);
    let text: String = header.chars().map(|c| if c.is_control() && !matches!(c, '\r' | '\n' | '\t') { '\u{fffd}' } else { c }).collect();
    format!("Untrusted original headers (encoded values may not be decoded):\n{text}\n\nThe message body, attachments and other MIME parts are not displayed; the complete original message will be imported without sending it.")
}

pub fn prepare_bound_mail_restore(cfg: &Config, selection: &RecoverySelection,
    signer: &dyn RecoveryDeviceSigner, credentials: &dyn ProviderCredentialStore,
    mailbox_id: &str) -> Result<(ApprovedBoundMailRestore, MailRestoreSummary), String> {
    let payload = read_bound_recovery_result(cfg, selection, signer, credentials)?;
    mail_restore_from_payload(selection, payload, mailbox_id)
}

fn mail_restore_from_payload(selection: &RecoverySelection, payload: RecoveredPayload,
    mailbox_id: &str) -> Result<(ApprovedBoundMailRestore, MailRestoreSummary), String> {
    if payload.kind != "mail" || format!("{:x}", Sha256::digest(&payload.bytes)) != payload.sha256 {
        return Err("Recovery result is not verified mail".into());
    }
    let mime = match payload.encoding.as_str() {
        "mime" => payload.bytes.as_slice(),
        "isync-mime-1" => {
            const MAGIC: &[u8] = b"ISYNC-MIME-1\n";
            if !payload.bytes.starts_with(MAGIC) || payload.bytes.len() < MAGIC.len() + 4 { return Err("Invalid recovered mail envelope".into()); }
            let start = MAGIC.len() + 4;
            let size = u32::from_be_bytes(payload.bytes[MAGIC.len()..start].try_into().map_err(|_| "Invalid mail metadata")?) as usize;
            let end = start.checked_add(size).filter(|end| *end <= payload.bytes.len()).ok_or("Invalid mail metadata")?;
            let metadata: Value = serde_json::from_slice(&payload.bytes[start..end]).map_err(|_| "Invalid mail metadata")?;
            if !metadata.is_object() { return Err("Invalid mail metadata".into()); }
            &payload.bytes[end..]
        },
        _ => return Err("Unknown recovered mail encoding".into()),
    };
    if mime.len() > 64 * 1024 * 1024 { return Err("Recovered mail is too large".into()); }
    let op = ApprovedBoundMailRestore { intent: ApprovedMailRestore {
        operation_id:selection.operation_id.clone(), account_id:selection.account_id.clone(), origin:selection.origin.clone(),
        mailbox_id:mailbox_id.into(), source_generation:selection.generation.clone(), source_object:selection.object.clone(),
        source_version:selection.version.clone(), original_sha256:format!("{:x}", Sha256::digest(mime)), original_mime:mime.to_vec(),
    }, protected_sha256:payload.sha256, encoding:payload.encoding };
    let summary = prepare_mail_restore(&op)?;
    Ok((op, summary))
}

pub fn prepare_mail_restore(op: &ApprovedBoundMailRestore) -> Result<MailRestoreSummary, String> {
    let m = &op.intent;
    let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if m.operation_id.is_empty() || m.operation_id.len() > 128 ||
        !m.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&m.account_id, &m.origin, &m.mailbox_id].iter().any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)) || m.mailbox_id != "Recovered" ||
        [&m.source_generation, &m.source_object, &m.source_version, &m.original_sha256, &op.protected_sha256].iter().any(|s| !hash(s)) ||
        !matches!(op.encoding.as_str(), "mime" | "isync-mime-1") || m.original_mime.len() > 64 * 1024 * 1024 ||
        format!("{:x}", Sha256::digest(&m.original_mime)) != m.original_sha256 {
        return Err("Invalid mail recovery intent".into());
    }
    let review_text = review_mail_headers(&m.original_mime);
    let approval = json!({"schema":"mail-recovery-approval-v1", "operation_id":m.operation_id,
        "account_id":m.account_id, "origin":m.origin, "target":"Recovered",
        "folder_operation_id":recovery_mail_folder_operation(&m.operation_id),
        "generation":m.source_generation, "object":m.source_object, "version":m.source_version,
        "protected_sha256":op.protected_sha256, "encoding":op.encoding, "mime_sha256":m.original_sha256,
        "mime_size":m.original_mime.len()});
    Ok(MailRestoreSummary { approval_sha256:format!("{:x}", Sha256::digest(approval.to_string().as_bytes())),
        mime_sha256:m.original_sha256.clone(), mime_size:m.original_mime.len(), review_text })
}

pub fn execute_bound_mail_restore_with_store(cfg: &Config, op: &ApprovedBoundMailRestore, approved_sha256: &str,
    credentials: &dyn ProviderCredentialStore) -> Result<RestoreOutcome, String> {
    if prepare_mail_restore(op)?.approval_sha256 != approved_sha256 { return Err("Mail recovery approval does not match intent".into()); }
    let folder = match ensure_recovered_folder_with_store(cfg, &op.intent.account_id, &op.intent.origin,
        &recovery_mail_folder_operation(&op.intent.operation_id), credentials)? {
        FolderOutcome::Ready(id) => id,
        FolderOutcome::Rejected => return Ok(RestoreOutcome::Rejected),
        FolderOutcome::Unknown => return Ok(RestoreOutcome::Unknown),
    };
    let mut intent = op.intent.clone();
    intent.mailbox_id = folder;
    execute_with_store(cfg, &intent, credentials)
}

/// A contact copy is bound to the protected result and to its personal target.
/// Keep this intent in the encrypted host confirmation store.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedContactRestore {
    pub operation_id: String,
    pub account_id: String,
    pub origin: String,
    pub entry: String,
    pub collection: String,
    pub source_generation: String,
    pub source_object: String,
    pub source_version: String,
    pub original_sha256: String,
    #[serde(with = "recovery_payload_base64")]
    pub original: Vec<u8>,
    pub new_uid: String,
}

#[derive(serde::Serialize)]
pub struct ContactRestoreSummary {
    pub approval_sha256: String,
    pub projected_sha256: String,
    pub projected_size: usize,
    pub preview_truncated: bool,
    /// Untrusted text for the protected human review route only.
    pub projected_vcard: String,
}

pub fn prepare_bound_contact_restore(cfg: &Config, selection: &RecoverySelection,
    signer: &dyn RecoveryDeviceSigner, credentials: &dyn ProviderCredentialStore,
    entry: &str, collection: &str) -> Result<(ApprovedContactRestore, ContactRestoreSummary), String> {
    use ring::rand::SecureRandom;
    let payload = read_bound_recovery_result(cfg, selection, signer, credentials)?;
    let mut random = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut random).map_err(|_| "Recovery identity unavailable")?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    contact_restore_from_payload(selection, payload, entry, collection, &format!("{suffix}@recovered.invalid"))
}

fn contact_restore_from_payload(selection: &RecoverySelection, payload: RecoveredPayload, entry: &str,
    collection: &str, new_uid: &str) -> Result<(ApprovedContactRestore, ContactRestoreSummary), String> {
    if payload.kind != "contact" || payload.encoding != "raw" { return Err("Recovery result is not a contact".into()); }
    let op = ApprovedContactRestore { operation_id: selection.operation_id.clone(), account_id: selection.account_id.clone(),
        origin: selection.origin.clone(), entry: entry.into(), collection: collection.into(),
        source_generation: selection.generation.clone(), source_object: selection.object.clone(),
        source_version: selection.version.clone(), original_sha256: payload.sha256,
        original: payload.bytes, new_uid: new_uid.into() };
    let summary = prepare_contact_restore(&op)?;
    Ok((op, summary))
}

fn staged_item(selection: &RecoverySelection, status: &BoundRecoveryStatus, signer: &dyn RecoveryDeviceSigner,
    bytes: Vec<u8>, kind: &str) -> Result<RecoveredPayload, String> {
    if status.selection_sha256 != recovery_selection_digest(selection, signer.device_id()) || status.state != "available" ||
        status.kind != kind || bytes.len() as u64 != status.size || bytes.len() > 100 * 1024 * 1024 ||
        format!("{:x}", Sha256::digest(&bytes)) != status.sha256 {
        return Err("Recovery staged payload mismatch".into());
    }
    Ok(RecoveredPayload { bytes, kind:status.kind.clone(), encoding:status.encoding.clone(), sha256:status.sha256.clone() })
}

pub fn prepare_staged_mail_restore(selection: &RecoverySelection, status: &BoundRecoveryStatus,
    signer: &dyn RecoveryDeviceSigner, bytes: Vec<u8>, mailbox_id: &str)
    -> Result<(ApprovedBoundMailRestore, MailRestoreSummary), String> {
    mail_restore_from_payload(selection, staged_item(selection, status, signer, bytes, "mail")?, mailbox_id)
}

/// The host must persist new_uid with the staged manifest before approval and
/// reuse it on every reconstruction, including reconciliation.
pub fn prepare_staged_contact_restore(selection: &RecoverySelection, status: &BoundRecoveryStatus,
    signer: &dyn RecoveryDeviceSigner, bytes: Vec<u8>, entry: &str, collection: &str, new_uid: &str)
    -> Result<(ApprovedContactRestore, ContactRestoreSummary), String> {
    contact_restore_from_payload(selection, staged_item(selection, status, signer, bytes, "contact")?, entry, collection, new_uid)
}

pub fn prepare_contact_restore(op: &ApprovedContactRestore) -> Result<ContactRestoreSummary, String> {
    let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if op.operation_id.is_empty() || op.operation_id.len() > 128 ||
        !op.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&op.account_id, &op.origin, &op.entry, &op.collection, &op.new_uid].iter().any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)) ||
        [&op.source_generation, &op.source_object, &op.source_version, &op.original_sha256].iter().any(|s| !hash(s)) ||
        op.original.len() > 16 * 1024 * 1024 || format!("{:x}", Sha256::digest(&op.original)) != op.original_sha256 {
        return Err("Invalid contact recovery intent".into());
    }
    let projected = isyncyou_connectors::dav::edit::recovered_contact(&op.original, &op.new_uid)?;
    // Retain the complete card, including embedded photos. Preview only a
    // bounded untrusted prefix and explicitly disclose any omitted bytes.
    let shown = projected.len().min(64 * 1024);
    let preview_truncated = shown < projected.len();
    let text: String = String::from_utf8_lossy(&projected[..shown]).chars()
        .map(|c| if c.is_control() && !matches!(c, '\r' | '\n' | '\t') { '\u{fffd}' } else { c }).collect();
    let projected_sha256 = format!("{:x}", Sha256::digest(&projected));
    let approval = json!({"schema":"contact-recovery-approval-v1", "operation_id":op.operation_id,
        "account_id":op.account_id, "origin":op.origin, "entry":op.entry, "collection":op.collection,
        "generation":op.source_generation, "object":op.source_object, "version":op.source_version,
        "original_sha256":op.original_sha256, "projected_sha256":projected_sha256, "new_uid":op.new_uid});
    let approval_sha256 = format!("{:x}", Sha256::digest(approval.to_string().as_bytes()));
    Ok(ContactRestoreSummary { approval_sha256, projected_sha256, projected_size:projected.len(),
        preview_truncated, projected_vcard: text })
}

pub fn execute_contact_restore_with_store(cfg: &Config, op: &ApprovedContactRestore, approved_sha256: &str,
    credentials: &dyn ProviderCredentialStore) -> Result<crate::dav_live::RecoveryImportOutcome, String> {
    if prepare_contact_restore(op)?.approval_sha256 != approved_sha256 { return Err("Contact recovery approval does not match intent".into()); }
    match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    crate::provider_auth::with_locked_provider_access_token(cfg, &op.account_id, credentials, |token| {
        crate::dav_live::import_recovered_with_token(cfg, &op.account_id, &op.origin, &store, &op.entry, &op.collection,
            "contacts", &op.operation_id, &op.source_generation, &op.source_object, &op.source_version,
            &op.original, &op.original_sha256, &HashMap::new(), Some(&op.new_uid), token)
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Verified(String),
    /// Existing native folder is absent or cannot safely be identified.
    RecoveredFolderUnavailable,
    Rejected,
    /// A remote result is ambiguous; never re-upload or re-import blindly.
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FolderOutcome { Ready(String), Rejected, Unknown }

trait Transport {
    fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError>;
    fn create_recovered(&self, operation: &str, state: &str) -> Result<Option<String>, JmapError>;
    fn recovered_folder(&self, id: &str) -> Result<bool, JmapError>;
    fn upload(&self, mime: &[u8]) -> Result<String, JmapError>;
    fn import(&self, operation: &str, blob: &str, folder: &str) -> Result<Option<String>, JmapError>;
    fn readback(&self, id: &str, folder: &str, expected_sha: &str) -> Result<bool, JmapError>;
}

// An explicitly null ACL is the native representation of an unshared
// mailbox; an omitted property is not evidence that it is private.
fn is_unshared(value: &Value) -> bool {
    value.get("shareWith").is_some_and(|acl| {
        acl.is_null() || acl.as_object().is_some_and(|entries| entries.is_empty())
    })
}

impl Transport for JmapClient {
    fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> {
        let mut position = 0;
        let mut query_state = None::<String>;
        let mut ids = Vec::new();
        loop {
            let page = self.query_mailboxes(position, 100)?;
            let state = page.get("queryState").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
            if query_state.as_deref().is_some_and(|old| old != state) { return Err(JmapError::InvalidResponse); }
            query_state.get_or_insert_with(|| state.to_owned());
            let page_ids = page.get("ids").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
            let total = page.get("total").and_then(Value::as_u64).ok_or(JmapError::InvalidResponse)? as usize;
            if total > 1000 || position + page_ids.len() > total { return Err(JmapError::InvalidResponse); }
            for id in page_ids { ids.push(id.as_str().filter(|x| !x.is_empty()).ok_or(JmapError::InvalidResponse)?.to_owned()); }
            position += page_ids.len();
            if position == total { break; }
            if page_ids.is_empty() { return Err(JmapError::InvalidResponse); }
        }
        let mut unique = std::collections::HashSet::new();
        if ids.iter().any(|id| !unique.insert(id)) { return Err(JmapError::InvalidResponse); }
        let mut state = None::<String>;
        let mut found = Vec::new();
        for chunk in ids.chunks(self.max_objects_in_get()) {
            let properties: Vec<String> = ["id", "name", "role", "myRights", "shareWith"].into_iter().map(str::to_owned).collect();
            let response = self.get("Mailbox", Some(chunk), Some(&properties))?;
            let page_state = response.get("state").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
            if state.as_deref().is_some_and(|old| old != page_state) { return Err(JmapError::InvalidResponse); }
            state.get_or_insert_with(|| page_state.to_owned());
            let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
            if list.len() != chunk.len() || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) { return Err(JmapError::InvalidResponse); }
            let mut page_ids = std::collections::HashSet::new();
            for item in list {
                let id = item.get("id").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
                if !chunk.iter().any(|x| x == id) || !page_ids.insert(id) { return Err(JmapError::InvalidResponse); }
                if item.get("name").and_then(Value::as_str) == Some("Recovered") {
                    if !item.get("role").is_some_and(Value::is_null) ||
                       item.pointer("/myRights/mayAddItems").and_then(Value::as_bool) != Some(true) ||
                       !is_unshared(item) {
                        return Err(JmapError::InvalidResponse);
                    }
                    found.push(id.to_owned());
                }
            }
        }
        let final_state = self.get("Mailbox", Some(&[]), None)?;
        let final_state = final_state.get("state").and_then(Value::as_str).ok_or(JmapError::InvalidResponse)?;
        if state.as_deref().is_some_and(|old| old != final_state) { return Err(JmapError::InvalidResponse); }
        Ok((final_state.to_owned(), found))
    }
    fn create_recovered(&self, operation: &str, state: &str) -> Result<Option<String>, JmapError> {
        match self.create_mailbox(operation, "Recovered", state)? {
            MailboxCreateResult::Created { id } => Ok(Some(id)),
            MailboxCreateResult::Rejected { .. } => Ok(None),
        }
    }
    fn recovered_folder(&self, id: &str) -> Result<bool, JmapError> {
        let properties: Vec<String> = ["id", "name", "role", "myRights", "shareWith"].into_iter().map(str::to_owned).collect();
        let response = self.get("Mailbox", Some(&[id.to_owned()]), Some(&properties))?;
        let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
        if list.len() != 1 || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) { return Ok(false); }
        let item = &list[0];
        Ok(item.get("id").and_then(Value::as_str) == Some(id)
            && item.get("name").and_then(Value::as_str) == Some("Recovered")
            && item.get("role").is_some_and(Value::is_null)
            && item.pointer("/myRights/mayAddItems").and_then(Value::as_bool) == Some(true)
            && is_unshared(item))
    }
    fn upload(&self, mime: &[u8]) -> Result<String, JmapError> {
        self.upload(mime.to_vec(), "message/rfc822")?.get("blobId").and_then(Value::as_str)
            .filter(|s| !s.is_empty()).map(str::to_owned).ok_or(JmapError::OutcomeUnknown)
    }
    fn import(&self, operation: &str, blob: &str, folder: &str) -> Result<Option<String>, JmapError> {
        let mut memberships = serde_json::Map::new();
        memberships.insert(folder.to_owned(), Value::Bool(true));
        let response = self.import_email(operation, blob, Value::Object(memberships))?;
        let created = response.get("created").and_then(|x| x.get(operation));
        let rejected = response.get("notCreated").and_then(|x| x.get(operation));
        match (created, rejected) {
            (Some(item), None) => item.get("id").and_then(Value::as_str).filter(|x| !x.is_empty())
                .map(|id| Some(id.to_owned())).ok_or(JmapError::OutcomeUnknown),
            (None, Some(item)) if item.get("type").and_then(Value::as_str).is_some() => Ok(None),
            _ => Err(JmapError::OutcomeUnknown),
        }
    }
    fn readback(&self, id: &str, folder: &str, expected_sha: &str) -> Result<bool, JmapError> {
        let response = self.get("Email", Some(&[id.to_owned()]), None)?;
        let list = response.get("list").and_then(Value::as_array).ok_or(JmapError::InvalidResponse)?;
        if list.len() != 1 || response.get("notFound").and_then(Value::as_array).is_none_or(|x| !x.is_empty()) ||
            list[0].get("id").and_then(Value::as_str) != Some(id) ||
            list[0].get("mailboxIds").and_then(|v| v.get(folder)).and_then(Value::as_bool) != Some(true) {
            return Ok(false);
        }
        let blob = list[0].get("blobId").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or(JmapError::InvalidResponse)?;
        let mime = self.download(blob, "recovered.eml", "message/rfc822")?;
        Ok(format!("{:x}", Sha256::digest(&mime)) == expected_sha)
    }
}

fn run<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope, op: &ApprovedMailRestore) -> Result<RestoreOutcome, String> {
    let is_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if op.operation_id.is_empty() || op.operation_id.len() > 128 ||
        !op.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') ||
        [&op.mailbox_id, &op.source_generation, &op.source_object, &op.source_version].iter().any(|s| s.is_empty() || s.len() > 512) ||
        !is_hash(&op.source_generation) || !is_hash(&op.source_object) || !is_hash(&op.source_version) ||
        op.original_mime.len() > 64 * 1024 * 1024 ||
        format!("{:x}", Sha256::digest(&op.original_mime)) != op.original_sha256 {
        return Err("Invalid recovered mail intent".into());
    }
    // The destination is rechecked using the current native grant, never an
    // arbitrary mailbox or a client-supplied server path.
    match transport.recovered_folder(&op.mailbox_id) {
        Ok(true) => {}, Ok(false) => return Ok(RestoreOutcome::RecoveredFolderUnavailable),
        Err(_) => return Ok(RestoreOutcome::Unknown),
    }
    let intent = serde_json::to_string(&json!({"kind":"jmap-mail-recovery","account":scope.account_id,
        "origin":scope.origin,"native_account":scope.collection,"mailbox":op.mailbox_id,
        "source_generation":op.source_generation,"source_object":op.source_object,"source_version":op.source_version,
        "sha256":op.original_sha256,"mime_base64":STANDARD.encode(&op.original_mime)}))
        .map_err(|_| "Invalid restore intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if let Some((previous, state, id)) = store.provider_mutation(scope, &op.operation_id).map_err(|e| e.to_string())? {
        if previous != digest { return Err("Recovery operation does not match original intent".into()); }
        if state == "confirmed" { return Ok(RestoreOutcome::Verified(id.ok_or("Missing native ID")?)); }
        if state == "rejected" { return Ok(RestoreOutcome::Rejected); }
        // A known native ID permits read-only reconciliation; no resend.
        if let Some(id) = id {
            return finish_readback(store, transport, scope, op, &id);
        }
        return Ok(RestoreOutcome::Unknown);
    }
    if !store.claim_provider_mutation(scope, &op.operation_id, "jmap-mail-recovery", &intent, &digest)
        .map_err(|e| e.to_string())? { return Ok(RestoreOutcome::Unknown); }
    let blob = match transport.upload(&op.original_mime) { Ok(blob) => blob, Err(_) => return Ok(RestoreOutcome::Unknown) };
    // ACL changes during a potentially long upload must be observed before the
    // import. A server-side ACL race still requires native qualification.
    if !matches!(transport.recovered_folder(&op.mailbox_id), Ok(true)) { return Ok(RestoreOutcome::Unknown); }
    let id = match transport.import(&op.operation_id, &blob, &op.mailbox_id) {
        Ok(Some(id)) => id,
        Ok(None) => { return if store.finish_provider_mutation(scope, &op.operation_id, None).unwrap_or(false) {
            Ok(RestoreOutcome::Rejected) } else { Ok(RestoreOutcome::Unknown) }; },
        Err(_) => return Ok(RestoreOutcome::Unknown),
    };
    if !store.checkpoint_provider_mutation(scope, &op.operation_id, &digest, &id).unwrap_or(false) {
        return Ok(RestoreOutcome::Unknown);
    }
    finish_readback(store, transport, scope, op, &id)
}

fn finish_readback<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope,
                                  op: &ApprovedMailRestore, id: &str) -> Result<RestoreOutcome, String> {
    if !matches!(transport.recovered_folder(&op.mailbox_id), Ok(true)) { return Ok(RestoreOutcome::Unknown); }
    match transport.readback(id, &op.mailbox_id, &op.original_sha256) {
        Ok(true) if store.finish_provider_mutation(scope, &op.operation_id, Some(id)).unwrap_or(false) => Ok(RestoreOutcome::Verified(id.into())),
        _ => Ok(RestoreOutcome::Unknown),
    }
}

fn ensure_folder<T: Transport>(store: &Store, transport: &T, scope: &ProviderScope,
                               operation: &str) -> Result<FolderOutcome, String> {
    if operation.is_empty() || operation.len() > 110 ||
        !operation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("Invalid folder operation".into());
    }
    let folder_op = format!("{operation}-folder");
    // A prior ambiguous create is never resolved merely by matching the name:
    // another client may have created the same folder concurrently.
    if let Some((_digest, state, id)) = store.provider_mutation(scope, &folder_op).map_err(|e| e.to_string())? {
        if state == "rejected" { return Ok(FolderOutcome::Rejected); }
        let Some(id) = id else { return Ok(FolderOutcome::Unknown); };
        return match transport.recovered_folder(&id) {
            Ok(true) if state == "confirmed" => Ok(FolderOutcome::Ready(id)),
            Ok(true) if store.finish_provider_mutation(scope, &folder_op, Some(&id)).unwrap_or(false) => Ok(FolderOutcome::Ready(id)),
            _ => Ok(FolderOutcome::Unknown),
        };
    }
    let (state, existing) = match transport.discover_recovered() { Ok(value) => value, Err(_) => return Ok(FolderOutcome::Unknown) };
    match existing.as_slice() {
        [id] => return Ok(FolderOutcome::Ready(id.clone())),
        [] => {},
        _ => return Ok(FolderOutcome::Unknown),
    }
    let intent = serde_json::to_string(&json!({"kind":"jmap-recovered-folder","account":scope.account_id,
        "origin":scope.origin,"native_account":scope.collection,"name":"Recovered","if_in_state":state}))
        .map_err(|_| "Invalid folder intent")?;
    let digest = format!("{:x}", Sha256::digest(intent.as_bytes()));
    if !store.claim_provider_mutation(scope, &folder_op, "jmap-recovered-folder", &intent, &digest)
        .map_err(|e| e.to_string())? { return Ok(FolderOutcome::Unknown); }
    let id = match transport.create_recovered(&folder_op, &state) {
        Ok(Some(id)) => id,
        Ok(None) => return if store.finish_provider_mutation(scope, &folder_op, None).unwrap_or(false) {
            Ok(FolderOutcome::Rejected) } else { Ok(FolderOutcome::Unknown) },
        Err(_) => return Ok(FolderOutcome::Unknown),
    };
    if !store.checkpoint_provider_mutation(scope, &folder_op, &digest, &id).unwrap_or(false) {
        return Ok(FolderOutcome::Unknown);
    }
    match transport.recovered_folder(&id) {
        Ok(true) if store.finish_provider_mutation(scope, &folder_op, Some(&id)).unwrap_or(false) => Ok(FolderOutcome::Ready(id)),
        _ => Ok(FolderOutcome::Unknown),
    }
}

/// Obtain or create a personal Recovered folder after an effect-bound approval.
/// Unknown folder-creation outcomes require native reconciliation, not retry.
pub fn ensure_recovered_folder_with_store(cfg: &Config, account: &str, origin: &str,
                                          operation: &str, credentials: &dyn ProviderCredentialStore) -> Result<FolderOutcome, String> {
    ensure_folder_inner(cfg, account, origin, operation, Some(credentials))
}

pub fn ensure_recovered_folder(cfg: &Config, account: &str, origin: &str,
                               operation: &str) -> Result<FolderOutcome, String> {
    ensure_folder_inner(cfg, account, origin, operation, None)
}

fn ensure_folder_inner(cfg: &Config, account: &str, origin: &str, operation: &str,
                       credentials: Option<&dyn ProviderCredentialStore>) -> Result<FolderOutcome, String> {
    match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin: current, .. }) if current == origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let store = crate::provider_store::open(cfg, account)?;
    let run_locked = |credentials: &dyn ProviderCredentialStore| {
        crate::provider_auth::with_locked_provider_access_token(cfg, account, credentials, |token| {
            let client = JmapClient::discover(origin, token.to_owned()).map_err(|_| "Mail service unavailable")?;
            let scope = ProviderScope { account_id: account.into(), origin: origin.into(), service: "mail".into(),
                                        collection: client.account_id().to_owned() };
            ensure_folder(&store, &client, &scope, operation)
        })
    };
    match credentials {
        Some(credentials) => run_locked(credentials),
        None => crate::provider_auth::with_platform_credential_store(cfg, account, run_locked),
    }
}

/// Caller must supply an approval bound to every field and exact original bytes.
pub fn execute_with_store(cfg: &Config, op: &ApprovedMailRestore, credentials: &dyn ProviderCredentialStore) -> Result<RestoreOutcome, String> {
    execute_inner(cfg, op, Some(credentials))
}

pub fn execute(cfg: &Config, op: &ApprovedMailRestore) -> Result<RestoreOutcome, String> {
    execute_inner(cfg, op, None)
}

fn execute_inner(cfg: &Config, op: &ApprovedMailRestore, credentials: Option<&dyn ProviderCredentialStore>) -> Result<RestoreOutcome, String> {
    match cfg.provider_accounts.get(&op.account_id) {
        Some(ProviderConfig::SelfHosted { origin, .. }) if origin == &op.origin => {},
        _ => return Err("Account or origin changed after approval".into()),
    }
    let store = crate::provider_store::open(cfg, &op.account_id)?;
    let run_locked = |credentials: &dyn ProviderCredentialStore| {
        crate::provider_auth::with_locked_provider_access_token(cfg, &op.account_id, credentials, |token| {
            let client = JmapClient::discover(&op.origin, token.to_owned()).map_err(|_| "Mail service unavailable")?;
            let scope = ProviderScope { account_id: op.account_id.clone(), origin: op.origin.clone(),
                service: "mail".into(), collection: client.account_id().to_owned() };
            run(&store, &client, &scope, op)
        })
    };
    match credentials {
        Some(credentials) => run_locked(credentials),
        None => crate::provider_auth::with_platform_credential_store(cfg, &op.account_id, run_locked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn recovery_collection_approval_binds_identity_and_target() {
        let mut op = ApprovedRecoveryCollection { account_id:"account".into(), origin:"https://example.test/".into(),
            entry:"https://example.test/dav/".into(), service:"calendar".into(), home:"https://example.test/home/".into(),
            principal:"https://example.test/principal/".into(), target:"https://example.test/home/recovered/".into(),
            operation_id:"0123456789abcdef0123456789abcdef".into(), approval_sha256:String::new() };
        op.approval_sha256 = collection_approval(&op);
        let saved = serde_json::to_string(&op).unwrap();
        let restored: ApprovedRecoveryCollection = serde_json::from_str(&saved).unwrap();
        assert_eq!(collection_approval(&restored), op.approval_sha256);
        op.target.push_str("other/");
        assert_ne!(collection_approval(&op), restored.approval_sha256);
    }
    #[test]
    fn recovery_contact_approval_binds_target_and_full_copy() {
        use sha2::{Digest, Sha256};
        let raw = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:source\r\nFN:Fixture Person\r\nEND:VCARD\r\n".to_vec();
        let mut op = super::ApprovedContactRestore { operation_id:"operation-a".into(), account_id:"account".into(),
            origin:"https://example.test/".into(), entry:"https://example.test/dav/".into(), collection:"https://example.test/dav/recovered/".into(),
            source_generation:"a".repeat(64), source_object:"b".repeat(64), source_version:"c".repeat(64),
            original_sha256:format!("{:x}",Sha256::digest(&raw)), original:raw, new_uid:"new@recovered.invalid".into() };
        let first = super::prepare_contact_restore(&op).unwrap();
        assert!(first.projected_vcard.contains("FN:Fixture Person"));
        assert!(first.projected_vcard.contains("UID:new@recovered.invalid"));
        op.collection.push_str("other/");
        assert_ne!(first.approval_sha256, super::prepare_contact_restore(&op).unwrap().approval_sha256);
        op.original.push(b'x');
        assert!(super::prepare_contact_restore(&op).is_err());
    }
    #[test]
    fn recovery_contact_large_photo_is_retained_and_disclosed() {
        use sha2::{Digest, Sha256};
        let raw = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:old\r\nFN:Fixture\r\nPHOTO:data:image/png;base64,{}\r\nEND:VCARD\r\n", "A".repeat(70000)).into_bytes();
        let op = super::ApprovedContactRestore { operation_id:"photo-a".into(), account_id:"account".into(),
            origin:"https://example.test/".into(), entry:"https://example.test/dav/".into(), collection:"https://example.test/dav/recovered/".into(),
            source_generation:"a".repeat(64), source_object:"b".repeat(64), source_version:"c".repeat(64),
            original_sha256:format!("{:x}",Sha256::digest(&raw)), original:raw, new_uid:"new@recovered.invalid".into() };
        let summary = super::prepare_contact_restore(&op).unwrap();
        assert!(summary.preview_truncated);
        assert!(summary.projected_size > 64 * 1024);
        assert_eq!(summary.projected_sha256, format!("{:x}",Sha256::digest(isyncyou_connectors::dav::edit::recovered_contact(&op.original,&op.new_uid).unwrap())));
    }
    #[test]
    fn recovery_mail_review_warns_about_unrendered_mime() {
        let raw = b"From: a@example.test\r\nSubject: fixture\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\n";
        let review = super::review_mail_headers(raw);
        assert!(review.contains("Subject: fixture"));
        assert!(review.contains("attachments and other MIME parts are not displayed"));
        assert!(!review.contains("--x"));
        assert!(super::review_mail_headers(b"malformed").contains("headers unavailable"));
    }
    #[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
    #[test]
    fn linux_signer_uses_verifiable_p256_asn1_and_rejects_empty_challenge() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use ring::signature::KeyPair;
        let rng = ring::rand::SystemRandom::new();
        let private = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, private.as_ref(), &rng).unwrap();
        let public = key.public_key().as_ref().to_vec();
        let signer = LinuxRecoverySigner { installation:"installation-123456".into(), public_key:"public".into(), key };
        let challenge = b"bound account and effect";
        let signature = URL_SAFE_NO_PAD.decode(signer.sign(challenge).unwrap()).unwrap();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, public)
            .verify(challenge,&signature).unwrap();
        assert!(signer.sign(b"").is_err());
    }
    #[test]
    fn broker_checks_account_and_effect_before_device_signing() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        struct Signer(std::sync::atomic::AtomicUsize);
        impl RecoveryDeviceSigner for Signer {
            fn device_id(&self) -> &str { "device" }
            fn sign(&self, _: &[u8]) -> Result<String, String> { self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Ok("signature".into()) }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for account in ["wrong-account", "native-account"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                loop {
                    let count = stream.read(&mut buf).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buf[..count]);
                    if let Some(pos) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..pos]);
                        let length = headers.lines().find_map(|line| line.split_once(':').and_then(|(k,v)|
                            if k.eq_ignore_ascii_case("content-length") { v.trim().parse::<usize>().ok() } else { None })).unwrap();
                        if request.len() >= pos + 4 + length { break; }
                    }
                }
                let expires = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 60;
                let payload = json!({"schema":1,"challenge_id":"challenge","native_account":account,"subject":"person",
                    "device":"device","action":"read","effect":{"catalog_after":0},"expires":expires}).to_string();
                let response = json!({"challenge_id":"challenge","payload":URL_SAFE_NO_PAD.encode(payload.as_bytes()),"expires":expires}).to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
            }
        });
        let broker = RecoveryBroker { http:reqwest::blocking::Client::builder().no_proxy().build().unwrap(), origin:reqwest::Url::parse(&format!("http://{addr}/")).unwrap(), token:"test".into(), native_account:"native-account".into() };
        let signer = Signer(std::sync::atomic::AtomicUsize::new(0));
        let effect = json!({"catalog_after":0});
        assert!(broker.signed(&signer,"read",&effect).is_err());
        assert_eq!(signer.0.load(std::sync::atomic::Ordering::SeqCst),0);
        assert!(broker.signed(&signer,"read",&effect).is_ok());
        assert_eq!(signer.0.load(std::sync::atomic::Ordering::SeqCst),1);
        server.join().unwrap();
    }
    #[test]
    fn calendar_restore_prepares_bound_personal_effect_and_rejects_changed_source() {
        assert_eq!(recovery_preview_multiline(&json!("first\r\nsecond\rthird"),4096).unwrap(), "first\nsecond\nthird");
        assert!(recovery_preview_multiline(&json!("first\u{0007}second"),4096).is_err());
        let original = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:old\r\nSUMMARY:Planning\r\nDESCRIPTION:Discuss plan\r\nLOCATION:Office\r\nX-KEEP:custom\r\nATTACH;FMTTYPE=application/pdf:https://example.test/document\r\nDTSTART:20261103T120000Z\r\nORGANIZER:mailto:a@example.test\r\nATTENDEE:mailto:b@example.test\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_vec();
        let mut op = ApprovedCalendarRestore { operation_id:"op-1".into(), account_id:"a".into(), origin:"https://example.test/".into(),
            entry:"/dav/".into(), collection:"https://example.test/dav/recovered/".into(), source_generation:"a".repeat(64),
            source_object:"b".repeat(64), source_version:"c".repeat(64), original_sha256:format!("{:x}",Sha256::digest(&original)),
            original, uid_map:HashMap::from([("old".into(),"new".into())]), kind:CalendarRestoreKind::Event };
        let summary = prepare_calendar_restore(&op).unwrap();
        assert_eq!(summary.components[0].title.as_deref(), Some("Planning"));
        assert_eq!(summary.components[0].uid, "new");
        assert!(summary.components[0].other_properties.iter().any(|p| p.name == "DESCRIPTION" && p.value == "Discuss plan"));
        assert!(summary.components[0].other_properties.iter().any(|p| p.name == "LOCATION" && p.value == "Office"));
        assert!(summary.components[0].other_properties.iter().any(|p| p.name == "X-KEEP" && p.value == "custom"));
        assert!(summary.components[0].has_attachments);
        assert!(summary.components[0].other_properties.iter().any(|p| p.name == "ATTACH" &&
            p.value == "https://example.test/document" && p.parameters.get("FMTTYPE").map(String::as_str) == Some("application/pdf")));
        assert_eq!(summary.components[0].start.as_ref().map(|x| x.value.as_str()), Some("20261103T120000Z"));
        assert!(summary.removed_scheduling_participants);
        assert!(summary.removed_scheduling_method);
        assert!(summary.removed_alarms);
        let projected = crate::dav_live::prepare_recovered_calendar(&op.original, &op.uid_map).unwrap();
        let text = String::from_utf8(projected.clone()).unwrap();
        assert!(text.contains("UID:new\r\n"));
        for removed in ["METHOD:","ORGANIZER:","ATTENDEE:","VALARM"] { assert!(!text.contains(removed)); }
        assert_eq!(summary.projected_sha256, format!("{:x}",Sha256::digest(&projected)));
        let embedded = std::str::from_utf8(&op.original).unwrap().replace("https://example.test/document", "data:application/pdf;base64,AA==");
        let mut embedded_op = op.clone();
        embedded_op.original = embedded.into_bytes();
        embedded_op.original_sha256 = format!("{:x}",Sha256::digest(&embedded_op.original));
        assert!(prepare_calendar_restore(&embedded_op).is_err());
        op.account_id = "other".into();
        assert_ne!(prepare_calendar_restore(&op).unwrap().approval_sha256, summary.approval_sha256);
        op.account_id = "a".into();
        op.collection = "https://example.test/dav/other/".into();
        assert_ne!(prepare_calendar_restore(&op).unwrap().approval_sha256, summary.approval_sha256);
        op.kind = CalendarRestoreKind::Task;
        assert!(prepare_calendar_restore(&op).is_err());
        op.kind = CalendarRestoreKind::Event;
        op.original.push(b'!');
        assert!(prepare_calendar_restore(&op).is_err());
    }
    #[test]
    fn task_restore_requires_mapped_parent_and_removes_scheduling() {
        let original = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT:parent\r\nATTENDEE:mailto:b@example.test\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n".to_vec();
        let mut op = ApprovedCalendarRestore { operation_id:"task-1".into(), account_id:"a".into(), origin:"https://example.test/".into(),
            entry:"/dav/".into(), collection:"https://example.test/dav/recovered/".into(), source_generation:"a".repeat(64),
            source_object:"b".repeat(64), source_version:"c".repeat(64), original_sha256:format!("{:x}",Sha256::digest(&original)),
            original, uid_map:HashMap::from([("child".into(),"new-child".into())]), kind:CalendarRestoreKind::Task };
        assert!(prepare_calendar_restore(&op).is_err());
        op.uid_map.insert("parent".into(),"new-parent".into());
        let summary = prepare_calendar_restore(&op).unwrap();
        assert_eq!(summary.kind, CalendarRestoreKind::Task);
        let projected = String::from_utf8(crate::dav_live::prepare_recovered_calendar(&op.original, &op.uid_map).unwrap()).unwrap();
        assert!(projected.contains("RELATED-TO;RELTYPE=PARENT:new-parent\r\n"));
        assert!(!projected.contains("ATTENDEE:"));
        assert!(!projected.contains("VALARM"));
    }
    #[test]
    fn recovery_preview_discloses_retained_timezone_rules_without_attachments() {
        let original = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Example/Zone\r\nBEGIN:STANDARD\r\nDTSTART:20261025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:old\r\nSUMMARY:Planning\r\nDTSTART;TZID=Example/Zone:20261103T120000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let projected = crate::dav_live::prepare_recovered_calendar(original,&HashMap::from([("old".into(),"new".into())])).unwrap();
        let (_,_,_,_,timezones) = calendar_preview(original,&projected).unwrap();
        assert_eq!(timezones.len(),1);
        assert!(timezones[0].to_ascii_lowercase().contains("standard"));
        assert!(timezones[0].to_ascii_lowercase().contains("tzoffsetto"));
        let unsafe_zone = std::str::from_utf8(original).unwrap().replace("TZID:Example/Zone\r\n", "TZID:Example/Zone\r\nATTACH:data:application/octet-stream;base64,AA==\r\n");
        let projected = crate::dav_live::prepare_recovered_calendar(unsafe_zone.as_bytes(),&HashMap::from([("old".into(),"new".into())])).unwrap();
        assert!(calendar_preview(unsafe_zone.as_bytes(),&projected).is_err());
    }
    #[test]
    fn recovery_batch_prepares_consistent_parent_mapping_before_dispatch() {
        fn selection(operation: &str) -> RecoverySelection {
            RecoverySelection { operation_id:operation.into(), account_id:"a".into(), origin:"https://example.test/".into(),
                generation:"a".repeat(64), object:"b".repeat(64), version:"c".repeat(64), confirmation:"ok".into() }
        }
        fn payload(raw: &[u8]) -> RecoveredPayload {
            RecoveredPayload { bytes:raw.to_vec(), kind:"task".into(), encoding:"raw".into(), sha256:format!("{:x}",Sha256::digest(raw)) }
        }
        let parent = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:parent\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let child = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:child\r\nRELATED-TO;RELTYPE=PARENT:parent\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (a,b) = (selection("a"), selection("b"));
        assert!(calendar_batch_from_payloads(vec![(&b,payload(child))],"/dav/","https://example.test/recovered/").is_err());
        let result = calendar_batch_from_payloads(vec![(&b,payload(child)),(&a,payload(parent))],"/dav/","https://example.test/recovered/").unwrap();
        assert_eq!(result[0].0.operation_id, "a");
        assert_eq!(result[1].0.operation_id, "b");
        let parent_uid = result[0].0.uid_map.get("parent").unwrap();
        let projected = crate::dav_live::prepare_recovered_calendar(&result[1].0.original, &result[1].0.uid_map).unwrap();
        assert!(String::from_utf8(projected).unwrap().contains(&format!("RELATED-TO;RELTYPE=PARENT:{parent_uid}")));
        assert_eq!(result[0].0.uid_map, result[1].0.uid_map);
        let items: Vec<_> = result.into_iter().map(|(item,_)| item).collect();
        let hash = calendar_batch_approval(&items).unwrap();
        let batch = ApprovedCalendarBatch { items, approval_sha256:hash.clone() };
        let mut calls = Vec::new();
        let stopped = execute_calendar_batch(&batch, &hash, |op,_| {
            calls.push(op.operation_id.clone());
            Ok(if op.operation_id == "a" { crate::dav_live::RecoveryImportOutcome::Confirmed { href:"parent".into() } }
               else { crate::dav_live::RecoveryImportOutcome::Unknown })
        }).unwrap();
        assert_eq!(stopped,CalendarBatchOutcome::Stopped {completed:vec!["parent".into()],next_index:1,reason:"unknown"});
        assert_eq!(calls,vec!["a","b"]);
        let resumed = execute_calendar_batch(&batch, &hash, |op,_| {
            calls.push(op.operation_id.clone());
            Ok(crate::dav_live::RecoveryImportOutcome::Confirmed {href:op.operation_id.clone()})
        }).unwrap();
        assert_eq!(resumed, CalendarBatchOutcome::Confirmed(vec!["a".into(),"b".into()]));
        assert_eq!(calls,vec!["a","b","a","b"]);
        let mut altered = batch.clone();
        altered.items.swap(0,1);
        assert!(execute_calendar_batch(&altered,&hash,|_,_| panic!("unapproved order must not dispatch")).is_err());
        let encoded = serde_json::to_vec(&batch).unwrap();
        let decoded: ApprovedCalendarBatch = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(calendar_batch_approval(&decoded.items).unwrap(),hash);
        let cyclic_parent = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:parent\r\nRELATED-TO:child\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(calendar_batch_from_payloads(vec![(&a,payload(cyclic_parent)),(&b,payload(child))],"/dav/","https://example.test/recovered/").is_err());
        struct TestDevice;
        impl RecoveryDeviceSigner for TestDevice {
            fn device_id(&self) -> &str { "test-device" }
            fn sign(&self, _: &[u8]) -> Result<String, String> { Err("not used".into()) }
        }
        let device = TestDevice;
        let status = |selection: &RecoverySelection, bytes: &[u8]| BoundRecoveryStatus {state:"available".into(),kind:"task".into(),encoding:"raw".into(),
            size:bytes.len() as u64,sha256:format!("{:x}",Sha256::digest(bytes)),selection_sha256:recovery_selection_digest(selection,device.device_id())};
        let selections = vec![a.clone(),b.clone()];
        let statuses = vec![status(&a,parent),status(&b,child)];
        assert!(prepare_staged_calendar_batch(&selections,&statuses,&device,vec![parent.to_vec(),child.to_vec()],"/dav/","https://example.test/recovered/").is_ok());
        assert!(prepare_staged_calendar_batch(&selections,&statuses,&device,vec![parent.to_vec(),b"changed".to_vec()],"/dav/","https://example.test/recovered/").is_err());
        let mut swapped = statuses.clone();
        swapped.swap(0,1);
        assert!(prepare_staged_calendar_batch(&selections,&swapped,&device,vec![parent.to_vec(),child.to_vec()],"/dav/","https://example.test/recovered/").is_err());
    }
    #[test]
    fn recovered_folder_requires_explicit_unshared_acl() {
        assert!(is_unshared(&json!({"shareWith": null})));
        assert!(is_unshared(&json!({"shareWith": {}})));
        assert!(!is_unshared(&json!({})));
        assert!(!is_unshared(&json!({"shareWith": {"other": {"mayReadItems": true}}})));
    }
    struct Fake { folder: Cell<bool>, folder_count: Cell<u32>, upload_count: Cell<u32>, import_count: Cell<u32>,
                  lost_import: bool, readable: Cell<bool> }
    impl Transport for Fake {
        fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> {
            Ok(("state".into(), if self.folder.get() { vec!["recovered".into()] } else { vec![] }))
        }
        fn create_recovered(&self, _: &str, _: &str) -> Result<Option<String>, JmapError> {
            self.folder_count.set(self.folder_count.get() + 1);
            self.folder.set(true);
            Ok(Some("recovered".into()))
        }
        fn recovered_folder(&self, _: &str) -> Result<bool, JmapError> { Ok(self.folder.get()) }
        fn upload(&self, _: &[u8]) -> Result<String, JmapError> {
            self.upload_count.set(self.upload_count.get() + 1); Ok("blob".into())
        }
        fn import(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, JmapError> {
            self.import_count.set(self.import_count.get() + 1);
            if self.lost_import { Err(JmapError::OutcomeUnknown) } else { Ok(Some("native-id".into())) }
        }
        fn readback(&self, _: &str, _: &str, _: &str) -> Result<bool, JmapError> { Ok(self.readable.get()) }
    }
    fn fixture() -> (ProviderScope, ApprovedMailRestore) {
        let mime = b"Subject: saved\r\n\r\noriginal".to_vec();
        (ProviderScope { account_id:"a".into(), origin:"https://example.test/".into(),
                         service:"mail".into(), collection:"native-a".into() },
         ApprovedMailRestore { operation_id:"restore-1".into(), account_id:"a".into(),
            origin:"https://example.test/".into(), mailbox_id:"recovered".into(),
            source_generation:"a".repeat(64), source_object:"b".repeat(64), source_version:"c".repeat(64),
            original_sha256:format!("{:x}", Sha256::digest(&mime)), original_mime:mime })
    }
    fn fake(folder: bool, lost_import: bool, readable: bool) -> Fake {
        Fake { folder:Cell::new(folder), folder_count:Cell::new(0), upload_count:Cell::new(0), import_count:Cell::new(0), lost_import, readable:Cell::new(readable) }
    }
    #[test]
    fn readback_verified_before_success_and_repeat_does_not_resend() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let transport = fake(true, false, true);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!((transport.upload_count.get(), transport.import_count.get()), (1,1));
    }
    #[test]
    fn missing_folder_and_lost_import_never_trigger_unapproved_retry() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let denied = fake(false, false, true);
        assert_eq!(run(&store, &denied, &scope, &op).unwrap(), RestoreOutcome::RecoveredFolderUnavailable);
        assert_eq!(denied.upload_count.get(), 0);
        let lost = fake(true, true, true);
        assert_eq!(run(&store, &lost, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        assert_eq!(run(&store, &lost, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        assert_eq!((lost.upload_count.get(), lost.import_count.get()), (1,1));
    }
    #[test]
    fn known_native_id_reconciles_read_only_without_second_import() {
        let store = Store::open_in_memory().unwrap(); let (scope, op) = fixture(); let transport = fake(true, false, false);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Unknown);
        transport.readable.set(true);
        assert_eq!(run(&store, &transport, &scope, &op).unwrap(), RestoreOutcome::Verified("native-id".into()));
        assert_eq!((transport.upload_count.get(), transport.import_count.get()), (1,1));
    }
    #[test]
    fn recovered_folder_create_is_persisted_verified_and_not_duplicated() {
        let store = Store::open_in_memory().unwrap(); let (scope, _op) = fixture(); let transport = fake(false, false, true);
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Ready("recovered".into()));
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Ready("recovered".into()));
        assert_eq!(transport.folder_count.get(), 1);
    }
    #[test]
    fn unknown_folder_creation_is_not_repeated_or_inferred_from_name() {
        struct Lost { calls: Cell<u32> }
        impl Transport for Lost {
            fn discover_recovered(&self) -> Result<(String, Vec<String>), JmapError> { Ok(("s".into(), vec![])) }
            fn create_recovered(&self, _: &str, _: &str) -> Result<Option<String>, JmapError> {
                self.calls.set(self.calls.get() + 1); Err(JmapError::OutcomeUnknown)
            }
            fn recovered_folder(&self, _: &str) -> Result<bool, JmapError> { panic!("no known native id") }
            fn upload(&self, _: &[u8]) -> Result<String, JmapError> { panic!("no upload") }
            fn import(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, JmapError> { panic!("no import") }
            fn readback(&self, _: &str, _: &str, _: &str) -> Result<bool, JmapError> { panic!("no readback") }
        }
        let store = Store::open_in_memory().unwrap(); let (scope, _op) = fixture(); let transport = Lost { calls:Cell::new(0) };
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Unknown);
        assert_eq!(ensure_folder(&store, &transport, &scope, "restore-1").unwrap(), FolderOutcome::Unknown);
        assert_eq!(transport.calls.get(), 1);
    }
}
