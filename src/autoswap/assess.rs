//! Per-window and per-account assessment (§3.3): lower bounds, estimates,
//! the blind projection, runway, evidence, verification and tiers.

use std::cmp::Ordering;

use chrono::{DateTime, Duration, Utc};

use super::memory::{Latch, SwapMemory};
use super::policy::*;
use super::runway::{est_window, time_to};
use super::{AccountIn, Evidence, Policy, Tier, WindowId, WindowIn};
use crate::providers::trait_def::Money;

/// What the engine needs besides the account itself.
pub struct Ctx<'a> {
    pub now: DateTime<Utc>,
    pub policy: &'a Policy,
    pub memory: &'a SwapMemory,
    /// Detected sleep intervals `(from, to)` in unix secs (excluded from the
    /// blind age).
    pub sleep: &'a [(i64, i64)],
}

impl Ctx<'_> {
    fn now_ts(&self) -> i64 {
        self.now.timestamp()
    }

    fn limit(&self, w: WindowId) -> f64 {
        match w {
            WindowId::Session => self.policy.limit_session(),
            WindowId::Weekly => self.policy.limit_weekly(),
        }
    }
}

/// One window's assessment.
#[derive(Debug, Clone)]
pub struct WinAssess {
    pub id: WindowId,
    /// The lower bound: the latest successful reading's pct.
    pub lb: f64,
    pub resets_at: Option<DateTime<Utc>>,
    /// The reading predates the window's reset (`resets_at ≤ now`): its pct
    /// is an upper bound only.
    pub pending: bool,
    /// `lb ≥ BLOCKED_PCT` and not pending.
    pub blocked: bool,
    /// `lb ≥ DRAIN_EXIT_PCT` and not pending.
    pub drain_eligible: bool,
    /// Drift estimate (room claims only).
    pub est: f64,
    /// Blind projection (active, blind in band only).
    pub proj: Option<f64>,
    pub limit: f64,
    /// Whether the window can block before it resets.
    pub binds: bool,
    /// Floored rate (`max(rate, DEFAULT)`).
    pub floored: f64,
}

impl WinAssess {
    /// The value runway is measured from: the projection when there is one,
    /// else the estimate.
    fn current(&self) -> f64 {
        self.proj.map_or(self.est, |p| p.max(self.est))
    }
}

/// One account's assessment.
#[derive(Debug, Clone)]
pub struct Assessed {
    pub key: String,
    pub is_active: bool,
    pub has_reading: bool,
    pub windows: Vec<WinAssess>,
    pub lapsed: bool,
    /// lb-blocked in a non-pending window, or lapsed.
    pub blocked: bool,
    /// Max reset of the blocked windows; `None` when unknown (or lapsed).
    pub blocked_until: Option<DateTime<Utc>>,
    /// Minutes until the binding windows reach their limit (floored rate).
    pub soft_runway: f64,
    /// Minutes until the binding windows reach `DRAIN_EXIT_PCT` (floored).
    pub hard_runway: f64,
    pub evidence: Evidence,
    /// Never left, or read at least `POST_LEAVE_SETTLE_S` after we left it.
    pub post_leave_ok: bool,
    /// Some window is reset-pending.
    pub any_pending: bool,
    /// I5.
    pub verified: bool,
    /// §3.4 `servable`.
    pub servable: bool,
    pub tier: Tier,
    /// Ready apart from freshness (the arms pick from these; the evidence
    /// post-pass decides whether a read is needed first).
    pub ready_basis: bool,
    pub trigger_latched: bool,
    pub drained_latched: bool,
    /// Active only: last reading in band and older than `BLIND_AFTER_S`.
    pub blind_in_band: bool,
    pub credits: Option<Money>,
    pub attempted: bool,
    pub unverified: bool,
    pub in_backoff: bool,
    pub weekly_reset: Option<DateTime<Utc>>,
    pub fetched_at: Option<i64>,
}

impl Assessed {
    /// The highest lower bound over binding, non-pending windows, relative
    /// to nothing (raw pct). Used for logs.
    pub fn max_lb(&self) -> f64 {
        self.windows
            .iter()
            .filter(|w| !w.pending)
            .map(|w| w.lb)
            .fold(0.0, f64::max)
    }

