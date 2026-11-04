use super::*;
use crate::providers::CLAUDE_SLUG;
use crate::rows::provider_rows;
use crate::store::State;
use crate::test_support::{acct_with_cache, usage_at};
use chrono::Utc;

#[test]
fn manual_pick_holds_only_at_or_over_the_trigger() {
    // 0.8.8 miss: a pick at 79% held, then rode to 99% (usage 429s froze the
    // cache short of 100%) and never swapped.
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(79.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    assert!(!pick_holds(&state, CLAUDE_SLUG, "pick@e.com"));
    state.find_mut("pick@e.com").unwrap().cached_usage = Some(usage_at(95.0));
    assert!(pick_holds(&state, CLAUDE_SLUG, "PICK@e.com"));
    // No usage yet: nothing to ack, so no hold.
    state.find_mut("pick@e.com").unwrap().cached_usage = None;
    assert!(!pick_holds(&state, CLAUDE_SLUG, "pick@e.com"));
}

#[test]
fn manual_pick_hold_settles_on_the_first_reading_after_the_switch() {
    // A pick decided on a stale 93% that was really at 96% must hold, not
    // auto-swap away on the first fresh reading.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(93.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    state.active = Some("pick@e.com".to_string());
    state.set_manual_pick(CLAUDE_SLUG, Some("pick@e.com"));
    let at = Utc::now().timestamp() - 60;
    state.manual_pick_at.insert(CLAUDE_SLUG.to_string(), at);
    // The reading the switch decided on predates it: nothing to settle yet.
    let cached = state.find_mut("pick@e.com").unwrap();
    cached.cached_usage.as_mut().unwrap().fetched_at = at - 300;
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(!settle_manual_pick(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), None);
    // First reading after the switch: at the trigger, so it holds.
    state.find_mut("pick@e.com").unwrap().cached_usage = Some(usage_at(96.0));
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(settle_manual_pick(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), Some("pick@e.com"));
    assert!(state.manual_pick_at.is_empty());
    // Settled once: later readings don't re-decide it.
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(!settle_manual_pick(&mut state, CLAUDE_SLUG, &rows, 95.0));
}

#[test]
fn a_stale_hold_drops_when_the_first_fresh_reading_is_under_the_trigger() {
    // Picked on a stale 97% after its window reset: the fresh 10% means the
    // pick was under the trigger, so it holds only until the trigger.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(97.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    state.active = Some("pick@e.com".to_string());
    state.set_manual_pick(CLAUDE_SLUG, Some("pick@e.com"));
    state.set_manual_hold(CLAUDE_SLUG, Some("pick@e.com"));
    state
        .manual_pick_at
        .insert(CLAUDE_SLUG.to_string(), Utc::now().timestamp() - 60);
    state.find_mut("pick@e.com").unwrap().cached_usage = Some(usage_at(10.0));
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(settle_manual_pick(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), None);
    assert_eq!(state.manual_pick(CLAUDE_SLUG), Some("pick@e.com"));
}

#[test]
fn a_hold_ends_once_the_held_account_resets() {
    // 2026-10-25: dev8 picked at 100% held; it reset, ran back to 100%, and
    // the leftover hold kept preparation from moving to the account that
    // reset 40 minutes sooner.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(100.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    state.active = Some("pick@e.com".to_string());
    state.set_manual_pick(CLAUDE_SLUG, Some("pick@e.com"));
    state.set_manual_hold(CLAUDE_SLUG, Some("pick@e.com"));
    // Still settling the first reading: leave it to settle_manual_pick.
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(!release_reset_hold(&mut state, CLAUDE_SLUG, &rows, 95.0));
    state.manual_pick_at.clear();
    // Still over the trigger: the hold stands.
    assert!(!release_reset_hold(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), Some("pick@e.com"));
    // Window reset: the hold ends, the pick stays.
    state.find_mut("pick@e.com").unwrap().cached_usage = Some(usage_at(3.0));
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(release_reset_hold(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), None);
    assert_eq!(state.manual_pick(CLAUDE_SLUG), Some("pick@e.com"));
}

#[test]
fn a_late_first_reading_keeps_the_switch_time_decision() {
    // 429s held off the first reading for 10 minutes; by then 96% says how
    // far it climbed since the pick, not where the pick started.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(96.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    state.active = Some("pick@e.com".to_string());
    state.set_manual_pick(CLAUDE_SLUG, Some("pick@e.com"));
    state
        .manual_pick_at
        .insert(CLAUDE_SLUG.to_string(), Utc::now().timestamp() - 600);
    let rows = provider_rows(&state, CLAUDE_SLUG);
    assert!(settle_manual_pick(&mut state, CLAUDE_SLUG, &rows, 95.0));
    assert_eq!(state.manual_hold(CLAUDE_SLUG), None);
    assert!(state.manual_pick_at.is_empty());
}

#[test]
fn a_hold_survives_a_cycle_run_with_auto_swap_off() {
    // Auto-swap off runs the cycle at the 101% sentinel; the hold was decided
    // against the saved 95% trigger and must be judged against it.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State {
        trigger_pct: Some(95.0),
        ..State::default()
    };
    let mut a = acct_with_cache(Some(usage_at(97.0)));
    a.email = Some("pick@e.com".to_string());
    state.upsert(a);
    state.active = Some("pick@e.com".to_string());
    state.set_manual_pick(CLAUDE_SLUG, Some("pick@e.com"));
    state.set_manual_hold(CLAUDE_SLUG, Some("pick@e.com"));
    state.manual_pick_at.clear(); // the first reading already settled it
    state.save().unwrap();
    // The cycle ran at the auto-swap-off sentinel (101%).
    settle_manual_picks();
    assert_eq!(
        State::load().unwrap().manual_hold(CLAUDE_SLUG),
        Some("pick@e.com")
    );
}
