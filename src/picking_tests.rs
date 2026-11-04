use super::*;
use crate::store::State;
use crate::test_support::{
    claude_account_with_identity, row_full, seed_claude_state, usage_at, with_claude_home,
};
use chrono::{Duration, Utc};

// --- candidate_order / auto_pick tie-break (D1) ---

// --- menu_order (dropdown priority) ---

#[test]
fn menu_order_lists_use_first_account_first() {
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let mut rows = [
        row_full("later@e.com", 5.0, 5.0, later),
        row_full("soon@e.com", 40.0, 40.0, soon),
    ];
    rows.sort_by(|a, b| menu_order(a, b, None, Utc::now()));
    // Soonest weekly reset (the account auto-pick would use first) leads.
    assert_eq!(rows[0].email, "soon@e.com");
    assert_eq!(rows[1].email, "later@e.com");
}

#[test]
fn menu_order_sinks_maxed_accounts_below_usable_ones() {
    let reset = Utc::now() + Duration::hours(6);
    // A maxed account resets soonest, but it's unusable — it must sort last.
    let mut maxed = row_full("maxed@e.com", 100.0, 40.0, reset);
    maxed.session.resets_at = Some(Utc::now() + Duration::hours(1));
    let mut rows = [
        maxed,
        row_full("free@e.com", 30.0, 30.0, Utc::now() + Duration::hours(24)),
    ];
    rows.sort_by(|a, b| menu_order(a, b, None, Utc::now()));
    assert_eq!(rows[0].email, "free@e.com");
    assert_eq!(rows[1].email, "maxed@e.com");
}

#[test]
fn menu_order_sorts_locked_accounts_by_unlock_time() {
    let now = Utc::now();
    // Weekly-locked, unlocks in 38h (its weekly reset is the soonest).
    let weekly_locked = row_full("weekly@e.com", 10.0, 100.0, now + Duration::hours(38));
    // Session-locked, unlocks in 2h, weekly reset 5d out — usable first.
    let mut session_locked = row_full("session@e.com", 100.0, 53.0, now + Duration::days(5));
    session_locked.session.resets_at = Some(now + Duration::hours(2));
    let mut rows = [weekly_locked, session_locked];
    rows.sort_by(|a, b| menu_order(a, b, None, now));
    assert_eq!(rows[0].email, "session@e.com");
    assert_eq!(rows[1].email, "weekly@e.com");
}

#[test]
fn menu_order_sinks_active_locked_account() {
    let now = Utc::now();
    // Active status must not lift a locked account above a usable one.
    let mut locked = row_full("active@e.com", 100.0, 40.0, now + Duration::hours(6));
    locked.session.resets_at = Some(now + Duration::hours(1));
    let free = row_full("free@e.com", 30.0, 30.0, now + Duration::hours(24));
    let mut rows = [free, locked];
    rows.sort_by(|a, b| menu_order(a, b, Some("active@e.com"), now));
    assert_eq!(rows[0].email, "free@e.com");
    assert_eq!(rows[1].email, "active@e.com");
}

#[test]
fn menu_order_sinks_lapsed_subscriptions_last_alphabetically() {
    let now = Utc::now();
    // Lapsed rows carry stale, "attractive" caches (0% / soonest reset) —
    // they must still sink below every live account, even the active one,
    // and order among themselves by email only.
    let mut zed = row_full("zed@e.com", 0.0, 0.0, now + Duration::hours(1));
    zed.no_subscription = true;
    let mut abe = row_full("abe@e.com", 0.0, 100.0, now + Duration::days(6));
    abe.no_subscription = true;
    let live = row_full("live@e.com", 100.0, 100.0, now + Duration::days(5));
    let mut rows = [zed, live, abe];
    rows.sort_by(|a, b| menu_order(a, b, Some("zed@e.com"), now));
    let order: Vec<&str> = rows.iter().map(|r| r.email.as_str()).collect();
    assert_eq!(order, ["live@e.com", "abe@e.com", "zed@e.com"]);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn optimize_now_switches_to_the_best_account_and_stays_when_already_there() {
    with_claude_home("a@e.com", |_home| {
        let mut a = claude_account_with_identity("a@e.com", 3_600_000);
        a.cached_usage = Some(usage_at(80.0));
        let mut b = claude_account_with_identity("b@e.com", 3_600_000);
        b.cached_usage = Some(usage_at(10.0));
        seed_claude_state(vec![a, b], Some("a@e.com"));
        assert_eq!(optimize_now().unwrap().as_deref(), Some("b@e.com"));
        assert_eq!(State::load().unwrap().active.as_deref(), Some("b@e.com"));
        assert_eq!(optimize_now().unwrap(), None);
    });
}

#[test]
fn auto_pick_v2_ranks_ready_by_earliest_weekly_reset() {
    let now = Utc::now();
    let far = now + Duration::days(5);
    // v1 picked the soonest reset (dev4, 3 points to the trigger); v2 picks
    // the runway (D2: dev8 has 5).
    let rows = vec![
        row_full("dev4@e.com", 16.0, 92.0, far - Duration::hours(5)),
        row_full("dev8@e.com", 40.0, 90.0, far),
    ];
    assert_eq!(auto_pick_v2(&rows, 95.0, 95.0, now).unwrap(), "dev8@e.com");
    // Among Ready accounts the earliest weekly reset goes first, at any
    // distance (not only inside the 6 h rebalance horizon).
    let rows = vec![
        row_full("later@e.com", 10.0, 10.0, far),
        row_full("sooner@e.com", 10.0, 50.0, now + Duration::days(1)),
    ];
    assert_eq!(
        auto_pick_v2(&rows, 95.0, 95.0, now).unwrap(),
        "sooner@e.com"
    );
    // A Ready account whose weekly resets inside the 6 h horizon goes first.
    let rows = vec![
        row_full("roomy@e.com", 10.0, 10.0, far),
        row_full("expiring@e.com", 10.0, 50.0, now + Duration::hours(2)),
    ];
    assert_eq!(
        auto_pick_v2(&rows, 95.0, 95.0, now).unwrap(),
        "expiring@e.com"
    );
    // Every account at its limit: an error naming the soonest reset.
    let mut full = row_full("full@e.com", 10.0, 100.0, now + Duration::hours(3));
    full.weekly.resets_at = Some(now + Duration::hours(3));
    let err = auto_pick_v2(&[full], 95.0, 95.0, now)
        .unwrap_err()
        .to_string();
    assert!(err.contains("full@e.com"), "{err}");
}

#[test]
fn menu_order_active_locked_account_uses_unlock_time() {
    let now = Utc::now();
    let mut active = row_full("dev8", 100.0, 27.0, now + Duration::days(6));
    active.session.resets_at = Some(now + Duration::hours(4));
    let mut earlier = row_full("dev4", 100.0, 54.0, now + Duration::days(7));
    earlier.session.resets_at = Some(now + Duration::hours(2));
    for selected in [Some("dev8"), Some("dev4"), None] {
        assert!(menu_order(&earlier, &active, selected, now).is_lt());
    }
}
