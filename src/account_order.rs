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
        Self {
            lapsed,
            locked: until.is_some(),
            reset: until
                .or(usage.weekly_reset)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
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
