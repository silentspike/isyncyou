use crate::PairingPayload;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::{aead, digest, hmac};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

const PAIRING_CODE_PREFIX: &str = "isy2.";
const PAIR_ID_BYTES: usize = 24;
const TRANSFER_KEY_BYTES: usize = 32;
const CLAIM_ID_BYTES: usize = 24;
const CLAIM_SECRET_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const PAIRING_TTL_MS: u64 = 5 * 60 * 1_000;
const CLAIM_RESUME_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_DESCRIPTOR_BYTES: usize = 64 * 1_024;
const SESSION_CANARY: &str = "isyncyou-session-pairing-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingV2Error {
    InvalidCode,
    InvalidDescriptor,
    Expired,
    AlreadyClaimed,
    Revoked,
    WrongClaim,
    Crypto,
    TransportUnavailable,
    OutcomeUnknown,
}

impl std::fmt::Display for PairingV2Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCode => "pairing_invalid_code",
            Self::InvalidDescriptor => "pairing_invalid_descriptor",
            Self::Expired => "pairing_expired",
            Self::AlreadyClaimed => "pairing_already_claimed",
            Self::Revoked => "pairing_revoked",
            Self::WrongClaim => "pairing_wrong_claim",
            Self::Crypto => "pairing_crypto_unavailable",
            Self::TransportUnavailable => "pairing_transport_unavailable",
            Self::OutcomeUnknown => "pairing_outcome_unknown",
        })
    }
}

#[cfg(feature = "onedrive")]
#[derive(Clone)]
pub struct OneDrivePairingTransportV2 {
    client: isyncyou_graph::GraphClient,
}

#[cfg(feature = "onedrive")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedPairingDescriptorV2 {
    pub descriptor: PairingDescriptorV2,
    pub item_id: String,
    pub etag: String,
}

#[cfg(feature = "onedrive")]
impl OneDrivePairingTransportV2 {
    const ROOT: &'static str = "Apps/iSyncYou/agent/pairing";

    pub fn new(token: impl Into<String>) -> Self {
        Self {
            client: isyncyou_graph::GraphClient::new(token),
        }
    }

    pub fn create_or_adopt(
        &self,
        source: &PairingSourceSecretV2,
    ) -> Result<VersionedPairingDescriptorV2, PairingV2Error> {
        self.ensure_root()?;
        let path = self.path(source.pair_id())?;
        let expected = source.descriptor().canonical_bytes()?;
        let created = self
            .client
            .upload_content_with_conflict_behavior(
                &path,
                &expected,
                isyncyou_graph::http::ConflictBehavior::Fail,
            )
            .map_err(|_| PairingV2Error::TransportUnavailable)?;
        if let Some(item) = created {
            return versioned_descriptor(item, source.descriptor().clone());
        }
        let current = self.load(source.pair_id())?;
        let bytes = self.read_path(&path)?;
        validate_adopted_descriptor(&expected, &bytes, source.descriptor(), &current.descriptor)?;
        Ok(current)
    }

    pub fn load(&self, pair_id: &str) -> Result<VersionedPairingDescriptorV2, PairingV2Error> {
        let path = self.path(pair_id)?;
        let item = self
            .client
            .get_drive_item_by_path(&path, &["id", "eTag"])
            .map_err(|_| PairingV2Error::TransportUnavailable)?
            .ok_or(PairingV2Error::InvalidDescriptor)?;
        let descriptor = PairingDescriptorV2::parse(&self.read_path(&path)?)?;
        versioned_descriptor(item, descriptor)
    }

