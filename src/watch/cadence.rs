//! Adaptive poll cadence (how long until the next cycle).

use chrono::Utc;

use crate::poll::claude::v2_active_floor_secs;
use crate::rows::Row;
use crate::watch::cycle::CycleOutcome;
use crate::{autoswap, logging};

// --- watch (auto-swap daemon) defaults ---
/// How often the watcher polls, in seconds — the BASE tier of the adaptive
/// cadence when every account is comfortably far from `trigger`. Tightened
/// to 180s from the historical 150s alongside the v0.7.3 four-tier ramp:
/// with the new tiers picking up long before we're near the trigger (60s
/// inside 10pt, 30s inside 5pt), the BASE tier is now the "nothing is even
/// close" case and can breathe.
pub(crate) const WATCH_INTERVAL_SECS: u64 = 180;
/// Upper bound for the poll interval when backing off after a 429.
pub(crate) const WATCH_MAX_INTERVAL_SECS: u64 = 1200;
/// Swap away from the active account when it reaches this utilization.
pub(crate) const TRIGGER_PCT: f64 = 95.0;

/// The ACTIVE account's `Row::max_pct()` (session OR weekly, whichever is
/// tighter), or `None` if there's no active account or it has no data yet —
/// feeds `next_interval`'s adaptive cadence.
///
/// v0.5.13: scoped from "peak across ALL accounts" (`peak_max_pct`) to the
/// active account alone. The cadence backstop exists to catch the moment an
/// account crosses the trigger so the auto-swap fires promptly — but the only
/// account you ever swap AWAY from is the active one, and it's the only one
/// that climbs on its own (inactive accounts aren't being spent). An inactive
/// account already pinned at 100% (e.g. a weekly limit that won't reset for
/// days) is not actionable: you never swap TO an account that's out of room.
/// Folding it into the peak pinned the poll cadence at the 30s BACKSTOP for
/// days at a stretch, which hammered the usage endpoint into HTTP 429s for no
/// benefit. Scoping to the active account preserves the original
/// user-reported miss-fix (active at 94% must tighten so it doesn't lock
/// before the next poll — see `next_interval`) while dropping the waste.
///
/// Still weekly-aware: `Row::max_pct()` folds `max(session%, weekly%)`, so an
/// active account at session=0%/weekly=99% correctly yields `Some(99.0)` and
/// thus BACKSTOP cadence.
pub(crate) fn cadence_max_pct(rows: &[Row], active: Option<&str>) -> Option<f64> {
    let active = active?;
    rows.iter()
        .find(|r| r.email == active && r.has_data())
        .map(Row::max_pct)
}

/// Threshold band widths (in percentage points below `trigger`) and their
/// paired cadence intervals for `next_interval`'s adaptive ramp.
///
/// History: v0.4.3 was a fixed 150s cadence — caught an account at 94%, waited
/// the full 150s, account was at 99–100% by then (lock, swap missed). v0.5.x
/// added a binary WARNING band (30s inside 15pt of trigger, 150s below).
/// That fixed the miss but overshot: every account 80–95% was polled every
/// 30s. With ~6 accounts that's 720 req/hr against `/api/oauth/usage`, which
/// Anthropic 429s. The 429 loop pinned cached usage to a stale sub-trigger
/// value, auto-swap never fired, Claude Code hit session-expired (v0.7.2
/// user report; ~150 usage-429 events across a single day's log).
///
/// v0.7.3 replaces the binary band with a 4-tier ramp keyed to distance from
/// `trigger`. Widths are absolute percentage points so the whole ramp slides
/// with the user-configurable trigger (70 / 95 / 98 all work). Cadence
/// intervals halve at each step so an account climbing toward the trigger
/// gets pinged progressively more often without a cliff:
///
/// | Cached usage           | Cadence                               |
/// |------------------------|---------------------------------------|
/// | ≥ trigger              | 30s BACKSTOP (only if `actionable`)   |
/// | trigger−5 .. trigger   | 30s TIGHT                             |
/// | trigger−10 .. trigger−5| 60s MIDDLE                            |
/// | trigger−20 .. trigger−10| 120s RELAXED                         |
/// | < trigger−20           | 180s BASE (WATCH_INTERVAL_SECS)       |
///
/// The same tier table also drives the per-account fetch floor — see
/// `per_account_fetch_floor_secs`. Even when the global loop wakes at 30s
/// (because *some* account is in the TIGHT band), an account whose own
/// tier is MIDDLE/RELAXED/BASE only calls `usage::fetch` if its own last
/// fetch is at least that tier's interval old. That's the hard ceiling
/// that prevents any single account from being pinged faster than its tier
/// allows, regardless of how tight the loop cadence gets for other accounts.
pub(crate) const WATCH_TIGHT_BAND: f64 = 5.0;
pub(crate) const WATCH_MIDDLE_BAND: f64 = 10.0;
pub(crate) const WATCH_RELAXED_BAND: f64 = 20.0;
pub(crate) const WATCH_TIGHT_INTERVAL_SECS: u64 = 30;
pub(crate) const WATCH_MIDDLE_INTERVAL_SECS: u64 = 60;
pub(crate) const WATCH_RELAXED_INTERVAL_SECS: u64 = 120;
pub(crate) const WATCH_BACKSTOP_INTERVAL_SECS: u64 = 30;

