use super::*;
use crate::menubar::poller::{shared_swap_guard, ENGINE_DISPLAY_ONLY};
use crate::menubar::test_support::{codex_state_acct, restore_test_acct, POLL_LOCK, REFRESH_LOCK};
use crate::providers::CLAUDE_SLUG;
use crate::state_ops::with_state_lock;
use crate::store::State;
use std::cell::Cell as StdCell;
use std::time::Duration;

#[test]
fn try_start_refresh_dedupes_rapid_clicks() {
    // try_start_refresh reaches logging::log → store::config_dir(), which
    // panics in tests without a HOME_OVERRIDE. Wrap in ScopedConfigDir so
    // the tripwire (added post-hermeticity merge) doesn't fire here.
    let _l = REFRESH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    // Reset in case a prior test in this binary left it set (best-effort
    // — tests run with --test-threads=1 so no other test races us here).
    REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);

    let spawn_count = std::rc::Rc::new(StdCell::new(0u32));
    let c1 = spawn_count.clone();
    try_start_refresh(move || {
        c1.set(c1.get() + 1);
        // Deliberately do NOT reset REFRESH_IN_FLIGHT here — this stands
        // in for "the background thread is still running" so the second
        // click below is the one under test.
    });
    assert_eq!(spawn_count.get(), 1, "first click must spawn");

    let c2 = spawn_count.clone();
    try_start_refresh(move || {
        c2.set(c2.get() + 1);
    });
    assert_eq!(
        spawn_count.get(),
        1,
        "second rapid click must NOT spawn a second refresh"
    );

    // Clean up so later tests in this binary see the flag cleared.
    REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn only_the_latest_queued_switch_runs() {
    let shared = shared_swap_guard();
    let held = shared.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = std::sync::mpsc::channel();
    for name in ["A", "B"] {
        let tx = tx.clone();
        spawn_latest_switch("latest-test", move |_| {
            let _ = tx.send(name);
        });
    }
    drop(held);
    // Wait generously for the surviving switch (other tests hold the
    // process-global guard), then briefly confirm nothing else arrives.
    let mut ran = Vec::new();
    if let Ok(n) = rx.recv_timeout(std::time::Duration::from_secs(10)) {
        ran.push(n);
    }
    while let Ok(n) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
        ran.push(n);
    }
    assert_eq!(ran, ["B"], "superseded switch must not run");
}

#[test]
fn serialized_work_does_not_block_the_caller_while_poller_holds_guard() {
    let shared = shared_swap_guard();
    let held = shared.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_serialized_with_poller(move |_| {
        let _ = tx.send(());
    });
    // We got here without blocking; the work must not run until released.
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err());
    drop(held);
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .expect("work runs once the guard is free");
}