    pub fn compare_and_swap(
        &self,
        current: &VersionedPairingDescriptorV2,
        next: &PairingDescriptorV2,
    ) -> Result<Option<VersionedPairingDescriptorV2>, PairingV2Error> {
        if current.descriptor.pair_id != next.pair_id {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let bytes = next.canonical_bytes()?;
        self.client
            .replace_content_if_match(&current.item_id, &bytes, &current.etag)
            .map_err(|_| PairingV2Error::TransportUnavailable)?
            .map(|item| versioned_descriptor(item, next.clone()))
            .transpose()
    }

    pub fn delete(&self, current: &VersionedPairingDescriptorV2) -> Result<bool, PairingV2Error> {
        self.client
            .delete_item_if_match(&current.item_id, &current.etag)
            .map_err(|_| PairingV2Error::TransportUnavailable)
    }

    fn ensure_root(&self) -> Result<(), PairingV2Error> {
        let mut parent_id = String::new();
        let mut path = String::new();
        for component in ["Apps", "iSyncYou", "agent", "pairing"] {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(component);
            let item = self
                .client
                .get_drive_item_by_path(&path, &["id", "folder"])
                .map_err(|_| PairingV2Error::TransportUnavailable)?;
            let item = match item {
                Some(item) => item,
                None => match self.client.create_folder(&parent_id, component) {
                    Ok(item) => item,
                    Err(_) => self
                        .client
                        .get_drive_item_by_path(&path, &["id", "folder"])
                        .map_err(|_| PairingV2Error::TransportUnavailable)?
                        .ok_or(PairingV2Error::TransportUnavailable)?,
                },
            };
            if item.get("folder").is_none() {
                return Err(PairingV2Error::TransportUnavailable);
            }
            parent_id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or(PairingV2Error::TransportUnavailable)?
                .to_owned();
        }
        Ok(())
    }

    fn path(&self, pair_id: &str) -> Result<String, PairingV2Error> {
        valid_id(pair_id, 32)
            .then(|| format!("{}/{}.json", Self::ROOT, pair_id))
            .ok_or(PairingV2Error::InvalidDescriptor)
    }

    fn read_path(&self, path: &str) -> Result<Vec<u8>, PairingV2Error> {
        self.client
            .get_bytes_bounded(
                &format!("/me/drive/root:/{path}:/content"),
                MAX_DESCRIPTOR_BYTES,
            )
            .map_err(|_| PairingV2Error::TransportUnavailable)
    }
}

#[cfg(any(feature = "onedrive", test))]
fn validate_adopted_descriptor(
    expected_bytes: &[u8],
    observed_bytes: &[u8],
    expected: &PairingDescriptorV2,
    observed: &PairingDescriptorV2,
) -> Result<(), PairingV2Error> {
    if observed_bytes == expected_bytes && observed == expected {
        Ok(())
    } else {
        Err(PairingV2Error::OutcomeUnknown)
    }
}

#[cfg(feature = "onedrive")]
fn versioned_descriptor(
    item: serde_json::Value,
    descriptor: PairingDescriptorV2,
) -> Result<VersionedPairingDescriptorV2, PairingV2Error> {
    let item_id = item
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or(PairingV2Error::TransportUnavailable)?
        .to_owned();
    let etag = item
        .get("eTag")
        .and_then(serde_json::Value::as_str)
        .ok_or(PairingV2Error::TransportUnavailable)?
        .to_owned();
    Ok(VersionedPairingDescriptorV2 {
        descriptor,
        item_id,
        etag,
    })
}

impl std::error::Error for PairingV2Error {}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingCodeV2 {
    pair_id: String,
    transfer_key: [u8; TRANSFER_KEY_BYTES],
}

impl PairingCodeV2 {
    pub fn parse(value: &str) -> Result<Self, PairingV2Error> {
        if value.len() != 81 || !value.is_ascii() {
            return Err(PairingV2Error::InvalidCode);
        }
        let mut components = value.split('.');
        if components.next() != Some("isy2") {
            return Err(PairingV2Error::InvalidCode);
        }
        let pair_id = components.next().ok_or(PairingV2Error::InvalidCode)?;
        let transfer_key = components.next().ok_or(PairingV2Error::InvalidCode)?;
        if components.next().is_some() || !valid_id(pair_id, 32) || transfer_key.len() != 43 {
            return Err(PairingV2Error::InvalidCode);
        }
        Ok(Self {
            pair_id: pair_id.to_owned(),
            transfer_key: decode_array(transfer_key).map_err(|_| PairingV2Error::InvalidCode)?,
        })
    }

    pub fn pair_id(&self) -> &str {
        &self.pair_id
    }