/// Compute the next poll interval. Priority order:
///   1. Rate-limited from Anthropic → exponential backoff, doubling the
///      CURRENT interval (30s → 60s → 120s …, capped at
///      WATCH_MAX_INTERVAL_SECS). Overrides everything below. Doubling from
///      `max(current, base)` instead turned one 429 at the 30s tier into a
///      360s blind spot right next to the trigger — long enough to run out.
///   2. Active account at or above the trigger threshold AND a swap is
///      actionable (an eligible target exists, even if currently
///      cooldown-blocked) → BACKSTOP (30s). The auto-swap should already
///      have fired; this makes sure a transient failure or a soon-clearing
///      cooldown doesn't leave us blind for a full base cycle.
///   3. Active account at/above trigger but NOT actionable (no eligible
///      target at all — a single-account user, or every other account is
///      full / needs_relogin / env-overridden) → BASE. There is nothing to
///      catch, so polling every 30s for up to a week only risks HTTP 429;
///      the reset-boundary cap still wakes us near the reset.
///   4. Active account inside the ramp (trigger − WATCH_RELAXED_BAND
///      ≤ pct < trigger) → TIGHT / MIDDLE / RELAXED per the table on
///      `WATCH_TIGHT_BAND` above, regardless of `actionable` — we tighten
///      progressively to catch the *crossing*, at which point a target may
///      become relevant.
///   5. Comfortably below → BASE (default WATCH_INTERVAL_SECS = 180s).
///
/// `max_pct` is the ACTIVE account's `Row::max_pct()` — i.e.
/// `max(session%, weekly%)` for the account currently logged in (v0.5.13,
/// scoped down from the cross-account peak; see `cadence_max_pct`). It's
/// weekly-aware: a weekly window pinned at 99% with a healthy session is just
/// as "about to lock" as the reverse. `actionable` (v0.5.17) gates the
/// backstop so a maxed account with nowhere to swap doesn't over-poll.
pub(crate) fn next_interval(
    current: u64,
    base: u64,
    rate_limited: bool,
    max_pct: Option<f64>,
    trigger: f64,
    actionable: bool,
) -> u64 {
    if rate_limited {
        return (current.max(WATCH_TIGHT_INTERVAL_SECS) * 2).min(WATCH_MAX_INTERVAL_SECS);
    }
    let Some(pct) = max_pct else {
        return base;
    };
    if pct >= trigger {
        if actionable {
            WATCH_BACKSTOP_INTERVAL_SECS
        } else {
            base
        }
    } else {
        tier_interval_for_pct(pct, trigger, base)
    }
}

/// Poll cadence across every switchable provider. Each provider keeps its own
/// backoff state (`current`), so one provider's 429 never slows another's
/// polling; the loop wakes at the soonest any provider needs.
pub(crate) struct Cadence {
    pub(crate) base: u64,
    pub(crate) current: std::collections::HashMap<String, u64>,
    pub(crate) last_active: std::collections::HashMap<String, Option<String>>,
}

impl Cadence {
    pub(crate) fn new(base: u64) -> Self {
        Cadence {
            base,
            current: std::collections::HashMap::new(),
            last_active: std::collections::HashMap::new(),
        }
    }

    /// The provider's current interval (its 429-backoff seed), `base` if unseen.
    pub(crate) fn current_for(&self, slug: &str) -> u64 {
        self.current.get(slug).copied().unwrap_or(self.base)
    }

