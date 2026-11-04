//! Shared menu/CLI display ordering, independent of the selected account.
use crate::countdown::{self, AccountUsage, DisplayState};
use chrono::{DateTime, Utc};
use std::cmp::Ordering;

pub(crate) struct Priority {
    lapsed: bool,
    locked: bool,
    reset: DateTime<Utc>,
    headroom: f64,
}

impl Priority {
    pub(crate) fn new(
        lapsed: bool,
        usage: &AccountUsage,
        headroom: f64,
        now: DateTime<Utc>,
    ) -> Self {
        let until = match countdown::compute_display(usage, now) {
            DisplayState::Locked { until, .. } => Some(until),
            _ => None,
        };
        // Blocked with no known unlock: locked, ranked after every account
        // that has a real unlock time.
        let indefinite = countdown::has_indefinite_block(usage);
        Self {
            lapsed,
            locked: until.is_some() || indefinite,
            reset: if indefinite {
                DateTime::<Utc>::MAX_UTC
            } else {
                until
                    .or(usage.weekly_reset)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
            },
            headroom,
        }
    }

    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        self.lapsed.cmp(&other.lapsed).then_with(|| {
            if self.lapsed {
                return Ordering::Equal;
            }
            self.locked
                .cmp(&other.locked)
                .then(self.reset.cmp(&other.reset))
                .then_with(|| {
                    other
                        .headroom
                        .partial_cmp(&self.headroom)
                        .unwrap_or(Ordering::Equal)
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn usage(
        s: Option<f64>,
        sr: Option<DateTime<Utc>>,
        w: Option<f64>,
        wr: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> AccountUsage {
        AccountUsage {
            session_pct: s,
            session_reset: sr,
            weekly_pct: w,
            weekly_reset: wr,
            fetched_at: Some(now),
        }
    }

    #[test]
    fn indefinitely_blocked_account_sorts_after_locked_accounts_with_known_unlock() {
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        // Session maxed with no reset time + weekly locked for 1h: blocked
        // indefinitely, so the 1h weekly unlock is not a real unlock time.
        let indefinite = Priority::new(
            false,
            &usage(
                Some(100.0),
                None,
                Some(100.0),
                Some(now + Duration::hours(1)),
                now,
            ),
            0.0,
            now,
        );
        let known = Priority::new(
            false,
            &usage(
                Some(100.0),
                Some(now + Duration::hours(2)),
                Some(10.0),
                None,
                now,
            ),
            0.0,
            now,
        );
        let usable = Priority::new(
            false,
            &usage(Some(10.0), None, Some(10.0), None, now),
            0.0,
            now,
        );
        assert_eq!(known.cmp(&indefinite), Ordering::Less);
        assert_eq!(usable.cmp(&indefinite), Ordering::Less);
    }
}