    pub fn max_est(&self) -> f64 {
        self.windows
            .iter()
            .filter(|w| !w.pending)
            .map(|w| w.est)
            .fold(0.0, f64::max)
    }

    pub fn max_proj(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter_map(|w| w.proj)
            .fold(None, |a, p| Some(a.map_or(p, |m: f64| m.max(p))))
    }

    /// Some binding window has a lower bound at or above the drain exit.
    pub fn lb_drain_eligible(&self) -> bool {
        self.windows.iter().any(|w| w.binds && w.drain_eligible)
    }

    /// The window that drove the account to its limit (for latches): the
    /// blocked window, else the binding window closest to (or most over) its
    /// limit.
    pub fn binding_window(&self) -> Option<&WinAssess> {
        if let Some(w) = self.windows.iter().find(|w| w.blocked) {
            return Some(w);
        }
        self.windows
            .iter()
            .filter(|w| !w.pending)
            .max_by(|a, b| (a.current() - a.limit).total_cmp(&(b.current() - b.limit)))
    }

    /// A latch for leaving this account now.
    pub fn latch(&self, now: DateTime<Utc>) -> Option<Latch> {
        let w = self.binding_window()?;
        Some(Latch {
            window: w.id,
            resets_at: w.resets_at.map(|r| r.timestamp()),
            pct: w.lb,
            at: now.timestamp(),
        })
    }
}

/// Seconds of `[from, to]` covered by the detected sleep intervals.
fn sleep_overlap(sleep: &[(i64, i64)], from: i64, to: i64) -> i64 {
    sleep
        .iter()
        .map(|&(a, b)| (b.min(to) - a.max(from)).max(0))
        .sum()
}

fn minutes(d: Duration) -> f64 {
    d.num_milliseconds() as f64 / 60_000.0
}

/// A latch holds while the account is still in the epoch it was set in.
fn latch_holds(latch: &Latch, a: &AccountIn, now: DateTime<Utc>) -> bool {
    let w = match latch.window {
        WindowId::Session => a.session.as_ref(),
        WindowId::Weekly => a.weekly.as_ref(),
    };
    let Some(w) = w else { return false };
    if w.resets_at.is_some_and(|r| r <= now) {
        return false; // its reset passed: a new epoch
    }
    if w.resets_at.map(|r| r.timestamp()) != latch.resets_at {
        return false; // a different epoch
    }
    w.pct + 1e-9 >= latch.pct // a lower reading starts a new epoch
}

