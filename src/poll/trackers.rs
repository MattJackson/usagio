//! Per-account fetch trackers: 429 backoff, inactive-fetch floors, last errors.

use chrono::Utc;

use crate::autoswap;
use crate::poll::plan::SUBSCRIPTION_RECHECK_SECS;
use crate::providers::CLAUDE_SLUG;

/// Flat minimum interval between usage fetches for an INACTIVE account. Only
/// the active account burns usage, so an inactive reading only moves at its
/// reset (which bypasses this floor) or if it's used elsewhere — the tier ramp
/// is reserved for the active account so the request budget goes where a
/// failover depends on it.
pub(crate) const INACTIVE_FETCH_FLOOR_SECS: u64 = 600;
/// Cap for an inactive account's own backoff after repeated failed fetches.
pub(crate) const INACTIVE_FETCH_MAX_BACKOFF_SECS: u64 = 3600;

/// Key into the in-memory usage-fetch trackers (429 escalation, inactive
/// backoff, subscription re-checks). Claude keeps the bare email; other
/// providers are namespaced so the same address on two providers can't share
/// a tracker.
pub(crate) fn fetch_tracker_key(slug: &str, key: &str) -> String {
    if slug == CLAUDE_SLUG {
        key.to_string()
    } else {
        format!("{slug}/{key}")
    }
}

// ---------------------------------------------------------------------------
// v0.8.0 auto-swap escalation on repeated `/oauth/usage` 429s
// ---------------------------------------------------------------------------

/// Ephemeral per-account usage-endpoint tracker for the v0.8.0 escalation
/// path. In-memory only, no `State` field — persisting a counter reintroduced
/// the `needs_relogin` stale-latch bug class in prior sessions (Opus
/// adversarial review BLOCKER 2). A daemon restart drops the state; the fresh
/// process observes real 429s (or successes) and rebuilds within seconds.
#[derive(Default, Clone, Copy)]
pub(crate) struct UsageFetchTracker {
    pub(crate) consecutive_429s: u32,
    /// Inactive-account backoff (v0.8.5): consecutive failed fetches of any
    /// kind, and the time before which we won't try again.
    pub(crate) consecutive_failures: u32,
    pub(crate) backoff_until_ts: Option<i64>,
    /// Last profile (subscription) check — throttles them to
    /// `SUBSCRIPTION_RECHECK_SECS` per account.
    pub(crate) last_subscription_check_ts: Option<i64>,
    /// Last usage fetch attempt (unix secs) and whether it failed — so the
    /// v2 poller honours the active account's own backoff instead of
    /// re-fetching on every wake (a failure doesn't advance `fetched_at`).
    pub(crate) last_attempt_ts: Option<i64>,
    pub(crate) last_attempt_failed: bool,
}

pub(crate) static USAGE_FETCH_TRACKERS: std::sync::Mutex<
    Option<std::collections::HashMap<String, UsageFetchTracker>>,
> = std::sync::Mutex::new(None);

/// Set by `poll_loop` (and the menubar poller) at the top of each cycle so
/// `per_account_fetch_floor_secs_with_projection` can compute the correct
/// projection horizon `dt = max(tier_floor, current_loop_interval)`. Under
/// a 429 backoff `next_interval` can push the loop cadence to 1200s while
/// tier_floor is only 30–180s — projecting on the wrong horizon
/// under-predicts by up to 40× and the smart-cadence feature never trips
/// (Opus adversarial review BLOCKER 1). Zero means "not yet set" and
/// callers fall back to using the tier floor alone.
pub(crate) static CURRENT_LOOP_INTERVAL_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn set_current_loop_interval_secs(secs: u64) {
    CURRENT_LOOP_INTERVAL_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn with_fetch_trackers<T>(
    f: impl FnOnce(&mut std::collections::HashMap<String, UsageFetchTracker>) -> T,
) -> T {
    let mut guard = USAGE_FETCH_TRACKERS.lock().unwrap();
    if guard.is_none() {
        *guard = Some(std::collections::HashMap::new());
    }
    f(guard.as_mut().unwrap())
}

/// The last token-refresh / usage-fetch failure per fetch-tracker key, shown
/// on the account's menu row until a fetch succeeds. In-memory like the
/// trackers: the polling process is the one whose menu renders it.
pub(crate) static LAST_REFRESH_ERRORS: std::sync::Mutex<
    Option<std::collections::HashMap<String, String>>,
> = std::sync::Mutex::new(None);

pub(crate) fn note_refresh_error(key: &str, err: Option<String>) {
    let mut g = LAST_REFRESH_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(Default::default);
    match err {
        Some(e) => {
            m.insert(key.to_string(), e);
        }
        None => {
            m.remove(key);
        }
    }
}

pub(crate) fn last_refresh_error(key: &str) -> Option<String> {
    LAST_REFRESH_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(key).cloned())
}

pub(crate) fn record_usage_fetch_success(email: &str) {
    note_refresh_error(email, None);
    with_fetch_trackers(|t| {
        let entry = t.entry(email.to_string()).or_default();
        entry.consecutive_429s = 0;
        entry.consecutive_failures = 0;
        entry.backoff_until_ts = None;
        entry.last_attempt_ts = Some(Utc::now().timestamp());
        entry.last_attempt_failed = false;
    });
}

/// Record a failed usage fetch attempt (any error) for the backoff check.
pub(crate) fn record_fetch_attempt_failed(key: &str, now_ts: i64) {
    with_fetch_trackers(|t| {
        let entry = t.entry(key.to_string()).or_default();
        entry.last_attempt_ts = Some(now_ts);
        entry.last_attempt_failed = true;
    });
}