    fn encode(&self) -> String {
        format!(
            "{PAIRING_CODE_PREFIX}{}.{}",
            self.pair_id,
            URL_SAFE_NO_PAD.encode(self.transfer_key)
        )
    }
}

impl std::fmt::Debug for PairingCodeV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingCodeV2")
            .field("pair_id", &self.pair_id)
            .field("transfer_key", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingRemoteStateV2 {
    Pending,
    Claimed,
    Consumed,
    Revoked,
    ClaimedExpired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingDescriptorV2 {
    pub version: u32,
    pub pair_id: String,
    pub expires_at_ms: u64,
    pub state: PairingRemoteStateV2,
    pub nonce: String,
    pub ciphertext: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_secret_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_until_ms: Option<u64>,
}

impl PairingDescriptorV2 {
    pub fn parse(bytes: &[u8]) -> Result<Self, PairingV2Error> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let descriptor: Self = parse_strict_json(bytes)?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PairingV2Error> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| PairingV2Error::InvalidDescriptor)?;
        (bytes.len() <= MAX_DESCRIPTOR_BYTES)
            .then_some(bytes)
            .ok_or(PairingV2Error::InvalidDescriptor)
    }

    pub fn revoke(mut self) -> Result<Self, PairingV2Error> {
        match self.state {
            PairingRemoteStateV2::Pending
            | PairingRemoteStateV2::Claimed
            | PairingRemoteStateV2::ClaimedExpired => {
                self.state = PairingRemoteStateV2::Revoked;
                Ok(self)
            }
            PairingRemoteStateV2::Consumed | PairingRemoteStateV2::Revoked => {
                Err(PairingV2Error::Revoked)
            }
        }
    }

    pub fn expire_claim(mut self, now_ms: u64) -> Result<Self, PairingV2Error> {
        self.validate()?;
        match self.state {
            PairingRemoteStateV2::Claimed if now_ms > self.resume_until_ms.unwrap_or_default() => {
                self.state = PairingRemoteStateV2::ClaimedExpired;
                Ok(self)
            }
            PairingRemoteStateV2::ClaimedExpired => Ok(self),
            PairingRemoteStateV2::Claimed => Err(PairingV2Error::AlreadyClaimed),
            PairingRemoteStateV2::Pending => Err(PairingV2Error::WrongClaim),
            PairingRemoteStateV2::Consumed => Err(PairingV2Error::AlreadyClaimed),
            PairingRemoteStateV2::Revoked => Err(PairingV2Error::Revoked),
        }
    }

    fn validate(&self) -> Result<(), PairingV2Error> {
        if self.version != 2
            || !valid_id(&self.pair_id, 32)
            || decode_array::<NONCE_BYTES>(&self.nonce).is_err()
            || URL_SAFE_NO_PAD.decode(&self.ciphertext).is_err()
        {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let claim_fields_present = self.claim_id.is_some()
            && self.claim_secret_hash.is_some()
            && self.destination_binding.is_some()
            && self.resume_until_ms.is_some();
        let claim_fields_valid = || {
            claim_fields_present
                && valid_id(self.claim_id.as_deref().unwrap_or_default(), 32)
                && self.claim_secret_hash.as_deref().map(str::len) == Some(43)
                && self.destination_binding.as_deref().map(str::len) == Some(43)
        };
        match self.state {
            PairingRemoteStateV2::Pending => {
                if self.claim_id.is_some()
                    || self.claim_secret_hash.is_some()
                    || self.destination_binding.is_some()
                    || self.resume_until_ms.is_some()
                {
                    return Err(PairingV2Error::InvalidDescriptor);
                }
            }
            PairingRemoteStateV2::Revoked => {
                let no_claim_fields = self.claim_id.is_none()
                    && self.claim_secret_hash.is_none()
                    && self.destination_binding.is_none()
                    && self.resume_until_ms.is_none();
                if !no_claim_fields && !claim_fields_valid() {
                    return Err(PairingV2Error::InvalidDescriptor);
                }
            }
            PairingRemoteStateV2::Claimed
            | PairingRemoteStateV2::Consumed
            | PairingRemoteStateV2::ClaimedExpired => {
                if !claim_fields_valid() {
                    return Err(PairingV2Error::InvalidDescriptor);
                }
            }
        }
        Ok(())
    }
}

fn parse_strict_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, PairingV2Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValueSeed
        .deserialize(&mut deserializer)
        .map_err(|_| PairingV2Error::InvalidDescriptor)?;
    deserializer
        .end()
        .map_err(|_| PairingV2Error::InvalidDescriptor)?;
    serde_json::from_value(value).map_err(|_| PairingV2Error::InvalidDescriptor)
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
        Ok(Value::String(value.to_owned()))
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingPlaintextV2 {
    version: u32,
    pairing_payload: String,
    canary: String,
}

#[derive(Clone)]
pub struct PairingSourceSecretV2 {
    code: PairingCodeV2,
    descriptor: PairingDescriptorV2,
}

impl PairingSourceSecretV2 {
    pub fn create(payload: &PairingPayload, now_ms: u64) -> Result<Self, PairingV2Error> {
        let pair_id = random_b64(PAIR_ID_BYTES)?;
        let transfer_key = random_array()?;
        let expires_at_ms = now_ms
            .checked_add(PAIRING_TTL_MS)
            .ok_or(PairingV2Error::InvalidDescriptor)?;
        let plaintext = serde_json::to_vec(&PairingPlaintextV2 {
            version: 2,
            pairing_payload: payload.encode().map_err(|_| PairingV2Error::Crypto)?,
            canary: SESSION_CANARY.into(),
        })
        .map_err(|_| PairingV2Error::Crypto)?;
        let nonce = random_array::<NONCE_BYTES>()?;
        let ciphertext = seal_payload(
            &transfer_key,
            &nonce,
            &pairing_aad(&pair_id, expires_at_ms),
            &plaintext,
        )?;
        Ok(Self {
            code: PairingCodeV2 {
                pair_id: pair_id.clone(),
                transfer_key,
            },
            descriptor: PairingDescriptorV2 {
                version: 2,
                pair_id,
                expires_at_ms,
                state: PairingRemoteStateV2::Pending,
                nonce: URL_SAFE_NO_PAD.encode(nonce),
                ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
                claim_id: None,
                claim_secret_hash: None,
                destination_binding: None,
                resume_until_ms: None,
            },
        })
    }

    pub fn pair_id(&self) -> &str {
        self.code.pair_id()
    }

    pub fn descriptor(&self) -> &PairingDescriptorV2 {
        &self.descriptor
    }

    pub fn reveal_code(&self) -> String {
        self.code.encode()
    }

    pub fn pairing_payload(&self) -> Result<PairingPayload, PairingV2Error> {
        open_pairing_payload(&self.code, &self.descriptor)
    }

    pub fn to_secret_bytes(&self) -> Result<Vec<u8>, PairingV2Error> {
        serde_json::to_vec(&PairingSourceSecretWireV2 {
            version: 2,
            code: self.code.encode(),
            descriptor: self.descriptor.clone(),
        })
        .map_err(|_| PairingV2Error::Crypto)
    }

    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self, PairingV2Error> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES + 256 {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let wire: PairingSourceSecretWireV2 =
            serde_json::from_slice(bytes).map_err(|_| PairingV2Error::InvalidDescriptor)?;
        let code = PairingCodeV2::parse(&wire.code)?;
        wire.descriptor.validate()?;
        if wire.version != 2 || wire.descriptor.pair_id != code.pair_id {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        open_pairing_payload(&code, &wire.descriptor)?;
        Ok(Self {
            code,
            descriptor: wire.descriptor,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingSourceSecretWireV2 {
    version: u32,
    code: String,
    descriptor: PairingDescriptorV2,
}

impl std::fmt::Debug for PairingSourceSecretV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingSourceSecretV2")
            .field("pair_id", &self.code.pair_id)
            .field("secret", &"[redacted]")
            .finish()
    }
}

pub struct PairingClaimV2 {
    pub descriptor: PairingDescriptorV2,
    pub payload: PairingPayload,
    claim_secret: [u8; CLAIM_SECRET_BYTES],
}

impl PairingClaimV2 {
    pub fn claim(
        code: &PairingCodeV2,
        descriptor: &PairingDescriptorV2,
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<Self, PairingV2Error> {
        descriptor.validate()?;
        if descriptor.pair_id != code.pair_id {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        match descriptor.state {
            PairingRemoteStateV2::Pending => {}
            PairingRemoteStateV2::Revoked => return Err(PairingV2Error::Revoked),
            _ => return Err(PairingV2Error::AlreadyClaimed),
        }
        if now_ms > descriptor.expires_at_ms {
            return Err(PairingV2Error::Expired);
        }
        let payload = open_pairing_payload(code, descriptor)?;
        let claim_id = random_b64(CLAIM_ID_BYTES)?;
        let claim_secret = random_array()?;
        let resume_until_ms = now_ms
            .checked_add(CLAIM_RESUME_TTL_MS)
            .ok_or(PairingV2Error::InvalidDescriptor)?;
        let destination_binding = destination_binding(&code.transfer_key, installation_principal);
        let mut claimed = descriptor.clone();
        claimed.state = PairingRemoteStateV2::Claimed;
        claimed.claim_id = Some(claim_id);
        claimed.claim_secret_hash = Some(hash_secret(&claim_secret));
        claimed.destination_binding = Some(destination_binding);
        claimed.resume_until_ms = Some(resume_until_ms);
        Ok(Self {
            descriptor: claimed,
            payload,
            claim_secret,
        })
    }

    pub fn resume(
        code: &PairingCodeV2,
        descriptor: &PairingDescriptorV2,
        claim_secret: &[u8; CLAIM_SECRET_BYTES],
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<PairingPayload, PairingV2Error> {
        descriptor.validate()?;
        if descriptor.pair_id != code.pair_id {
            return Err(PairingV2Error::WrongClaim);
        }
        match descriptor.state {
            PairingRemoteStateV2::Claimed | PairingRemoteStateV2::Consumed => {}
            PairingRemoteStateV2::ClaimedExpired => return Err(PairingV2Error::Expired),
            PairingRemoteStateV2::Revoked => return Err(PairingV2Error::Revoked),
            PairingRemoteStateV2::Pending => return Err(PairingV2Error::WrongClaim),
        }
        if now_ms > descriptor.resume_until_ms.unwrap_or_default() {
            return Err(PairingV2Error::Expired);
        }
        let expected_hash = descriptor.claim_secret_hash.as_deref().unwrap_or_default();
        let expected_binding = descriptor
            .destination_binding
            .as_deref()
            .unwrap_or_default();
        if !constant_time_eq(
            hash_secret(claim_secret).as_bytes(),
            expected_hash.as_bytes(),
        ) || !constant_time_eq(
            destination_binding(&code.transfer_key, installation_principal).as_bytes(),
            expected_binding.as_bytes(),
        ) {
            return Err(PairingV2Error::WrongClaim);
        }
        open_pairing_payload(code, descriptor)
    }

    pub fn finalize(
        &self,
        descriptor: &PairingDescriptorV2,
    ) -> Result<PairingDescriptorV2, PairingV2Error> {
        descriptor.validate()?;
        if descriptor.state == PairingRemoteStateV2::Consumed
            && constant_time_eq(
                hash_secret(&self.claim_secret).as_bytes(),
                descriptor
                    .claim_secret_hash
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            )
        {
            return Ok(descriptor.clone());
        }
        if descriptor.state != PairingRemoteStateV2::Claimed
            || !constant_time_eq(
                hash_secret(&self.claim_secret).as_bytes(),
                descriptor
                    .claim_secret_hash
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            )
        {
            return Err(PairingV2Error::WrongClaim);
        }
        let mut consumed = descriptor.clone();
        consumed.state = PairingRemoteStateV2::Consumed;
        Ok(consumed)
    }

    pub fn claim_secret(&self) -> &[u8; CLAIM_SECRET_BYTES] {
        &self.claim_secret
    }

    pub fn to_resume_bytes(&self, code: &PairingCodeV2) -> Result<Vec<u8>, PairingV2Error> {
        if code.pair_id != self.descriptor.pair_id {
            return Err(PairingV2Error::WrongClaim);
        }
        serde_json::to_vec(&PairingClaimResumeWireV2 {
            version: 2,
            code: code.encode(),
            descriptor: self.descriptor.clone(),
            claim_secret: URL_SAFE_NO_PAD.encode(self.claim_secret),
        })
        .map_err(|_| PairingV2Error::Crypto)
    }

    pub fn from_resume_bytes(
        bytes: &[u8],
        installation_principal: &str,
        now_ms: u64,
    ) -> Result<(PairingCodeV2, Self), PairingV2Error> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES + 256 {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let wire: PairingClaimResumeWireV2 =
            serde_json::from_slice(bytes).map_err(|_| PairingV2Error::InvalidDescriptor)?;
        if wire.version != 2 {
            return Err(PairingV2Error::InvalidDescriptor);
        }
        let code = PairingCodeV2::parse(&wire.code)?;
        let claim_secret = decode_array(&wire.claim_secret)?;
        let payload = Self::resume(
            &code,
            &wire.descriptor,
            &claim_secret,
            installation_principal,
            now_ms,
        )?;
        Ok((
            code,
            Self {
                descriptor: wire.descriptor,
                payload,
                claim_secret,
            },
        ))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingClaimResumeWireV2 {
    version: u32,
    code: String,
    descriptor: PairingDescriptorV2,
    claim_secret: String,
}

impl std::fmt::Debug for PairingClaimV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairingClaimV2")
            .field("descriptor", &self.descriptor)
            .field("payload", &"[redacted]")
            .field("claim_secret", &"[redacted]")
            .finish()
    }
}

fn open_pairing_payload(
    code: &PairingCodeV2,
    descriptor: &PairingDescriptorV2,
) -> Result<PairingPayload, PairingV2Error> {
    let nonce = decode_array(&descriptor.nonce)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&descriptor.ciphertext)
        .map_err(|_| PairingV2Error::InvalidDescriptor)?;
    let plaintext = open_payload(
        &code.transfer_key,
        &nonce,
        &pairing_aad(&descriptor.pair_id, descriptor.expires_at_ms),
        &ciphertext,
    )?;
    let plaintext: PairingPlaintextV2 =
        serde_json::from_slice(&plaintext).map_err(|_| PairingV2Error::InvalidDescriptor)?;
    if plaintext.version != 2 || plaintext.canary != SESSION_CANARY {
        return Err(PairingV2Error::InvalidDescriptor);
    }
    PairingPayload::parse(&plaintext.pairing_payload).map_err(|_| PairingV2Error::InvalidDescriptor)
}

fn seal_payload(
    key: &[u8; TRANSFER_KEY_BYTES],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PairingV2Error> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| PairingV2Error::Crypto)?,
    );
    let mut output = plaintext.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(*nonce),
        aead::Aad::from(aad),
        &mut output,
    )
    .map_err(|_| PairingV2Error::Crypto)?;
    Ok(output)
}

fn open_payload(
    key: &[u8; TRANSFER_KEY_BYTES],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, PairingV2Error> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| PairingV2Error::Crypto)?,
    );
    let mut output = ciphertext.to_vec();
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(*nonce),
            aead::Aad::from(aad),
            &mut output,
        )
        .map_err(|_| PairingV2Error::Crypto)?;
    Ok(plaintext.to_vec())
}

fn pairing_aad(pair_id: &str, expires_at_ms: u64) -> Vec<u8> {
    let mut aad = b"isyncyou-pairing-transfer-v2".to_vec();
    aad.extend_from_slice(&(pair_id.len() as u32).to_be_bytes());
    aad.extend_from_slice(pair_id.as_bytes());
    aad.extend_from_slice(&expires_at_ms.to_be_bytes());
    aad
}

fn destination_binding(key: &[u8; TRANSFER_KEY_BYTES], principal: &str) -> String {
    URL_SAFE_NO_PAD.encode(hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, key),
        &[b"isyncyou-pairing-destination-v2\0", principal.as_bytes()].concat(),
    ))
}