/// Assess `a` (the active account when `is_active`).
pub fn assess(a: &AccountIn, is_active: bool, ctx: &Ctx) -> Assessed {
    let now = ctx.now;
    let now_ts = ctx.now_ts();
    let left_at = if is_active {
        None
    } else {
        ctx.memory.left_at(&a.key)
    };
    let has_reading = a.fetched_at.is_some() && (a.session.is_some() || a.weekly.is_some());
    let age_s = a.fetched_at.map(|f| (now_ts - f).max(0));
    let age_min = age_s.unwrap_or(0) as f64 / 60.0;
    let post_leave_ok = match (left_at, a.fetched_at) {
        (None, _) => true,
        (Some(l), Some(f)) => f >= l + POST_LEAVE_SETTLE_S,
        (Some(_), None) => false,
    };

    // Blind in band (active only): the last reading is in the band and old.
    let blind_age_min = a.fetched_at.map(|f| {
        let raw = now_ts - f;
        (raw - sleep_overlap(ctx.sleep, f, now_ts)).max(0) as f64 / 60.0
    });

    let mut windows = Vec::new();
    for (id, w) in [
        (WindowId::Session, a.session.as_ref()),
        (WindowId::Weekly, a.weekly.as_ref()),
    ] {
        let Some(WindowIn { pct, resets_at }) = w.cloned() else {
            continue;
        };
        let pending = resets_at.is_some_and(|r| r <= now);
        let limit = ctx.limit(id);
        let own = a.rate.effective(id);
        let floored = a.rate.floored(id);
        let est = if pending {
            pct
        } else if is_active {
            // Awake age: no usage accrues while the machine sleeps.
            let awake = blind_age_min.unwrap_or(age_min);
            est_window(pct, own, awake.min(EST_MAX_AGE_S as f64 / 60.0), SAFETY)
        } else if let (Some(l), Some(f)) = (left_at, a.fetched_at) {
            // Carry the drift forward to when we left it (B10).
            est_window(pct, own, ((l - f).max(0)) as f64 / 60.0, 1.0)
        } else {
            pct
        };
        windows.push(WinAssess {
            id,
            lb: pct,
            resets_at,
            pending,
            blocked: pct >= BLOCKED_PCT && !pending,
            drain_eligible: pct >= DRAIN_EXIT_PCT && !pending,
            est,
            proj: None,
            limit,
            binds: true,
            floored,
        });
    }

    let blind_in_band = is_active
        && has_reading
        && age_s.is_some_and(|s| s > BLIND_AFTER_S)
        && windows
            .iter()
            .any(|w| !w.pending && !w.blocked && w.lb >= w.limit - BLIND_BAND);
    if blind_in_band {
        let bm = blind_age_min.unwrap_or(0.0);
        for w in windows.iter_mut().filter(|w| !w.pending) {
            let blind_rate = match w.id {
                WindowId::Session => BLIND_RATE_SESSION,
                WindowId::Weekly => BLIND_RATE_WEEKLY,
            };
            let r = a.rate.peak(w.id).max(blind_rate).max(w.floored);
            w.proj = Some(est_window(w.lb, r, bm, SAFETY));
        }
    }

    // A window that resets before it could block doesn't bind (B4).
    for w in windows.iter_mut() {
        if w.pending {
            w.binds = false;
            continue;
        }
        if let Some(r) = w.resets_at {
            let t = time_to(w.current(), DRAIN_EXIT_PCT, w.floored);
            if t.is_finite() && minutes(r - now) < t {
                w.binds = false;
            }
        }
    }

    let blocked_windows: Vec<&WinAssess> = windows.iter().filter(|w| w.blocked).collect();
    let blocked = a.lapsed || !blocked_windows.is_empty();
    let blocked_until = if a.lapsed
        || blocked_windows.is_empty()
        || blocked_windows.iter().any(|w| w.resets_at.is_none())
    {
        None
    } else {
        blocked_windows.iter().filter_map(|w| w.resets_at).max()
    };
    let soft_runway = windows
        .iter()
        .filter(|w| w.binds)
        .map(|w| time_to(w.current(), w.limit, w.floored))
        .fold(f64::INFINITY, f64::min);
    let hard_runway = windows
        .iter()
        .filter(|w| w.binds)
        .map(|w| time_to(w.current(), DRAIN_EXIT_PCT, w.floored))
        .fold(f64::INFINITY, f64::min);
    let any_pending = windows.iter().any(|w| w.pending);
    let pending_would_block = windows.iter().any(|w| w.pending && w.lb >= BLOCKED_PCT);

    let evidence = match age_s {
        _ if !post_leave_ok => Evidence::Stale,
        Some(s) if s <= REAL_MAX_AGE_S => Evidence::Real,
        Some(s) if s <= EST_MAX_AGE_S => Evidence::Estimated,
        _ => Evidence::Stale,
    };
    let verified = has_reading
        && post_leave_ok
        && !any_pending
        && !a.unverified
        && age_s.is_some_and(|s| s <= TARGET_VERIFY_MAX_AGE_S);
    let servable = !blocked && has_reading && post_leave_ok && !any_pending;

    let trigger_latched = !is_active
        && ctx
            .memory
            .trigger_latch(&a.key)
            .is_some_and(|l| latch_holds(l, a, now))
        && !(evidence == Evidence::Real
            && windows
                .iter()
                .filter(|w| !w.pending)
                .all(|w| w.lb <= ctx.policy.trigger - LATCH_CLEAR_PTS));
    let drained_latched = !is_active
        && ctx
            .memory
            .drained_latch(&a.key)
            .is_some_and(|l| latch_holds(l, a, now));

    let credit_capable = !a.lapsed && a.credits.is_some() && ctx.policy.use_credits;
    let all_pending = !windows.is_empty() && windows.iter().all(|w| w.pending);

    let (tier, ready_basis) = if is_active {
        if !has_reading {
            (Tier::Unknown, false)
        } else if blocked {
            if credit_capable && !blocked_windows.is_empty() {
                (Tier::Credits, false)
            } else {
                (Tier::Blocked, false)
            }
        } else if all_pending {
            (Tier::Unknown, false)
        } else {
            let over = windows.iter().filter(|w| w.binds).any(|w| {
                w.lb >= w.limit || w.est >= w.limit || w.proj.is_some_and(|p| p >= w.limit)
            });
            // Re-entry to Ready from Draining needs a Real reading under
            // `limit − 1` (B8).
            let was_draining = ctx
                .memory
                .draining
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(&a.key));
            let real_clear = evidence == Evidence::Real
                && windows
                    .iter()
                    .filter(|w| w.binds)
                    .all(|w| w.lb < w.limit - 1.0);
            if over || (was_draining && !real_clear) {
                (Tier::Draining, false)
            } else {
                (Tier::Ready, true)
            }
        }
    } else if !has_reading {
        (Tier::Unknown, false)
    } else if blocked {
        if credit_capable {
            (Tier::Credits, false)
        } else {
            (Tier::Blocked, false)
        }
    } else if pending_would_block {
        (Tier::Unknown, false)
    } else {
        let basis = soft_runway >= MIN_LANDING_MIN && post_leave_ok && !trigger_latched;
        if basis && verified {
            (Tier::Ready, true)
        } else {
            (Tier::Draining, basis)
        }
    };

    Assessed {
        key: a.key.clone(),
        is_active,
        has_reading,
        weekly_reset: a.weekly.as_ref().and_then(|w| w.resets_at),
        windows,
        lapsed: a.lapsed,
        blocked,
        blocked_until,
        soft_runway,
        hard_runway,
        evidence,
        post_leave_ok,
        any_pending,
        verified,
        servable,
        tier,
        ready_basis,
        trigger_latched,
        drained_latched,
        blind_in_band,
        credits: a.credits.clone(),
        attempted: a.attempted,
        unverified: a.unverified,
        in_backoff: a.in_backoff,
        fetched_at: a.fetched_at,
    }
}

