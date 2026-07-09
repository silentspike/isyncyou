//! Validated identifiers for the cross-device agent session.
//!
//! These IDs are used in cloud paths, AEAD associated data, and lease ownership. Keep
//! construction narrow so callers cannot smuggle path separators or delimiter-shaped
//! strings into any of those places.

use crate::AgentError;
use std::fmt;

const SAFE_SEGMENT_MAX: usize = 128;
const TURN_ID_LEN: usize = 26;
const TURN_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn invalid_id(kind: &str, why: &str) -> AgentError {
    AgentError::Provider(format!("invalid {kind}: {why}"))
}

fn validate_safe_segment(kind: &str, value: &str) -> Result<(), AgentError> {
    if value.is_empty() {
        return Err(invalid_id(kind, "empty"));
    }
    if value.len() > SAFE_SEGMENT_MAX {
        return Err(invalid_id(kind, "too long"));
    }
    if value.contains("..") {
        return Err(invalid_id(kind, "contains traversal marker"));
    }
    if value == "." {
        return Err(invalid_id(kind, "current directory marker"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid_id(kind, "not a safe path segment"));
    }
    Ok(())
}

fn validate_turn_id(value: &str) -> Result<(), AgentError> {
    if value.len() != TURN_ID_LEN {
        return Err(invalid_id("turn id", "bad length"));
    }
    if !value.bytes().all(|b| TURN_ALPHABET.contains(&b)) {
        return Err(invalid_id("turn id", "not a Crockford ULID"));
    }
    Ok(())
}

macro_rules! safe_segment_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, AgentError> {
                let value = value.as_ref();
                validate_safe_segment($kind, value)?;
                Ok(Self(value.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = AgentError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = AgentError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

safe_segment_id!(SessionId, "session id");
safe_segment_id!(DeviceId, "device id");
safe_segment_id!(LeaseId, "lease id");

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(String);

impl TurnId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, AgentError> {
        let value = value.as_ref();
        validate_turn_id(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TurnId").field(&self.0).finish()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for TurnId {
    type Error = AgentError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for TurnId {
    type Error = AgentError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = "0000000000000000000000000A";

    #[test]
    fn session_id_rejects_path_traversal_segments() {
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "x/y",
            "x\\y",
            "x:y",
            "a..b",
            "has space",
        ] {
            assert!(SessionId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(SessionId::new("session_01-A.ok").is_ok());
    }

    #[test]
    fn device_and_lease_ids_use_same_safe_segment_policy() {
        assert!(DeviceId::new("pixel8pro").is_ok());
        assert!(LeaseId::new("lease-01").is_ok());
        assert!(DeviceId::new("device/other").is_err());
        assert!(LeaseId::new("lease:other").is_err());
    }

    #[test]
    fn turn_id_requires_crockford_ulid_shape() {
        assert!(TurnId::new(TURN).is_ok());
        assert!(TurnId::new("01").is_err());
        assert!(TurnId::new("0000000000000000000000000I").is_err());
        assert!(TurnId::new("0000000000000000000000000a").is_err());
    }
}
