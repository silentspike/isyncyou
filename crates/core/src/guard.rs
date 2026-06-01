//! Two-sided mass-delete guard (plan §5, §23).
//!
//! Before a sync run applies a batch of destructive operations (deletes /
//! overwrites) in **either** direction, the guard checks whether the batch looks
//! catastrophic — e.g. a bug or a mis-mounted folder made half the library
//! "disappear". If so the job is blocked and the user must confirm, rather than
//! silently propagating the destruction.
//!
//! Pure and deterministic; the engine supplies the counts.

use std::fmt;

/// Which way the destructive batch flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    LocalToCloud,
    CloudToLocal,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::LocalToCloud => write!(f, "local→cloud"),
            Direction::CloudToLocal => write!(f, "cloud→local"),
        }
    }
}

/// Thresholds for the guard.
#[derive(Debug, Clone)]
pub struct DeleteGuard {
    /// Hard cap on destructive ops in one batch, regardless of library size.
    pub max_absolute: usize,
    /// Block when the destructive fraction of all tracked items reaches this.
    pub max_fraction: f64,
    /// Only apply the fraction rule when at least this many items are tracked
    /// (so deleting a 2-of-2 tiny folder is not blocked).
    pub fraction_min_total: usize,
}

impl Default for DeleteGuard {
    fn default() -> Self {
        DeleteGuard {
            max_absolute: 1000,
            max_fraction: 0.5,
            fraction_min_total: 10,
        }
    }
}

/// The guard's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Proceed,
    Block { reason: String },
}

impl GuardVerdict {
    pub fn is_blocked(&self) -> bool {
        matches!(self, GuardVerdict::Block { .. })
    }
}

impl DeleteGuard {
    /// Evaluate a batch of `destructive` operations (deletes + replacements)
    /// against `total_tracked` items flowing in `direction`.
    pub fn evaluate(
        &self,
        destructive: usize,
        total_tracked: usize,
        direction: Direction,
    ) -> GuardVerdict {
        if destructive == 0 {
            return GuardVerdict::Proceed;
        }
        if destructive >= self.max_absolute {
            return GuardVerdict::Block {
                reason: format!(
                    "{direction}: {destructive} destructive operations \
                     reach the absolute limit of {} — confirm to proceed",
                    self.max_absolute
                ),
            };
        }
        if total_tracked >= self.fraction_min_total {
            let fraction = destructive as f64 / total_tracked as f64;
            if fraction >= self.max_fraction {
                return GuardVerdict::Block {
                    reason: format!(
                        "{direction}: {destructive}/{total_tracked} items \
                         ({:.0}%) would be removed, at or above the {:.0}% limit \
                         — confirm to proceed",
                        fraction * 100.0,
                        self.max_fraction * 100.0
                    ),
                };
            }
        }
        GuardVerdict::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::Direction::*;
    use super::*;

    #[test]
    fn nothing_to_delete_proceeds() {
        assert_eq!(
            DeleteGuard::default().evaluate(0, 1000, LocalToCloud),
            GuardVerdict::Proceed
        );
    }

    #[test]
    fn normal_batch_proceeds() {
        // 5 of 1000 = 0.5% — well under both limits
        assert_eq!(
            DeleteGuard::default().evaluate(5, 1000, CloudToLocal),
            GuardVerdict::Proceed
        );
    }

    #[test]
    fn absolute_limit_blocks() {
        let v = DeleteGuard::default().evaluate(1000, 1_000_000, LocalToCloud);
        assert!(v.is_blocked());
        if let GuardVerdict::Block { reason } = v {
            assert!(reason.contains("absolute limit"));
            assert!(reason.contains("local→cloud"));
        }
    }

    #[test]
    fn fraction_limit_blocks_when_half_vanishes() {
        // 6 of 10 = 60% >= 50%
        let v = DeleteGuard::default().evaluate(6, 10, CloudToLocal);
        assert!(v.is_blocked());
        if let GuardVerdict::Block { reason } = v {
            assert!(reason.contains("60%"));
            assert!(reason.contains("cloud→local"));
        }
    }

    #[test]
    fn tiny_total_not_subject_to_fraction_rule() {
        // delete all 3 of a 3-item folder: 100% but total < fraction_min_total(10),
        // and under the absolute cap -> allowed
        assert_eq!(
            DeleteGuard::default().evaluate(3, 3, LocalToCloud),
            GuardVerdict::Proceed
        );
    }

    #[test]
    fn exactly_at_fraction_blocks() {
        // 50 of 100 = exactly 50%
        assert!(DeleteGuard::default()
            .evaluate(50, 100, LocalToCloud)
            .is_blocked());
        // 49 of 100 = 49% -> proceed
        assert_eq!(
            DeleteGuard::default().evaluate(49, 100, LocalToCloud),
            GuardVerdict::Proceed
        );
    }

    #[test]
    fn custom_thresholds() {
        let g = DeleteGuard {
            max_absolute: 3,
            max_fraction: 0.9,
            fraction_min_total: 2,
        };
        assert!(g.evaluate(3, 100, LocalToCloud).is_blocked()); // hits absolute=3
        assert_eq!(g.evaluate(2, 100, LocalToCloud), GuardVerdict::Proceed); // 2% < 90%, <3
    }
}