    /// Fold a cycle's outcome into each provider's interval and return how
    /// long to sleep before the next cycle.
    ///
    /// A different active account (auto-swap this cycle, or a manual switch
    /// since the last one) restarts that provider from the TIGHT interval: the
    /// backoff we carried was earned by the account we left. Seeding it with
    /// `base` instead turned one 429 on a just-landed account into a 360s
    /// blind spot. An auto-swap this cycle also ignores this cycle's 429 —
    /// it belonged to the old account.
    pub(crate) fn advance(&mut self, out: &CycleOutcome, trigger: f64) -> u64 {
        let mut next: Option<u64> = None;
        for p in &out.providers {
            let base = self.base;
            let cur = self.current.entry(p.slug.clone()).or_insert(base);
            let last = self.last_active.insert(p.slug.clone(), p.active.clone());
            let changed = last.is_some_and(|l| l != p.active);
            if p.swapped.is_some() {
                *cur = WATCH_TIGHT_INTERVAL_SECS;
            } else {
                if changed {
                    *cur = WATCH_TIGHT_INTERVAL_SECS;
                }
                let prev = *cur;
                *cur = match p.urgency {
                    Some(u) => next_interval_v2(
                        *cur,
                        base,
                        p.rate_limited,
                        u,
                        p.max_pct,
                        trigger,
                        p.actionable,
                    ),
                    None => {
                        next_interval(*cur, base, p.rate_limited, p.max_pct, trigger, p.actionable)
                    }
                };
                if p.rate_limited {
                    logging::log(&format!(
                        "rate limited; backing off to {}s (provider={})",
                        *cur, p.slug
                    ));
                } else if *cur != prev && *cur < base {
                    let max_pct = p.max_pct.unwrap_or(0.0);
                    logging::log(&format!(
                        "cadence: {prev}s → {}s (provider {}, max {max_pct:.1}%, trigger {trigger:.0}%) \
                         event=cadence provider={} prev={prev}s new={}s max_pct={max_pct:.1} \
                         trigger={trigger:.0}",
                        *cur, p.slug, p.slug, *cur
                    ));
                }
            }
            let mut wake = *cur;
            if let Some(at) = p.next_check_at {
                // Wake at `next_check_at` (a wake with no fetch due still
                // decides), but never spin faster than the check floor.
                let secs = (at - Utc::now()).num_seconds().max(0) as u64;
                wake = wake.min(secs.max(NEXT_CHECK_MIN_SECS));
            }
            next = Some(next.map_or(wake, |n| n.min(wake)));
        }
        next.unwrap_or(self.base)
    }
}

/// Shortest sleep a v2 `next_check_at` may request.
pub(crate) const NEXT_CHECK_MIN_SECS: u64 = 15;

/// The v2 cadence (§5): `actionable` and the TIGHT band come from the
/// decision's urgency. TIGHT is 60 s (with the blind projection), an active
/// 429 in band or Draining backs off at most to 120 s, and Draining/Blocked
/// with nothing to catch falls back to BASE.
pub(crate) fn next_interval_v2(
    current: u64,
    base: u64,
    rate_limited: bool,
    urgency: autoswap::Urgency,
    max_pct: Option<f64>,
    trigger: f64,
    actionable: bool,
) -> u64 {
    use autoswap::policy::{ACTIVE_BAND_429_CAP_S, ACTIVE_TIGHT_FLOOR_S};
    use autoswap::Urgency;
    if rate_limited {
        let doubled = (current.max(ACTIVE_TIGHT_FLOOR_S) * 2).min(WATCH_MAX_INTERVAL_SECS);
        return if matches!(urgency, Urgency::Band | Urgency::Draining) {
            doubled.min(ACTIVE_BAND_429_CAP_S)
        } else {
            doubled
        };
    }
    match urgency {
        Urgency::Draining | Urgency::Blocked => {
            if actionable {
                ACTIVE_TIGHT_FLOOR_S
            } else {
                base
            }
        }
        Urgency::Band => ACTIVE_TIGHT_FLOOR_S,
        Urgency::Calm => v2_active_floor_secs(max_pct, trigger, base),
    }
}

/// Map a usage percentage to its cadence tier interval. Shared by
/// `next_interval` (drives the global loop) and `per_account_fetch_floor_secs`
/// (per-account fetch throttle) so the two views of the same ramp can't drift.
///
/// The `>= trigger` case never reaches here in `next_interval` (handled
/// above with `actionable`), but the per-account floor uses this directly:
/// an account already over the trigger stays on the TIGHT tier so a stale
/// 429-preserved cache still refreshes as soon as the endpoint recovers.
pub(crate) fn tier_interval_for_pct(pct: f64, trigger: f64, base: u64) -> u64 {
    if pct >= trigger - WATCH_TIGHT_BAND {
        WATCH_TIGHT_INTERVAL_SECS
    } else if pct >= trigger - WATCH_MIDDLE_BAND {
        WATCH_MIDDLE_INTERVAL_SECS
    } else if pct >= trigger - WATCH_RELAXED_BAND {
        WATCH_RELAXED_INTERVAL_SECS
    } else {
        base
    }
}

#[cfg(test)]
#[path = "cadence_tests.rs"]
mod tests;
