//! Typed, redacted connectivity observations for the product provider path.
//!
//! This module deliberately contains no credential, URL, or provider response data.
//! The caller selects one closed provider/purpose pair and maps transport facts into a
//! small public diagnostic code.

use std::sync::atomic::{AtomicUsize, Ordering};

pub const MAX_CONCURRENT_PROBES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityProvider {
    Claude,
    Codex,
}

impl ConnectivityProvider {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityPurpose {
    OAuthStart,
    TurnStart,
    Refresh,
    CredentialRevoke,
}

impl ConnectivityPurpose {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "oauth_start" => Some(Self::OAuthStart),
            "turn_start" => Some(Self::TurnStart),
            "refresh" => Some(Self::Refresh),
            "credential_revoke" => Some(Self::CredentialRevoke),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::OAuthStart => "oauth_start",
            Self::TurnStart => "turn_start",
            Self::Refresh => "refresh",
            Self::CredentialRevoke => "credential_revoke",
        }
    }
}

/// Internal target selection. The actual host/path remain inside transport code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeTarget {
    ClaudeOAuth,
    ClaudeInference,
    CodexOAuth,
    CodexInference,
    ClaudeRevoke,
    CodexRevoke,
}

pub fn target_for(provider: ConnectivityProvider, purpose: ConnectivityPurpose) -> ProbeTarget {
    match (provider, purpose) {
        (ConnectivityProvider::Claude, ConnectivityPurpose::OAuthStart)
        | (ConnectivityProvider::Claude, ConnectivityPurpose::Refresh) => ProbeTarget::ClaudeOAuth,
        (ConnectivityProvider::Claude, ConnectivityPurpose::TurnStart) => {
            ProbeTarget::ClaudeInference
        }
        (ConnectivityProvider::Claude, ConnectivityPurpose::CredentialRevoke) => {
            ProbeTarget::ClaudeRevoke
        }
        (ConnectivityProvider::Codex, ConnectivityPurpose::OAuthStart)
        | (ConnectivityProvider::Codex, ConnectivityPurpose::Refresh) => ProbeTarget::CodexOAuth,
        (ConnectivityProvider::Codex, ConnectivityPurpose::TurnStart) => {
            ProbeTarget::CodexInference
        }
        (ConnectivityProvider::Codex, ConnectivityPurpose::CredentialRevoke) => {
            ProbeTarget::CodexRevoke
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestrictBackgroundStatus {
    Disabled,
    Whitelisted,
    Enabled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AndroidNetworkSnapshot {
    pub active_network: bool,
    pub internet_capability: bool,
    pub validated_capability: bool,
    pub metered: bool,
    pub restrict_background: RestrictBackgroundStatus,
    pub notifications_visible: bool,
    pub guard_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeObservation {
    NameResolutionFailed,
    ConnectFailed,
    ConnectTimedOut,
    TlsFailed,
    HttpStatus(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityPreflightCode {
    Ready,
    NoValidatedNetwork,
    RestrictedMeteredBackground,
    ForegroundGuardUnavailable,
    NameResolutionFailed,
    ConnectFailed,
    ConnectTimedOut,
    TlsFailed,
    HttpFailed,
    ProbeBusy,
}

impl ConnectivityPreflightCode {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NoValidatedNetwork => "no_validated_network",
            Self::RestrictedMeteredBackground => "restricted_metered_background",
            Self::ForegroundGuardUnavailable => "foreground_guard_unavailable",
            Self::NameResolutionFailed => "name_resolution_failed",
            Self::ConnectFailed => "connect_failed",
            Self::ConnectTimedOut => "connect_timed_out",
            Self::TlsFailed => "tls_failed",
            Self::HttpFailed => "http_failed",
            Self::ProbeBusy => "probe_busy",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::NoValidatedNetwork
                | Self::RestrictedMeteredBackground
                | Self::ForegroundGuardUnavailable
                | Self::NameResolutionFailed
                | Self::ConnectFailed
                | Self::ConnectTimedOut
                | Self::HttpFailed
                | Self::ProbeBusy
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectivityPreflight {
    pub code: ConnectivityPreflightCode,
    pub retryable: bool,
}

impl ConnectivityPreflight {
    fn from_code(code: ConnectivityPreflightCode) -> Self {
        Self {
            retryable: code.retryable(),
            code,
        }
    }
}

pub fn classify(
    snapshot: Option<AndroidNetworkSnapshot>,
    observation: Option<ProbeObservation>,
) -> ConnectivityPreflight {
    if let Some(snapshot) = snapshot {
        if !snapshot.guard_ready {
            return ConnectivityPreflight::from_code(
                ConnectivityPreflightCode::ForegroundGuardUnavailable,
            );
        }
        if !snapshot.active_network
            || !snapshot.internet_capability
            || !snapshot.validated_capability
        {
            return ConnectivityPreflight::from_code(ConnectivityPreflightCode::NoValidatedNetwork);
        }
        if snapshot.metered && snapshot.restrict_background == RestrictBackgroundStatus::Enabled {
            return ConnectivityPreflight::from_code(
                ConnectivityPreflightCode::RestrictedMeteredBackground,
            );
        }
    }
    let code = match observation {
        Some(ProbeObservation::NameResolutionFailed) => {
            ConnectivityPreflightCode::NameResolutionFailed
        }
        Some(ProbeObservation::ConnectFailed) => ConnectivityPreflightCode::ConnectFailed,
        Some(ProbeObservation::ConnectTimedOut) => ConnectivityPreflightCode::ConnectTimedOut,
        Some(ProbeObservation::TlsFailed) => ConnectivityPreflightCode::TlsFailed,
        Some(ProbeObservation::HttpStatus(500..=599)) => ConnectivityPreflightCode::HttpFailed,
        Some(ProbeObservation::HttpStatus(_)) => ConnectivityPreflightCode::Ready,
        None => ConnectivityPreflightCode::ProbeBusy,
    };
    ConnectivityPreflight::from_code(code)
}

/// Process-local limiter. A permit is released by Drop even on an early probe error.
pub struct ProbeLimiter {
    in_flight: AtomicUsize,
}

impl ProbeLimiter {
    pub const fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
        }
    }

    pub fn try_acquire(&self) -> Option<ProbePermit<'_>> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= MAX_CONCURRENT_PROBES {
                return None;
            }
            match self.in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(ProbePermit { limiter: self }),
                Err(next) => current = next,
            }
        }
    }
}

impl Default for ProbeLimiter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ProbePermit<'a> {
    limiter: &'a ProbeLimiter,
}

impl Drop for ProbePermit<'_> {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_snapshot() -> AndroidNetworkSnapshot {
        AndroidNetworkSnapshot {
            active_network: true,
            internet_capability: true,
            validated_capability: true,
            metered: false,
            restrict_background: RestrictBackgroundStatus::Disabled,
            notifications_visible: true,
            guard_ready: true,
        }
    }

    #[test]
    fn connectivity_preflight_selects_distinct_oauth_turn_and_refresh_targets() {
        assert_eq!(
            target_for(
                ConnectivityProvider::Claude,
                ConnectivityPurpose::OAuthStart
            ),
            ProbeTarget::ClaudeOAuth
        );
        assert_eq!(
            target_for(ConnectivityProvider::Claude, ConnectivityPurpose::TurnStart),
            ProbeTarget::ClaudeInference
        );
        assert_eq!(
            target_for(ConnectivityProvider::Codex, ConnectivityPurpose::Refresh),
            ProbeTarget::CodexOAuth
        );
        assert_eq!(
            target_for(ConnectivityProvider::Codex, ConnectivityPurpose::TurnStart),
            ProbeTarget::CodexInference
        );
    }

    #[test]
    fn credential_revoke_preflight_selects_reviewed_provider_target() {
        assert_eq!(
            target_for(
                ConnectivityProvider::Claude,
                ConnectivityPurpose::CredentialRevoke
            ),
            ProbeTarget::ClaudeRevoke
        );
        assert_eq!(
            target_for(
                ConnectivityProvider::Codex,
                ConnectivityPurpose::CredentialRevoke
            ),
            ProbeTarget::CodexRevoke
        );
        assert_ne!(
            target_for(
                ConnectivityProvider::Codex,
                ConnectivityPurpose::CredentialRevoke
            ),
            target_for(ConnectivityProvider::Codex, ConnectivityPurpose::OAuthStart)
        );
    }

    #[test]
    fn connectivity_preflight_requires_validated_network_before_transport() {
        let mut snapshot = ready_snapshot();
        snapshot.validated_capability = false;
        assert_eq!(
            classify(Some(snapshot), Some(ProbeObservation::HttpStatus(204))).code,
            ConnectivityPreflightCode::NoValidatedNetwork
        );
    }

    #[test]
    fn connectivity_preflight_marks_metered_restriction_only_from_snapshot() {
        let mut snapshot = ready_snapshot();
        snapshot.metered = true;
        snapshot.restrict_background = RestrictBackgroundStatus::Enabled;
        assert_eq!(
            classify(Some(snapshot), Some(ProbeObservation::HttpStatus(204))).code,
            ConnectivityPreflightCode::RestrictedMeteredBackground
        );
        snapshot.restrict_background = RestrictBackgroundStatus::Whitelisted;
        assert_eq!(
            classify(Some(snapshot), Some(ProbeObservation::HttpStatus(204))).code,
            ConnectivityPreflightCode::Ready
        );
    }

    #[test]
    fn connectivity_preflight_classifies_transport_observations_without_error_text() {
        assert_eq!(
            classify(None, Some(ProbeObservation::NameResolutionFailed)).code,
            ConnectivityPreflightCode::NameResolutionFailed
        );
        assert_eq!(
            classify(None, Some(ProbeObservation::ConnectTimedOut)).code,
            ConnectivityPreflightCode::ConnectTimedOut
        );
        assert_eq!(
            classify(None, Some(ProbeObservation::TlsFailed)).code,
            ConnectivityPreflightCode::TlsFailed
        );
        assert_eq!(
            classify(None, Some(ProbeObservation::HttpStatus(302))).code,
            ConnectivityPreflightCode::Ready
        );
        assert_eq!(
            classify(None, Some(ProbeObservation::HttpStatus(503))).code,
            ConnectivityPreflightCode::HttpFailed
        );
    }

    #[test]
    fn connectivity_preflight_limits_process_concurrency() {
        let limiter = ProbeLimiter::new();
        let first = limiter.try_acquire();
        let second = limiter.try_acquire();
        let third = limiter.try_acquire();
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(third.is_none());
        drop(first);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn connectivity_preflight_public_codes_have_no_transport_detail() {
        for code in [
            ConnectivityPreflightCode::NameResolutionFailed,
            ConnectivityPreflightCode::ConnectFailed,
            ConnectivityPreflightCode::TlsFailed,
            ConnectivityPreflightCode::HttpFailed,
        ] {
            assert!(!code.wire().contains('.'));
            assert!(!code.wire().contains('/'));
            assert!(!code.wire().contains(':'));
        }
    }
}
