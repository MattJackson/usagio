//! The auto-swap v2 constants (design §3.7) and the per-cycle [`Policy`].
//!
//! Every tunable number the engine uses lives here, named once, so a test or
//! a review can find the whole policy in one place.

/// The single definition of "blocked": a window at or above this percent.
pub const BLOCKED_PCT: f64 = 99.5;
/// Drain exit: leaving a Draining account needs a reading with `lb ≥ 99`
/// (never a projection).
pub const DRAIN_EXIT_PCT: f64 = 99.0;
/// A candidate needs this many minutes of runway (floored rate) to be Ready.
pub const MIN_LANDING_MIN: f64 = 15.0;
/// Drain targets need at least this much hard runway…
pub const MIN_DRAIN_LANDING_MIN: f64 = 5.0;
/// …and this many minutes more than the active account (the real-gain rule).
pub const DRAIN_GAIN_MIN: f64 = 5.0;
/// "Has room" readings are `Real` up to this age (seconds)…
pub const REAL_MAX_AGE_S: i64 = 180;
/// …`Estimated` up to this age, `Stale` beyond it. Also caps the drift.
pub const EST_MAX_AGE_S: i64 = 600;
/// Drift multiplier on estimates and projections.
pub const SAFETY: f64 = 1.25;
/// The blind projection applies when the last reading was within this many
/// points of the limit…
pub const BLIND_BAND: f64 = 5.0;
/// …and is older than this (seconds).
pub const BLIND_AFTER_S: i64 = 120;
/// Worst plausible blind rate, pct/min (session, weekly).
pub const BLIND_RATE_SESSION: f64 = 2.0;
pub const BLIND_RATE_WEEKLY: f64 = 0.67;
/// Request budget (§5.7): verification reads per hour per provider.
pub const VERIFY_PER_HOUR: usize = 20;
/// Total usage requests (active + verification) per 60 s per provider.
pub const PROVIDER_REQ_PER_MIN: usize = 3;
/// No verification reads for this long after a 429 on any token (except
/// reset-pending reads).
pub const STORM_GUARD_S: i64 = 60;
/// Floor and no-data default rate, pct/min (session, weekly).
pub const DEFAULT_RATE_SESSION: f64 = 1.0;
pub const DEFAULT_RATE_WEEKLY: f64 = 0.25;
/// Rates: samples from the last 30 min…
pub const RATE_WINDOW_MIN: i64 = 30;
/// …skipping intervals longer than this (sleep, downtime)…
pub const RATE_GAP_MAX_MIN: i64 = 15;
/// …and the peak is the highest rate over 5-minute spans in the last 60 min.
pub const PEAK_SPAN_MIN: i64 = 5;
pub const PEAK_WINDOW_MIN: i64 = 60;
/// A target is verified by a reading at most this old (seconds)…
pub const TARGET_VERIFY_MAX_AGE_S: i64 = 120;
/// …taken at least this long after we last left it.
pub const POST_LEAVE_SETTLE_S: i64 = 60;
/// At most this many verification reads per cycle…
pub const MAX_VERIFY_FETCHES: usize = 2;
/// …and at most one per account per this many seconds.
pub const VERIFY_MIN_INTERVAL_S: i64 = 120;
/// Optional moves (Rebalance, Prepare) are at least this far apart.
pub const SWAP_COOLDOWN_S: i64 = 300;
/// A manual pick's first reading within this window re-settles its tier.
pub const MANUAL_PICK_SETTLE_S: i64 = 300;
/// A Real post-leave reading this far under the trigger clears `trigger_left`.
pub const LATCH_CLEAR_PTS: f64 = 5.0;
/// Rebalance only toward a weekly reset sooner than this (hours).
pub const REBALANCE_HORIZON_H: i64 = 6;
/// Active poll floor in the TIGHT band / Draining (seconds).
pub const ACTIVE_TIGHT_FLOOR_S: u64 = 60;
/// Active 429 backoff cap while in band or Draining (seconds).
pub const ACTIVE_BAND_429_CAP_S: u64 = 120;
/// Display hysteresis: relative margin and minimum dwell.
pub const BIND_SWITCH_MARGIN: f64 = 0.20;
pub const BIND_MIN_DWELL_S: i64 = 300;
/// Display override: the other window blocks within this many minutes.
pub const BIND_OVERRIDE_MIN: f64 = 15.0;
/// Display hysteresis in no-rate mode: the other pct must lead by this much.
pub const BIND_NO_RATE_LEAD_PTS: f64 = 3.0;
/// Latch entries are pruned after this many days at the latest.
pub const LATCH_MAX_AGE_DAYS: i64 = 8;
/// Below this a rate is treated as zero (division guard).
pub const RATE_EPSILON: f64 = 1e-9;
/// An lb-blocked active account is fetched again at the latest after this
/// long (v1's `MAX_LOCKED_STALENESS`).
pub const MAX_LOCKED_STALENESS_S: i64 = 12 * 3600;

/// The per-cycle policy: the user's settings plus the switches that turn the
/// engine off.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Auto-swap trigger percent (Settings ▸ Auto-swap).
    pub trigger: f64,
    /// Session ceiling (`usagio watch --ceiling`); only sets `limit_session`.
    pub session_ceiling: f64,
    /// Settings ▸ Auto-swap ▸ "Use credits when every plan is used up".
    pub use_credits: bool,
    /// Auto-swap is on.
    pub enabled: bool,
    /// The provider's env override is set (a switch would be ignored).
    pub env_override: bool,
}

impl Policy {
    pub fn new(trigger: f64, session_ceiling: f64) -> Self {
        Policy {
            trigger,
            session_ceiling: session_ceiling.min(trigger),
            use_credits: false,
            enabled: trigger <= 100.0,
            env_override: false,
        }
    }

    /// `limit_session = min(trigger, ceiling)`.
    pub fn limit_session(&self) -> f64 {
        self.trigger.min(self.session_ceiling)
    }

    /// `limit_weekly = trigger`.
    pub fn limit_weekly(&self) -> f64 {
        self.trigger
    }
}