fn hash_secret(secret: &[u8; CLAIM_SECRET_BYTES]) -> String {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"isyncyou-pairing-claim-secret-v2\0");
    context.update(secret);
    URL_SAFE_NO_PAD.encode(context.finish())
}

fn random_b64(bytes: usize) -> Result<String, PairingV2Error> {
    let mut value = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| PairingV2Error::Crypto)?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn random_array<const N: usize>() -> Result<[u8; N], PairingV2Error> {
    let mut value = [0u8; N];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| PairingV2Error::Crypto)?;
    Ok(value)
}

fn decode_array<const N: usize>(value: &str) -> Result<[u8; N], PairingV2Error> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| PairingV2Error::InvalidDescriptor)?
        .try_into()
        .map_err(|_| PairingV2Error::InvalidDescriptor)
}

fn valid_id(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    fn source(now_ms: u64) -> PairingSourceSecretV2 {
        let payload = PairingPayload::generate(SessionId::new("session-v2").unwrap()).unwrap();
        PairingSourceSecretV2::create(&payload, now_ms).unwrap()
    }

    #[test]
    fn pairing_code_and_descriptor_enforce_exact_format_path_and_size_bounds() {
        let source = source(1_000);
        let code = source.reveal_code();
        assert_eq!(code.len(), 81);
        assert_eq!(
            PairingCodeV2::parse(&code).unwrap().pair_id(),
            source.pair_id()
        );
        assert_eq!(
            PairingCodeV2::parse(&format!("{code}x")),
            Err(PairingV2Error::InvalidCode)
        );
        let bytes = source.descriptor().canonical_bytes().unwrap();
        assert_eq!(
            PairingDescriptorV2::parse(&bytes).unwrap(),
            *source.descriptor()
        );
        assert_eq!(
            PairingDescriptorV2::parse(&vec![b'x'; MAX_DESCRIPTOR_BYTES + 1]),
            Err(PairingV2Error::InvalidDescriptor)
        );
        let duplicate = format!(
            "{{\"version\":2,\"version\":2,\"pair_id\":\"{}\"}}",
            source.pair_id()
        );
        assert_eq!(
            PairingDescriptorV2::parse(duplicate.as_bytes()),
            Err(PairingV2Error::InvalidDescriptor)
        );
    }

    #[test]
    fn pairing_transfer_expires_after_five_minutes() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        assert_eq!(
            PairingClaimV2::claim(&code, source.descriptor(), "principal", 301_001).unwrap_err(),
            PairingV2Error::Expired
        );
    }

    #[test]
    fn pairing_transfer_is_single_use_under_concurrent_redeem() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, source.descriptor(), "principal-a", 2_000).unwrap();
        assert_eq!(claim.payload.session_id.as_str(), "session-v2");
        assert_eq!(
            PairingClaimV2::claim(&code, &claim.descriptor, "principal-b", 2_001).unwrap_err(),
            PairingV2Error::AlreadyClaimed
        );
    }

    #[test]
    fn pairing_claim_requires_same_claim_secret_and_device_binding() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, source.descriptor(), "principal-a", 2_000).unwrap();
        assert_eq!(
            PairingClaimV2::resume(
                &code,
                &claim.descriptor,
                claim.claim_secret(),
                "principal-b",
                2_001,
            )
            .unwrap_err(),
            PairingV2Error::WrongClaim
        );
        assert!(PairingClaimV2::resume(
            &code,
            &claim.descriptor,
            claim.claim_secret(),
            "principal-a",
            2_001,
        )
        .is_ok());
    }

    #[test]
    fn pairing_claim_after_resume_deadline_remains_unavailable_to_other_claimants() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, source.descriptor(), "principal-a", 2_000).unwrap();
        let expired = claim
            .descriptor
            .clone()
            .expire_claim(2_001 + CLAIM_RESUME_TTL_MS)
            .unwrap();
        assert_eq!(expired.state, PairingRemoteStateV2::ClaimedExpired);
        assert_eq!(
            PairingClaimV2::claim(&code, &expired, "principal-b", 3_000).unwrap_err(),
            PairingV2Error::AlreadyClaimed
        );
        assert_eq!(
            PairingClaimV2::resume(&code, &expired, claim.claim_secret(), "principal-a", 3_000,)
                .unwrap_err(),
            PairingV2Error::Expired
        );
    }

    #[test]
    fn pairing_claim_never_allows_second_claimant_after_claim_or_timeout() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, source.descriptor(), "principal-a", 2_000).unwrap();
        assert_eq!(
            PairingClaimV2::claim(&code, &claim.descriptor, "principal-b", 2_001).unwrap_err(),
            PairingV2Error::AlreadyClaimed
        );
        let expired = claim
            .descriptor
            .clone()
            .expire_claim(2_001 + CLAIM_RESUME_TTL_MS)
            .unwrap();
        assert_eq!(
            PairingClaimV2::claim(&code, &expired, "principal-b", 3_000).unwrap_err(),
            PairingV2Error::AlreadyClaimed
        );
    }

    #[test]
    fn pairing_same_claim_resumes_after_pending_expiry_before_resume_deadline() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, source.descriptor(), "principal-a", 2_000).unwrap();
        let resumed = PairingClaimV2::resume(
            &code,
            &claim.descriptor,
            claim.claim_secret(),
            "principal-a",
            source.descriptor().expires_at_ms + 1,
        )
        .unwrap();
        assert_eq!(resumed.session_id.as_str(), "session-v2");
    }

    #[test]
    fn lost_pairing_claim_requires_source_revoke_and_new_transfer() {
        let original = source(1_000);
        let code = PairingCodeV2::parse(&original.reveal_code()).unwrap();
        let claim =
            PairingClaimV2::claim(&code, original.descriptor(), "principal-a", 2_000).unwrap();
        let expired = claim
            .descriptor
            .expire_claim(2_001 + CLAIM_RESUME_TTL_MS)
            .unwrap();
        let revoked = expired.revoke().unwrap();
        assert_eq!(revoked.state, PairingRemoteStateV2::Revoked);
        assert_eq!(
            PairingClaimV2::claim(&code, &revoked, "principal-b", 3_000).unwrap_err(),
            PairingV2Error::Revoked
        );
        assert_ne!(source(3_000).pair_id(), original.pair_id());
    }

    #[test]
    fn pairing_transfer_validates_empty_session_with_canary() {
        let source = source(1_000);
        let payload = source.pairing_payload().unwrap();
        assert_eq!(payload.session_id.as_str(), "session-v2");

        let mut descriptor = source.descriptor().clone();
        let mut ciphertext = URL_SAFE_NO_PAD.decode(&descriptor.ciphertext).unwrap();
        ciphertext[0] ^= 1;
        descriptor.ciphertext = URL_SAFE_NO_PAD.encode(ciphertext);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        assert_eq!(
            PairingClaimV2::claim(&code, &descriptor, "principal", 2_000).unwrap_err(),
            PairingV2Error::Crypto
        );
    }

    #[test]
    fn pairing_reveal_ambiguous_create_adopts_only_exact_descriptor() {
        let source = source(1_000);
        let expected = source.descriptor().canonical_bytes().unwrap();
        assert_eq!(
            validate_adopted_descriptor(
                &expected,
                &expected,
                source.descriptor(),
                source.descriptor(),
            ),
            Ok(())
        );
        let mut changed = source.descriptor().clone();
        changed.expires_at_ms += 1;
        assert_eq!(
            validate_adopted_descriptor(&expected, &expected, source.descriptor(), &changed),
            Err(PairingV2Error::OutcomeUnknown)
        );
        let mut changed_bytes = expected.clone();
        changed_bytes.push(b' ');
        assert_eq!(
            validate_adopted_descriptor(
                &expected,
                &changed_bytes,
                source.descriptor(),
                source.descriptor(),
            ),
            Err(PairingV2Error::OutcomeUnknown)
        );
    }

    #[test]
    fn pairing_secret_never_appears_in_logs_api_errors_or_evidence() {
        let source = source(1_000);
        let code = source.reveal_code();
        assert!(!format!("{source:?}").contains(&code));
        let parsed = PairingCodeV2::parse(&code).unwrap();
        assert!(!format!("{parsed:?}").contains(&code));
        assert_eq!(
            PairingV2Error::WrongClaim.to_string(),
            "pairing_wrong_claim"
        );
    }

    #[test]
    fn pairing_crash_after_local_install_finalizes_consumed_idempotently() {
        let source = source(1_000);
        let code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        let claim = PairingClaimV2::claim(&code, source.descriptor(), "principal", 2_000).unwrap();
        let consumed = claim.finalize(&claim.descriptor).unwrap();
        assert_eq!(consumed.state, PairingRemoteStateV2::Consumed);
        assert_eq!(claim.finalize(&consumed).unwrap(), consumed);
    }

    #[test]
    fn pairing_transfer_rejects_wrong_key_canary_and_etag() {
        let source = source(1_000);
        let mut code = PairingCodeV2::parse(&source.reveal_code()).unwrap();
        code.transfer_key[0] ^= 1;
        assert_eq!(
            PairingClaimV2::claim(&code, source.descriptor(), "principal", 2_000).unwrap_err(),
            PairingV2Error::Crypto
        );
    }
}
