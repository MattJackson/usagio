use super::*;
use crate::menubar::actions::handle_click;
use crate::menubar::test_support::{
    next_mtime_tag, restore_test_acct, save_state_with_mtime, with_now, POLL_LOCK, REFRESH_LOCK,
    SNAPSHOT_LOCK,
};
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use crate::watch::cycle::SwapGuards;
use chrono::{TimeZone, Utc};
use std::time::Duration;

#[test]
fn suspend_gap_detects_only_real_wall_clock_jumps() {
    // Ordinary sleep: elapsed ~= chunk (allow scheduler jitter within slack).
    assert!(!is_suspend_gap(30, 30));
    assert!(!is_suspend_gap(30, 31));
    assert!(!is_suspend_gap(30, 30 + SUSPEND_GAP_SLACK_SECS));
    // A jump beyond chunk + slack means the machine was suspended.
    assert!(is_suspend_gap(30, 30 + SUSPEND_GAP_SLACK_SECS + 1));
    assert!(is_suspend_gap(30, 30 * 60)); // a 30-minute battery-death sleep
                                          // A short final chunk still detects a suspend.
    assert!(is_suspend_gap(5, 1800));
    assert!(!is_suspend_gap(5, 5));
}

#[test]
fn shared_swap_guard_returns_the_same_arc_every_call() {
    // `poll_loop` and
    // `handle_refresh_now` must reason about anti-thrash cooldown/
    // no-return through ONE `SwapGuard`, not a throwaway
    // `SwapGuard::default()` per call site. `Arc::ptr_eq` pins that both
    // callers observe the identical process-wide instance.
    let a = shared_swap_guard();
    let b = shared_swap_guard();
    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "shared_swap_guard must return the same Arc every call"
    );
    // And it's actually usable as a `&mut SwapGuard` through the lock,
    // same as every `run_cycle` call site needs — `SwapGuard`'s fields
    // are private but visible here since `menubar` is a descendant of
    // the crate-root module that declares the struct (same access
    // `run_cycle`'s callers already rely on for `&mut SwapGuard`).
    {
        // Tolerate poison, like every production caller: the refresh-thread
        // test deliberately panics a `run_cycle` while holding this guard.
        let mut g = a.lock().unwrap_or_else(|e| e.into_inner());
        g.for_provider(CLAUDE_SLUG).use_credits = true;
    }
    assert!(
        b.lock()
            .unwrap_or_else(|e| e.into_inner())
            .for_provider(CLAUDE_SLUG)
            .use_credits,
        "mutation through one handle must be visible through the other"
    );
}

fn cached(
    session: f64,
    weekly: f64,
    session_reset: &str,
    weekly_reset: &str,
) -> crate::store::CachedUsage {
    crate::store::CachedUsage {
        session_pct: Some(session),
        weekly_pct: Some(weekly),
        session_reset: Some(session_reset.to_string()),
        weekly_reset: Some(weekly_reset.to_string()),
        opus_pct: None,
        opus_reset: None,
        fetched_at: 0,
        reported: Default::default(),
    }
}

/// A Codex `auth.json` for `email` whose id_token the codex provider can parse.
fn codex_auth_json(email: &str) -> String {
    use base64::Engine;
    let b64 = |s: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let id_token = format!(
        "{}.{}.sig",
        b64(br#"{"alg":"none"}"#),
        b64(
            serde_json::json!({ "email": email, "exp": 4_000_000_000_i64 })
                .to_string()
                .as_bytes()
        )
    );
    serde_json::json!({
        "tokens": { "id_token": id_token, "access_token": "at", "refresh_token": "rt" }
    })
    .to_string()
}

#[test]
fn next_reset_wake_is_the_soonest_future_reset_plus_the_settle_margin() {
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let now = Utc.timestamp_opt(2_000_000_000, 0).unwrap();
    let at = |secs: i64| (now + chrono::Duration::seconds(secs)).to_rfc3339();
    let state_with = |session: i64, weekly: i64| {
        let mut a = restore_test_acct("a@x.com");
        a.cached_usage = Some(cached(10.0, 10.0, &at(session), &at(weekly)));
        let mut st = State::default();
        st.accounts.push(a);
        st
    };

    // Soonest of the future resets (50s) plus the 5s settle margin.
    save_state_with_mtime(&state_with(50, 100), next_mtime_tag());
    with_now(now, || assert_eq!(next_reset_wake_secs(), Some(55)));
    save_state_with_mtime(&state_with(300, 100), next_mtime_tag());
    with_now(now, || assert_eq!(next_reset_wake_secs(), Some(105)));

    // A reset that is already past — or exactly now — is not upcoming.
    save_state_with_mtime(&state_with(-10, 0), next_mtime_tag());
    with_now(now, || assert_eq!(next_reset_wake_secs(), None));
}

// --- poll wake-up -------------------------------------------------------

#[test]
fn poll_request_cuts_the_sleep_short_and_is_consumed_once() {
    use std::time::Instant;
    let _l = POLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = wait_for_poll_request(Duration::ZERO); // drain stray requests

    // Nothing asked: it really waits out the timeout and says so.
    let t0 = Instant::now();
    assert!(!wait_for_poll_request(Duration::from_millis(300)));
    assert!(t0.elapsed() >= Duration::from_millis(250), "returned early");

    // A request wakes it immediately, once.
    request_poll_now();
    let t0 = Instant::now();
    assert!(wait_for_poll_request(Duration::from_secs(10)));
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "request didn't wake the wait"
    );
    assert!(
        !wait_for_poll_request(Duration::ZERO),
        "request is consumed"
    );

    // The bounded sleep: nothing to sleep → false; a pending request ends
    // it at once with true; an uneventful sleep reports false.
    assert!(!sleep_bounded_detecting_suspend(0));
    request_poll_now();
    let t0 = Instant::now();
    assert!(sleep_bounded_detecting_suspend(30));
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(!sleep_bounded_detecting_suspend(1));
}

