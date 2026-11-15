//! Read-backs after an app rule starts or ends.
//!
//! The write that starts or ends a rule is confirmed by one read right after it, and that is
//! not enough: on `0x029F` (2026-11-15, Modern Standby, a game just closed) the tray wrote
//! Balanced, its read-back passed, and the EC held Performance a minute later. Nothing read
//! the device again, so the tray went on believing Balanced. These are the reads that follow:
//! +3 s and +15 s after each transition, like the wake checks; the caller writes only when a
//! read shows drift.

use std::time::{Duration, Instant};

/// When the read-backs run, after the transition.
pub const DELAYS: [(Duration, &str); 2] = [
    (Duration::from_secs(3), "app rule +3s"),
    (Duration::from_secs(15), "app rule +15s"),
];

/// The read-backs still to run for the latest rule transition.
#[derive(Debug, Default)]
pub struct RuleChecks {
    due: Vec<(Instant, &'static str)>,
}

impl RuleChecks {
    /// A rule started or ended at `now`. Checks left from an earlier transition are dropped:
    /// they would compare the device with an intent that has since changed anyway.
    pub fn schedule(&mut self, now: Instant) {
        self.due = DELAYS
            .iter()
            .map(|(after, why)| (now + *after, *why))
            .collect();
    }

    /// The earliest check due at `now`, removed, as the reason to log with it.
    pub fn take_due(&mut self, now: Instant) -> Option<&'static str> {
        // Kept in time order by `schedule`, so the first is the earliest.
        match self.due.first() {
            Some((at, _)) if now >= *at => Some(self.due.remove(0).1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transition_schedules_a_read_back_at_3_and_15_seconds() {
        let t0 = Instant::now();
        let mut checks = RuleChecks::default();
        checks.schedule(t0);
        assert_eq!(checks.take_due(t0 + Duration::from_secs(2)), None);
        assert_eq!(
            checks.take_due(t0 + Duration::from_secs(3)),
            Some("app rule +3s")
        );
        assert_eq!(checks.take_due(t0 + Duration::from_secs(4)), None);
        assert_eq!(
            checks.take_due(t0 + Duration::from_secs(15)),
            Some("app rule +15s")
        );
        assert_eq!(checks.take_due(t0 + Duration::from_secs(60)), None);
    }

    #[test]
    fn a_late_tick_runs_each_check_once_in_order() {
        // The loop can run late (standby); both checks are then due at once.
        let t0 = Instant::now();
        let mut checks = RuleChecks::default();
        checks.schedule(t0);
        let late = t0 + Duration::from_secs(40);
        assert_eq!(checks.take_due(late), Some("app rule +3s"));
        assert_eq!(checks.take_due(late), Some("app rule +15s"));
        assert_eq!(checks.take_due(late), None);
    }

    #[test]
    fn a_new_transition_replaces_the_pending_checks() {
        let t0 = Instant::now();
        let mut checks = RuleChecks::default();
        checks.schedule(t0);
        assert_eq!(
            checks.take_due(t0 + Duration::from_secs(3)),
            Some("app rule +3s")
        );
        let t1 = t0 + Duration::from_secs(10);
        checks.schedule(t1);
        // The first transition's +15 s check is gone; the second's start over.
        assert_eq!(
            checks.take_due(t0 + Duration::from_secs(15)),
            Some("app rule +3s")
        );
        assert_eq!(checks.take_due(t1 + Duration::from_secs(14)), None);
        assert_eq!(
            checks.take_due(t1 + Duration::from_secs(15)),
            Some("app rule +15s")
        );
    }
}