#[test]
fn toggle_notification_trigger_read_modify_write_is_atomic_under_contention() {
    // v0.5.2 item 5 removed the top-level "Auto-swap enabled" checkbox
    // (and `toggle_autoswap` with it — Off/threshold picks in Settings ▸
    // Auto-swap ▸ replace it, and those write a fixed target rather than
    // flipping a bool). `toggle_notification_trigger` is still a genuine
    // read-modify-write click handler, so it inherits this contention
    // regression test in `toggle_autoswap`'s place.
    let g = crate::store::ScopedConfigDir::new();
    let home = g.home();

    State::default().save().expect("seed initial state");
    let initial = State::load().unwrap().notification_config.threshold_enabled;

    const ITERATIONS: usize = 100;
    // Two threads toggle; a third bumps a counter field under the same
    // lock. A toggle that skips the lock reads a stale counter and writes
    // it back, so lost updates show up as an exact count shortfall instead
    // of only (maybe) flipping the parity of the bool.
    let handles: Vec<_> = (0..3)
        .map(|worker| {
            let home = home.clone();
            std::thread::spawn(move || {
                // HOME_OVERRIDE is thread-local (see store::ScopedConfigDir);
                // each worker thread must repoint it at the same tempdir
                // the parent test set up.
                crate::store::set_home_override(Some(home));
                for _ in 0..ITERATIONS {
                    if worker == 2 {
                        with_state_lock(|_| {
                            let mut st = State::load()?;
                            st.trigger_pct = Some(st.trigger_pct.unwrap_or(0.0) + 1.0);
                            st.save()
                        })
                        .unwrap();
                    } else {
                        toggle_notification_trigger("threshold");
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    let total_toggles = 2 * ITERATIONS;
    assert_eq!(
        State::load().unwrap().trigger_pct,
        Some(ITERATIONS as f64),
        "every locked counter bump must survive the concurrent toggles"
    );
    let expected = if total_toggles % 2 == 1 {
        !initial
    } else {
        initial
    };
    let final_state = State::load().unwrap();
    assert_eq!(
        final_state.notification_config.threshold_enabled, expected,
        "final parity must match initial XOR (total_toggles % 2 == 1); a lost \
             toggle under contention would flip this"
    );

    drop(g);
}

#[test]
fn reset_claim_requires_confirmation_and_never_claims_codex() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    for (provider, approved, expected) in [
        ("claude", false, false),
        ("claude", true, true),
        ("codex", true, false),
    ] {
        let asked = StdCell::new(false);
        let claimed = Arc::new(AtomicBool::new(false));
        let c = claimed.clone();
        handle_claim_with(
            provider,
            "gift@example.com",
            &[],
            Some("g1"),
            |question, on_yes| {
                asked.set(true);
                assert!(question.contains("gift@example.com"));
                if approved {
                    on_yes();
                }
            },
            move |key, grant| {
                assert_eq!(key, "gift@example.com");
                assert_eq!(grant.as_deref(), Some("g1"));
                c.store(true, Ordering::SeqCst);
            },
        );
        assert_eq!(asked.get(), provider == "claude");
        assert_eq!(claimed.load(Ordering::SeqCst), expected);
    }
}

fn cleared(key: &str, pct: f64) -> crate::providers::trait_def::ClearedLimit {
    crate::providers::trait_def::ClearedLimit {
        key: key.into(),
        pct: Some(pct),
    }
}

fn ready_grant(
    id: &str,
    clears: Vec<crate::providers::trait_def::ClearedLimit>,
) -> crate::providers::trait_def::ResetGrant {
    crate::providers::trait_def::ResetGrant {
        id: id.into(),
        resets_left: 1,
        usable_now: true,
        clears,
        ..Default::default()
    }
}

/// dev2's live shape: a full (session + weekly) reset listed first, then a
/// session-only launch reset.
fn dev2_grants() -> Vec<crate::providers::trait_def::ResetGrant> {
    vec![
        ready_grant(
            "full-reset-2611",
            vec![
                cleared("five_hour", 100.0),
                cleared("seven_day", 27.0),
                cleared("seven_day_overage_included", 7.0),
            ],
        ),
        ready_grant(
            "launch-2610-fable",
            vec![
                cleared("five_hour", 100.0),
                cleared("seven_day_overage_included", 7.0),
            ],
        ),
    ]
}

#[test]
fn claim_target_splits_account_and_grant() {
    assert_eq!(
        split_claim_target("dev2@example.com:launch-2610-fable"),
        ("dev2@example.com", Some("launch-2610-fable"))
    );
    // A grant id may itself contain ':'.
    assert_eq!(
        split_claim_target("dev2@example.com:a:b"),
        ("dev2@example.com", Some("a:b"))
    );
    // Old click ids carry only the account.
    assert_eq!(
        split_claim_target("dev2@example.com"),
        ("dev2@example.com", None)
    );
    let c = parse_click_id("claim:claude:dev2@example.com:launch-2610-fable");
    assert_eq!(c.key, Some("dev2@example.com:launch-2610-fable"));
}

#[test]
fn claim_spends_the_grant_that_was_clicked_not_the_first_ready_one() {
    let grants = dev2_grants();
    assert_eq!(
        chosen_grant(&grants, Some("launch-2610-fable")).map(|g| g.id.as_str()),
        Some("launch-2610-fable")
    );
    // No grant named (old menu): first ready one, as before.
    assert_eq!(
        chosen_grant(&grants, None).map(|g| g.id.as_str()),
        Some("full-reset-2611")
    );
    // A named grant that's gone or not ready is never swapped for another.
    assert!(chosen_grant(&grants, Some("gone")).is_none());
    let claimed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let c = claimed.clone();
    handle_claim_with(
        "claude",
        "dev2@example.com",
        &grants,
        Some("launch-2610-fable"),
        |_, on_yes| on_yes(),
        move |_, grant| *c.lock().unwrap() = grant,
    );
    assert_eq!(
        claimed.lock().unwrap().as_deref(),
        Some("launch-2610-fable")
    );
}

#[test]
fn claim_question_names_the_reset_and_what_stays_banked() {
    let grants = dev2_grants();
    assert_eq!(
        claim_question("dev2@example.com", &grants, Some("launch-2610-fable")),
        "Use the session reset on dev2@example.com?\n\n\
         Clears the 5-hour session limit (now 100%).\n\
         Your weekly limit (now 27%) is not reset.\n\n\
         Still banked after this: 1 session + weekly reset."
    );
    assert_eq!(
        claim_question("dev2@example.com", &grants, Some("full-reset-2611")),
        "Use the session + weekly reset on dev2@example.com?\n\n\
         Clears the 5-hour session limit (now 100%).\n\
         Clears the weekly limit (now 27%).\n\n\
         Only need the session cleared? Use the session reset\n\
         instead and keep this one for the weekly limit.\n\n\
         Still banked after this: 1 session reset."
    );
}

#[test]
fn claim_question_counts_the_last_and_remaining_resets() {
    let g = |id: &str| {
        ready_grant(
            id,
            vec![
                cleared("five_hour", 99.0),
                cleared("seven_day_overage_included", 0.0),
            ],
        )
    };
    assert_eq!(
        claim_question("dev4@example.com", &[g("a")], Some("a")),
        "Use the session reset on dev4@example.com?\n\n\
         Clears the 5-hour session limit (now 99%).\n\
         Your weekly limit is not reset.\n\n\
         This is the last reset on this account."
    );
    let q = claim_question("dev4@example.com", &[g("a"), g("b"), g("c")], Some("b"));
    assert!(
        q.ends_with("Still banked after this: 2 session resets."),
        "{q}"
    );
    assert!(!q.contains("extra-usage"), "{q}");
}

#[test]
fn claim_question_names_an_extra_usage_only_reset() {
    // matthew@getbusbar: weekly-capped, launch reset clears only extra usage.
    let g = ready_grant(
        "launch-2610-fable",
        vec![cleared("seven_day_overage_included", 2.0)],
    );
    assert_eq!(
        claim_question("m@example.com", &[g], None),
        "Use the extra-usage reset on m@example.com?\n\n\
         Clears the extra-usage allowance (now 2%).\n\
         Your weekly limit is not reset.\n\n\
         This is the last reset on this account."
    );
}

#[test]
fn claim_notification_says_what_was_cleared() {
    assert_eq!(
        cleared_phrase(&["five_hour".into(), "seven_day_overage_included".into()]),
        "5-hour session limit cleared"
    );
    assert_eq!(
        cleared_phrase(&["five_hour".into(), "seven_day".into()]),
        "5-hour session limit and weekly limit cleared"
    );
    assert_eq!(cleared_phrase(&[]), "limit cleared");
    assert_eq!(
        cleared_phrase(&["seven_day_overage_included".into()]),
        "extra-usage allowance cleared"
    );
}

#[test]
fn parse_click_id_splits_action_slug_key() {
    let c = parse_click_id("switch:claude:matt@example.com");
    assert_eq!(c.action, "switch");
    assert_eq!(c.slug, Some("claude"));
    assert_eq!(c.key, Some("matt@example.com"));

    let c = parse_click_id("capture:codex");
    assert_eq!(c.action, "capture");
    assert_eq!(c.slug, Some("codex"));
    assert!(c.key.is_none());

    let c = parse_click_id("autoswap:off");
    assert_eq!(c.action, "autoswap");
    assert_eq!(c.slug, Some("off"));

    let c = parse_click_id("quit");
    assert_eq!(c.action, "quit");
    assert!(c.slug.is_none());
}

#[test]
fn accounts_needing_relogin_lists_claude_and_provider_accounts_sorted() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    let mut z = restore_test_acct("z@x.com");
    z.needs_relogin = true;
    st.accounts.push(z);
    st.accounts.push(restore_test_acct("fine@x.com"));
    let mut c = codex_state_acct("c@x.com");
    c.needs_relogin = true;
    st.upsert_provider_account("codex", c);
    st.upsert_provider_account("codex", codex_state_acct("ok@x.com"));
    st.save().unwrap();
    assert_eq!(accounts_needing_relogin(), ["c@x.com", "z@x.com"]);

    // A lapsed (Free) provider account renders its plan, not "re-login".
    let mut lapsed = codex_state_acct("free@x.com");
    lapsed.needs_relogin = true;
    lapsed.no_subscription = true;
    st.upsert_provider_account("codex", lapsed);
    st.save().unwrap();
    assert_eq!(accounts_needing_relogin(), ["c@x.com", "z@x.com"]);

    let _g2 = crate::store::ScopedConfigDir::new();
    assert!(accounts_needing_relogin().is_empty());
}

// --- click routing (state-observable arms) -------------------------------

#[test]
fn clicks_route_settings_actions_to_state() {
    let _g = crate::store::ScopedConfigDir::new();
    State::default().save().unwrap();
    let load = || State::load().unwrap();

    handle_click("autoswap:off");
    assert!(load().autoswap_disabled);
    handle_click("autoswap:90");
    let st = load();
    assert_eq!(st.trigger_pct, Some(90.0));
    assert!(
        !st.autoswap_disabled,
        "picking a threshold re-enables auto-swap"
    );
    handle_click("autoswap:not-a-number");
    assert_eq!(
        load().trigger_pct,
        Some(90.0),
        "garbage threshold is ignored"
    );

    handle_click("trayicon:codex");
    assert_eq!(load().tray_icon_mode.as_deref(), Some("codex"));
    handle_click("trayicon:at-risk");
    assert_eq!(load().tray_icon_mode, None);

    let before = load().notification_config;
    handle_click("notifications:threshold");
    assert_eq!(
        load().notification_config.threshold_enabled,
        !before.threshold_enabled
    );
    handle_click("notifications:resetback");
    assert_eq!(
        load().notification_config.reset_back_enabled,
        !before.reset_back_enabled
    );
    handle_click("notifications:pace");
    assert_eq!(
        load().notification_config.pace_enabled,
        !before.pace_enabled
    );
    handle_click("notifications:bogus");
    handle_click("noop");
    handle_click("something-new:x:y");
    let after = load().notification_config;
    assert_eq!(after.threshold_enabled, !before.threshold_enabled);
    assert_eq!(after.reset_back_enabled, !before.reset_back_enabled);
    assert_eq!(after.pace_enabled, !before.pace_enabled);
}

#[test]
fn settings_handlers_write_exactly_their_field() {
    let _g = crate::store::ScopedConfigDir::new();
    State::default().save().unwrap();

    set_autoswap(false);
    assert!(State::load().unwrap().autoswap_disabled);
    set_autoswap(true);
    assert!(!State::load().unwrap().autoswap_disabled);

    set_autoswap_threshold(77.0);
    assert_eq!(State::load().unwrap().trigger_pct, Some(77.0));

    set_tray_icon_mode("claude");
    assert_eq!(
        State::load().unwrap().tray_icon_mode.as_deref(),
        Some("claude")
    );
    set_tray_icon_mode("at-risk");
    assert_eq!(State::load().unwrap().tray_icon_mode, None);

    // Each trigger flips only its own switch, and an unknown one flips none.
    for trigger in ["threshold", "resetback", "pace"] {
        let before = State::load().unwrap().notification_config;
        toggle_notification_trigger(trigger);
        let after = State::load().unwrap().notification_config;
        let flipped = [
            before.threshold_enabled != after.threshold_enabled,
            before.reset_back_enabled != after.reset_back_enabled,
            before.pace_enabled != after.pace_enabled,
        ];
        let expect = match trigger {
            "threshold" => [true, false, false],
            "resetback" => [false, true, false],
            _ => [false, false, true],
        };
        assert_eq!(flipped, expect, "{trigger}");
    }
    let before = State::load().unwrap().notification_config;
    toggle_notification_trigger("nonsense");
    assert_eq!(State::load().unwrap().notification_config, before);
}

#[test]
fn refresh_now_marks_a_refresh_in_flight_and_always_clears_it() {
    use std::sync::atomic::Ordering;
    let _l = REFRESH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);

    // Hold the poller's swap guard so the refresh thread parks on it and
    // the in-flight window is deterministic. The thread has no config-dir
    // override, so its `run_cycle` then panics -- exactly the case the
    // drop guard exists for: the flag must still clear.
    let starts: [(&str, fn()); 2] = [
        ("direct call", handle_refresh_now),
        ("menu click", || handle_click("refresh:now")),
    ];
    for (how, start) in starts {
        let shared = shared_swap_guard();
        let held = shared.lock().unwrap_or_else(|e| e.into_inner());
        start();
        assert!(
            REFRESH_IN_FLIGHT.load(Ordering::SeqCst),
            "a started refresh must be marked in flight ({how})"
        );
        drop(held);

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while REFRESH_IN_FLIGHT.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the in-flight flag was never cleared ({how})"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// --- round-2 B2 regressions ---------------------------------------------

fn switch_gen(slug: &str) -> u64 {
    SWITCH_GEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(slug).copied())
        .unwrap_or(0)
}

#[test]
fn optimize_now_click_runs_on_a_worker_and_bumps_the_switch_generation() {
    let _l = POLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let before = switch_gen(CLAUDE_SLUG);
    handle_click("autoswap:now");
    assert_eq!(
        switch_gen(CLAUDE_SLUG),
        before + 1,
        "an Optimize click must supersede older queued switches"
    );
}

#[test]
fn refresh_now_in_a_display_only_process_does_not_run_the_engine() {
    use std::sync::atomic::Ordering;
    let _l = REFRESH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
    ENGINE_DISPLAY_ONLY.store(true, Ordering::SeqCst);
    let shared = shared_swap_guard();
    let held = shared.lock().unwrap_or_else(|e| e.into_inner());
    handle_refresh_now();
    let started = REFRESH_IN_FLIGHT.load(Ordering::SeqCst);
    ENGINE_DISPLAY_ONLY.store(false, Ordering::SeqCst);
    drop(held);
    REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
    assert!(!started, "a display-only process must not run a cycle");
}

#[test]
fn confirm_tells_a_dialog_failure_from_a_cancel() {
    use rfd::MessageDialogResult as R;
    assert_eq!(
        confirm_answer(R::Custom("Remove".into()), "Remove"),
        Some(true)
    );
    assert_eq!(
        confirm_answer(R::Custom("Cancel".into()), "Remove"),
        Some(false)
    );
    assert_eq!(
        confirm_answer(R::Cancel, "Remove"),
        None,
        "a non-button result means the dialog could not be shown"
    );
}

#[test]
fn refresh_now_reports_failed_fetches_instead_of_success() {
    assert_eq!(
        refresh_summary(0, 3, &[]),
        "Refresh failed — no account could be refreshed"
    );
    assert_eq!(
        refresh_summary(2, 1, &[]),
        "Usage refreshed — 1 account could not be refreshed"
    );
    assert_eq!(
        refresh_summary(2, 2, &[]),
        "Usage refreshed — 2 accounts could not be refreshed"
    );
    assert_eq!(refresh_summary(2, 0, &[]), "Usage refreshed");
    // Nothing was due (all skipped as fresh): not a failure.
    assert_eq!(refresh_summary(0, 0, &[]), "Usage refreshed");
    assert_eq!(
        refresh_summary(1, 0, &["a@x.io".to_string()]),
        "Usage refreshed — a@x.io needs re-login"
    );
}

/// Drain the UI queue on this (the "UI") thread until `flag` is set.
fn drain_ui_until(flag: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        run_ui_tasks();
        std::thread::sleep(Duration::from_millis(10));
    }
    flag.load(Ordering::SeqCst)
}

/// Regression: the confirm dialog was asked on the UI thread, so on macOS the
/// run loop stopped while it was open and none of its buttons (Cancel
/// included) worked. The ask must return at once, run the dialog elsewhere,
/// and run the confirmed action back on the UI thread.
#[test]
fn confirm_does_not_block_the_ui_thread_and_acts_there_on_yes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    let _ui = UI_TASKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ui_thread = std::thread::current().id();
    let (answer_tx, answer_rx) = mpsc::channel::<bool>();
    let ask_thread = Arc::new(Mutex::new(None));
    let did = Arc::new(AtomicBool::new(false));
    let yes_thread = Arc::new(Mutex::new(None));
    let (at, d, yt) = (ask_thread.clone(), did.clone(), yes_thread.clone());
    confirm_then_with(
        "Use the reset?".into(),
        "Use reset".into(),
        move |q, b| {
            assert_eq!((q, b), ("Use the reset?", "Use reset"));
            *at.lock().unwrap() = Some(std::thread::current().id());
            // The user hasn't answered yet: the dialog is still open.
            answer_rx.recv().unwrap()
        },
        move || {
            *yt.lock().unwrap() = Some(std::thread::current().id());
            d.store(true, Ordering::SeqCst);
        },
    );
    // Returned while the dialog is still open, and the UI loop keeps running.
    run_ui_tasks();
    assert!(
        !did.load(Ordering::SeqCst),
        "acted before the user answered"
    );
    answer_tx.send(true).unwrap();
    assert!(drain_ui_until(&did), "the confirmed action never ran");
    let asked_on = ask_thread.lock().unwrap().expect("dialog was asked");
    assert_ne!(
        asked_on, ui_thread,
        "the dialog must not run on the UI thread"
    );
    assert_eq!(*yes_thread.lock().unwrap(), Some(ui_thread));
}

#[test]
fn confirm_cancel_or_no_answer_does_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    let _ui = UI_TASKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let did = Arc::new(AtomicBool::new(false));
    let (asked_tx, asked_rx) = mpsc::channel();
    let d = did.clone();
    confirm_then_with(
        "Remove account?".into(),
        "Remove".into(),
        move |_, _| {
            asked_tx.send(()).unwrap();
            false
        },
        move || d.store(true, Ordering::SeqCst),
    );
    asked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("dialog was asked");
    // Give a (wrongly) queued action every chance to run.
    for _ in 0..10 {
        run_ui_tasks();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !did.load(Ordering::SeqCst),
        "Cancel must not run the action"
    );
}

#[test]
fn block_on_waits_for_a_future_completed_by_another_thread() {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    type Slot = Arc<Mutex<(Option<u32>, Option<Waker>)>>;
    struct Answer(Slot);
    impl Future for Answer {
        type Output = u32;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            let mut s = self.0.lock().unwrap();
            match s.0.take() {
                Some(v) => Poll::Ready(v),
                None => {
                    s.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
    let slot: Slot = Arc::new(Mutex::new((None, None)));
    let s = slot.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let mut g = s.lock().unwrap();
        g.0 = Some(7);
        if let Some(w) = g.1.take() {
            w.wake();
        }
    });
    assert_eq!(block_on(Answer(slot)), 7);
}
