//! Adaptive request pacer.
//!
//! Project rule (no artificial bandwidth limit): run at **full speed** (zero
//! inter-request delay) until Microsoft returns `429`/`5xx`. Then honor
//! `Retry-After`, back off exponentially when no `Retry-After` is given, and
//! **decay back to full speed** once requests succeed again — so users never
//! mistake throttling for a slow tool or connection.
//!
//! The pacer is pure: [`Pacer::update`] returns the delay the caller should wait
//! before the next request. Tests assert the computed delays without sleeping.

use std::time::Duration;

/// Outcome of a single request, fed back into the [`Pacer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Request succeeded — decay toward full speed.
    Ok,
    /// Throttled/transient — back off; honor `after` (from `Retry-After`) if present.
    Retry { after: Option<Duration> },
}

/// Adaptive pacer with exponential attack and halving decay.
#[derive(Debug, Clone)]
pub struct Pacer {
    current: Duration,
    /// First backoff step when no `Retry-After` is supplied.
    base_backoff: Duration,
    /// Cap for the exponential attack (does not cap an explicit `Retry-After`).
    max_backoff: Duration,
    /// Hard cap applied even to a server-supplied `Retry-After`.
    retry_after_cap: Duration,
}

impl Default for Pacer {
    fn default() -> Self {
        Pacer {
            current: Duration::ZERO,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(16),
            retry_after_cap: Duration::from_secs(300),
        }
    }
}

impl Pacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The delay to wait before the next request right now.
    pub fn current_delay(&self) -> Duration {
        self.current
    }

    /// Feed back a request outcome; returns the new delay before the next request.
    pub fn update(&mut self, outcome: Outcome) -> Duration {
        self.current = match outcome {
            // Decay: halve, snapping to full speed once below the base step.
            Outcome::Ok => {
                let halved = self.current / 2;
                if halved < self.base_backoff {
                    Duration::ZERO
                } else {
                    halved
                }
            }
            // Honor the server's Retry-After (clamped), overriding the curve.
            Outcome::Retry { after: Some(d) } => d.min(self.retry_after_cap),
            // No Retry-After: exponential attack from base, capped.
            Outcome::Retry { after: None } => {
                if self.current.is_zero() {
                    self.base_backoff
                } else {
                    (self.current * 2).min(self.max_backoff)
                }
            }
        };
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_full_speed() {
        let p = Pacer::new();
        assert_eq!(p.current_delay(), Duration::ZERO);
    }

    #[test]
    fn success_keeps_full_speed() {
        let mut p = Pacer::new();
        assert_eq!(p.update(Outcome::Ok), Duration::ZERO);
        assert_eq!(p.update(Outcome::Ok), Duration::ZERO);
    }

    #[test]
    fn backoff_without_retry_after_is_exponential_and_capped() {
        let mut p = Pacer::new();
        assert_eq!(
            p.update(Outcome::Retry { after: None }),
            Duration::from_millis(250)
        );
        assert_eq!(
            p.update(Outcome::Retry { after: None }),
            Duration::from_millis(500)
        );
        assert_eq!(
            p.update(Outcome::Retry { after: None }),
            Duration::from_secs(1)
        );
        // keep hitting it -> caps at max_backoff (16s)
        for _ in 0..10 {
            p.update(Outcome::Retry { after: None });
        }
        assert_eq!(p.current_delay(), Duration::from_secs(16));
    }

    #[test]
    fn retry_after_is_honored_and_capped() {
        let mut p = Pacer::new();
        assert_eq!(
            p.update(Outcome::Retry {
                after: Some(Duration::from_secs(14))
            }),
            Duration::from_secs(14)
        );
        // a huge Retry-After is clamped to the hard cap (300s)
        assert_eq!(
            p.update(Outcome::Retry {
                after: Some(Duration::from_secs(9999))
            }),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn decays_back_to_full_speed_after_throttle() {
        let mut p = Pacer::new();
        p.update(Outcome::Retry {
            after: Some(Duration::from_secs(8)),
        });
        let mut seen = vec![p.current_delay()];
        for _ in 0..6 {
            seen.push(p.update(Outcome::Ok));
        }
        // monotonically non-increasing, and reaches full speed again
        assert!(
            seen.windows(2).all(|w| w[1] <= w[0]),
            "delays should not increase: {seen:?}"
        );
        assert_eq!(p.current_delay(), Duration::ZERO);
    }
}