/// Per-provider poll interval (the cadence's 429-backoff seed), published by
/// the poll loop so the v2 active fetch honours the provider's own backoff.
pub(crate) static PROVIDER_INTERVALS: std::sync::Mutex<
    Option<std::collections::HashMap<String, u64>>,
> = std::sync::Mutex::new(None);

pub(crate) fn set_provider_interval_secs(slug: &str, secs: u64) {
    let mut g = PROVIDER_INTERVALS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Default::default)
        .insert(slug.to_string(), secs);
}

pub(crate) fn provider_interval_secs(slug: &str) -> u64 {
    PROVIDER_INTERVALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(slug).copied())
        .unwrap_or(autoswap::policy::ACTIVE_BAND_429_CAP_S)
}

/// The active account's fetch is in backoff (v2): its last attempt failed
/// within the provider's current interval. No `NeedReading(active)` then.
pub(crate) fn active_fetch_in_backoff(slug: &str, key: &str, now_ts: i64) -> bool {
    let t = get_usage_fetch_tracker(&fetch_tracker_key(slug, key));
    t.last_attempt_failed
        && t.last_attempt_ts
            .is_some_and(|a| (0..provider_interval_secs(slug) as i64).contains(&(now_ts - a)))
}

/// Backoff after `failures` consecutive failed fetches of an inactive
/// account: the inactive floor, doubling, capped.
pub(crate) fn inactive_fetch_backoff_secs(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(8);
    (INACTIVE_FETCH_FLOOR_SECS << shift).min(INACTIVE_FETCH_MAX_BACKOFF_SECS)
}

/// Back an INACTIVE account off on its own after a failed fetch (429 or any
/// other error), instead of slowing the global loop the active account runs on.
pub(crate) fn record_inactive_fetch_failure(email: &str, now_ts: i64) {
    with_fetch_trackers(|t| {
        let entry = t.entry(email.to_string()).or_default();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.backoff_until_ts =
            Some(now_ts + inactive_fetch_backoff_secs(entry.consecutive_failures) as i64);
    });
}

/// Claim a subscription check for `email` if one is due (or `force`d);
/// returns false when the last one was under `SUBSCRIPTION_RECHECK_SECS` ago.
pub(crate) fn claim_subscription_check(email: &str, now_ts: i64, force: bool) -> bool {
    with_fetch_trackers(|t| {
        let entry = t.entry(email.to_string()).or_default();
        let due = force
            || entry
                .last_subscription_check_ts
                .is_none_or(|last| now_ts - last >= SUBSCRIPTION_RECHECK_SECS as i64);
        if due {
            entry.last_subscription_check_ts = Some(now_ts);
        }
        due
    })
}

/// Forget an account's failure backoff so a just-renewed subscription is
/// fetched this cycle instead of waiting out backoff earned while lapsed.
pub(crate) fn clear_inactive_fetch_backoff(email: &str) {
    with_fetch_trackers(|t| {
        if let Some(entry) = t.get_mut(email) {
            entry.consecutive_failures = 0;
            entry.backoff_until_ts = None;
        }
    });
}

/// The active account as of the last `refresh_usage_cache`. Lets the poller
/// notice a switch (manual, auto-swap, or an external `claude /login`) and
/// refresh the new active account right away at base cadence.
pub(crate) static LAST_POLLED_ACTIVE: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// Record `active` as polled; returns true if it differs from the last one.
pub(crate) fn note_polled_active(active: Option<&str>) -> bool {
    let mut last = LAST_POLLED_ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let changed = last.as_deref() != active;
    *last = active.map(str::to_string);
    changed
}

/// Per-provider twin of `note_polled_active`: the active key each non-Claude
/// provider had at its last poll. Returns true when it changed (or is new).
pub(crate) fn note_polled_provider_active(slug: &str, active: Option<&str>) -> bool {
    static LAST: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
        std::sync::Mutex::new(None);
    let mut guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(Default::default);
    // Tests run in parallel threads sharing one process: keep their state apart.
    #[cfg(test)]
    let slug = &format!("{slug}#{:?}", std::thread::current().id());
    let changed = map.get(slug).map(String::as_str) != active;
    match active {
        Some(a) => {
            map.insert(slug.to_string(), a.to_string());
        }
        None => {
            map.remove(slug);
        }
    }
    changed
}

pub(crate) fn record_usage_fetch_429(email: &str) {
    with_fetch_trackers(|t| {
        let entry = t.entry(email.to_string()).or_default();
        entry.consecutive_429s = entry.consecutive_429s.saturating_add(1);
    });
}

/// Any non-RateLimited error (network timeout, DNS, auth, …) resets the
/// consecutive counter. Flaky-network failures aren't the "the server is
/// telling us to back off" signal escalation exists to react to (Sonnet
/// adversarial review, BAD 4).
pub(crate) fn record_usage_fetch_non_429(email: &str) {
    with_fetch_trackers(|t| {
        if let Some(entry) = t.get_mut(email) {
            entry.consecutive_429s = 0;
        }
    });
}

/// Called from the switch path so a stale count on the account we're leaving
/// can't re-fire the moment it becomes active again after NO_RETURN expires
/// (Opus adversarial review BAD 4).
pub(crate) fn reset_usage_fetch_tracker(email: &str) {
    with_fetch_trackers(|t| {
        t.remove(email);
    });
}

pub(crate) fn get_usage_fetch_tracker(email: &str) -> UsageFetchTracker {
    with_fetch_trackers(|t| t.get(email).copied().unwrap_or_default())
}

#[cfg(test)]
#[path = "trackers_tests.rs"]
mod tests;
