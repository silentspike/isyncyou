//! Mobile transfer policy (#onedrive-mobile 0.8): a pure decision on whether a NEW
//! download / materialization may START, given the user's transfer policy and the
//! current device state.
//!
//! It never touches existing files — a blocked decision only prevents starting new
//! work. The low-storage rule is a device-protection floor (an OS safety floor), NOT a
//! user storage quota: below it, new downloads stop but everything already on disk is
//! kept. The device state (metered network, charging, free bytes) is supplied by the
//! caller — Android provides it on mobile; the desktop daemon passes unmetered/always-
//! on. Pure and deterministic so the enforcement rules are exhaustively unit-tested; the
//! transfer machinery (Phase 3/4) calls [`evaluate`] before each new download.

use crate::config::SyncConfig;

/// Why a new download/materialization is not allowed to start right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyBlock {
    /// Free space is below the device-protection floor. Existing files are kept; only
    /// NEW downloads stop.
    StorageFloor,
    /// The network is metered and the user restricted transfers to Wi-Fi.
    MeteredNetwork,
    /// The device is not charging and the user restricted transfers to charging.
    NotCharging,
}

/// The decision for starting one new transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allowed,
    Blocked(PolicyBlock),
}

impl PolicyDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PolicyDecision::Allowed)
    }

    /// A stable machine tag for the decision (for the API / UI copy), or `"allowed"`.
    pub fn reason(&self) -> &'static str {
        match self {
            PolicyDecision::Allowed => "allowed",
            PolicyDecision::Blocked(PolicyBlock::StorageFloor) => "storage_floor",
            PolicyDecision::Blocked(PolicyBlock::MeteredNetwork) => "metered_network",
            PolicyDecision::Blocked(PolicyBlock::NotCharging) => "not_charging",
        }
    }
}

/// Current device transfer conditions, supplied by the platform layer.
#[derive(Debug, Clone, Copy)]
pub struct DeviceState {
    /// The active network is metered (mobile data / a metered Wi-Fi). Desktop = `false`.
    pub metered: bool,
    /// The device is charging. Desktop = `true` (always powered).
    pub charging: bool,
    /// Free bytes on the volume that would receive the download.
    pub free_bytes: u64,
}

impl DeviceState {
    /// The desktop / always-on baseline: unmetered, powered, plenty of space. Handy for
    /// callers that have no platform signals (the policy then only gates on the floor).
    pub fn always_on(free_bytes: u64) -> Self {
        DeviceState {
            metered: false,
            charging: true,
            free_bytes,
        }
    }
}

/// Decide whether a NEW download/materialization may start. Order: the device-protection
/// storage floor is checked first (a hard safety stop), then the user's network and power
/// policies. Never affects existing files.
pub fn evaluate(cfg: &SyncConfig, state: &DeviceState) -> PolicyDecision {
    if state.free_bytes < cfg.min_free_bytes {
        return PolicyDecision::Blocked(PolicyBlock::StorageFloor);
    }
    if cfg.wifi_only && state.metered {
        return PolicyDecision::Blocked(PolicyBlock::MeteredNetwork);
    }
    if cfg.charging_only && !state.charging {
        return PolicyDecision::Blocked(PolicyBlock::NotCharging);
    }
    PolicyDecision::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SyncConfig {
        SyncConfig::default() // min_free_bytes = 256 MiB, wifi_only/charging_only off
    }

    // --- AC1: low-storage floor stops new downloads, keeps existing --------------
    #[test]
    fn below_the_storage_floor_blocks_new_downloads() {
        let c = cfg();
        let below = DeviceState::always_on(c.min_free_bytes - 1);
        assert_eq!(
            evaluate(&c, &below),
            PolicyDecision::Blocked(PolicyBlock::StorageFloor)
        );
        // exactly at the floor is still allowed (it is a floor, not a margin)
        let at = DeviceState::always_on(c.min_free_bytes);
        assert!(evaluate(&c, &at).is_allowed());
    }

    #[test]
    fn the_floor_only_gates_new_work_not_a_quota() {
        // evaluate() decides whether to START a download; it returns a decision, it never
        // reports or removes existing data — proving "existing files are kept".
        let c = cfg();
        let d = evaluate(&c, &DeviceState::always_on(0));
        assert_eq!(d, PolicyDecision::Blocked(PolicyBlock::StorageFloor));
        assert_eq!(d.reason(), "storage_floor");
    }

    // --- AC2: Wi-Fi-only + charging-only enforcement -----------------------------
    #[test]
    fn wifi_only_blocks_metered_but_allows_unmetered() {
        let mut c = cfg();
        c.wifi_only = true;
        let plenty = 10 * c.min_free_bytes;
        let metered = DeviceState {
            metered: true,
            charging: true,
            free_bytes: plenty,
        };
        assert_eq!(
            evaluate(&c, &metered),
            PolicyDecision::Blocked(PolicyBlock::MeteredNetwork)
        );
        let wifi = DeviceState {
            metered: false,
            charging: true,
            free_bytes: plenty,
        };
        assert!(evaluate(&c, &wifi).is_allowed());
    }

    #[test]
    fn charging_only_blocks_on_battery_but_allows_charging() {
        let mut c = cfg();
        c.charging_only = true;
        let plenty = 10 * c.min_free_bytes;
        let battery = DeviceState {
            metered: false,
            charging: false,
            free_bytes: plenty,
        };
        assert_eq!(
            evaluate(&c, &battery),
            PolicyDecision::Blocked(PolicyBlock::NotCharging)
        );
        let charging = DeviceState {
            metered: false,
            charging: true,
            free_bytes: plenty,
        };
        assert!(evaluate(&c, &charging).is_allowed());
    }

    #[test]
    fn storage_floor_takes_precedence_over_network_and_power() {
        let mut c = cfg();
        c.wifi_only = true;
        c.charging_only = true;
        // metered + on battery + below floor → the floor wins (device protection first)
        let bad = DeviceState {
            metered: true,
            charging: false,
            free_bytes: c.min_free_bytes - 1,
        };
        assert_eq!(
            evaluate(&c, &bad),
            PolicyDecision::Blocked(PolicyBlock::StorageFloor)
        );
    }

    #[test]
    fn defaults_allow_everything_above_the_floor() {
        let c = cfg(); // wifi_only/charging_only off
        let metered_battery = DeviceState {
            metered: true,
            charging: false,
            free_bytes: c.min_free_bytes,
        };
        assert!(evaluate(&c, &metered_battery).is_allowed());
    }
}