/// The tier an account ranks in: candidates that are Ready apart from
/// freshness rank with the Ready ones.
fn rank_tier(a: &Assessed) -> Tier {
    if a.ready_basis {
        Tier::Ready
    } else {
        a.tier
    }
}

/// The provider's dominant credits unit: the most common among `accounts`.
pub fn dominant_unit(accounts: &[Assessed]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for c in accounts.iter().filter_map(|a| a.credits.as_ref()) {
        match counts.iter_mut().find(|(u, _)| *u == c.unit) {
            Some(e) => e.1 += 1,
            None => counts.push((c.unit.clone(), 1)),
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(u, _)| u)
}

/// `rank_key` (§3.3) as a comparator: better first. Also used by `auto_pick`,
/// `optimize_now` and `switch` with no selector. Uses `total_cmp` throughout,
/// so it is a total order and never NaN-poisoned.
pub fn rank_cmp(a: &Assessed, b: &Assessed, now: DateTime<Utc>, unit: Option<&str>) -> Ordering {
    let (ta, tb) = (rank_tier(a), rank_tier(b));
    ta.cmp(&tb).then_with(|| match ta {
        Tier::Ready => {
            let horizon = now + Duration::hours(REBALANCE_HORIZON_H);
            let expiring = |x: &Assessed| {
                x.weekly_reset
                    .filter(|r| *r < horizon)
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
            };
            expiring(a)
                .cmp(&expiring(b))
                .then(b.soft_runway.total_cmp(&a.soft_runway))
                .then_with(|| a.key.cmp(&b.key))
        }
        Tier::Draining => b
            .hard_runway
            .total_cmp(&a.hard_runway)
            .then_with(|| a.key.cmp(&b.key)),
        Tier::Credits => {
            let k = |x: &Assessed| {
                let m = x.credits.as_ref();
                let other_unit = m.is_some_and(|m| Some(m.unit.as_str()) != unit);
                (other_unit, -m.map_or(0, |m| m.amount_minor))
            };
            k(a).cmp(&k(b)).then_with(|| a.key.cmp(&b.key))
        }
        Tier::Blocked => a
            .blocked_until
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
            .cmp(&b.blocked_until.unwrap_or(DateTime::<Utc>::MAX_UTC))
            .then_with(|| a.key.cmp(&b.key)),
        Tier::Unknown => a.key.cmp(&b.key),
    })
}