#[test]
fn capture_and_switch_clicks_reach_the_codex_handlers() {
    let _l = POLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob_a = codex_auth_json("a@example.com");
        let blob_b = codex_auth_json("b@example.com");
        for blob in [&blob_a, &blob_b] {
            std::fs::write(dir.path().join("auth.json"), blob).unwrap();
            let _ = wait_for_poll_request(Duration::ZERO);
            handle_click("capture:codex");
            // Capture runs on a worker; it requests a poll once saved.
            let mut done = false;
            for _ in 0..200 {
                if wait_for_poll_request(Duration::from_millis(50)) {
                    done = true;
                    break;
                }
            }
            assert!(done, "capture never completed");
        }
        let st = State::load().unwrap();
        assert!(st.find_provider_account("codex", "a@example.com").is_some());
        assert!(st.find_provider_account("codex", "b@example.com").is_some());
        assert_eq!(
            st.provider_accounts("codex").unwrap().active.as_deref(),
            Some("b@example.com"),
            "a fresh capture becomes the active account"
        );

        handle_click("switch:codex:a@example.com");
        // The switch runs on a worker thread; wait for it to land.
        for _ in 0..500 {
            let st = State::load().unwrap();
            if st.provider_accounts("codex").unwrap().active.as_deref() == Some("a@example.com") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let st = State::load().unwrap();
        assert_eq!(
            st.provider_accounts("codex").unwrap().active.as_deref(),
            Some("a@example.com")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            blob_a
        );
    });
    // The worker asks for a poll after switching; consume it.
    let _ = wait_for_poll_request(Duration::from_secs(5));
}

#[test]
fn run_cycle_reports_the_trigger_it_dispatched_on() {
    let _g = crate::store::ScopedConfigDir::new();
    let codex = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(codex.path().to_str().unwrap()), || {
        let mut guards = SwapGuards::default();
        let mut trigger = |st: &State, opts: &LoopOpts| {
            st.save().unwrap();
            run_cycle(&mut guards, false, opts).1
        };
        let mut st = State::default();
        assert_eq!(trigger(&st, &LoopOpts::default()), 95.0, "default trigger");

        st.trigger_pct = Some(80.0);
        assert_eq!(trigger(&st, &LoopOpts::default()), 80.0, "Settings trigger");
        let watch = LoopOpts {
            trigger: Some(70.0),
            ..LoopOpts::default()
        };
        assert_eq!(trigger(&st, &watch), 70.0, "`watch --trigger` wins");

        // Auto-swap off -> an unreachable trigger, so the cycle only observes.
        st.autoswap_disabled = true;
        assert_eq!(trigger(&st, &LoopOpts::default()), 101.0);
    });
}

#[test]
fn dropping_the_engine_lock_makes_the_process_display_only() {
    use std::sync::atomic::Ordering;
    let _l = REFRESH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    ENGINE_DISPLAY_ONLY.store(true, Ordering::SeqCst);
    let lock = acquire_engine_lock();
    let owner = !ENGINE_DISPLAY_ONLY.load(Ordering::SeqCst);
    // A poll_loop panic unwinds and drops the lock before the respawn
    // backoff; another process may take it then.
    drop(lock);
    let after_drop = ENGINE_DISPLAY_ONLY.load(Ordering::SeqCst);
    ENGINE_DISPLAY_ONLY.store(false, Ordering::SeqCst);
    assert!(owner, "holding engine.lock makes this process the owner");
    assert!(
        after_drop,
        "a released engine.lock must not leave Refresh-now running an engine"
    );
}

#[test]
fn generic_capture_requests_a_poll() {
    let _l = POLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        std::fs::write(
            dir.path().join("auth.json"),
            codex_auth_json("a@example.com"),
        )
        .unwrap();
        let _ = wait_for_poll_request(Duration::ZERO);
        handle_click("capture:codex");
        let mut polled = false;
        for _ in 0..100 {
            if wait_for_poll_request(Duration::from_millis(50)) {
                polled = true;
                break;
            }
        }
        assert!(polled, "capture makes the login active; refresh it now");
    });
}

#[test]
fn a_failed_cycle_is_reported_as_an_error() {
    let _g = crate::store::ScopedConfigDir::new();
    let dir = crate::store::config_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), b"{ not json").unwrap();
    let shared = shared_swap_guard();
    let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
    let (res, _) = run_cycle_checked(&mut g, true, &LoopOpts::default());
    assert!(res.is_err(), "an aborted cycle must not look like success");
}

#[test]
fn a_queued_capture_is_never_dropped_by_a_later_switch() {
    let _l = POLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let auth = dir.path().join("auth.json");
        std::fs::write(&auth, codex_auth_json("a@example.com")).unwrap();
        let _ = wait_for_poll_request(Duration::ZERO);
        handle_click("capture:codex");
        for _ in 0..200 {
            if wait_for_poll_request(Duration::from_millis(50)) {
                break;
            }
        }
        std::fs::write(&auth, codex_auth_json("b@example.com")).unwrap();
        let shared = shared_swap_guard();
        let held = shared.lock().unwrap_or_else(|e| e.into_inner());
        handle_click("capture:codex");
        handle_click("switch:codex:a@example.com");
        drop(held);
        let mut ok = false;
        for _ in 0..200 {
            let st = State::load().unwrap();
            let pa = st.provider_accounts("codex").unwrap();
            if st.find_provider_account("codex", "b@example.com").is_some()
                && pa.active.as_deref() == Some("a@example.com")
            {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ok, "capture must run first, then the switch to a@");
    });
}
