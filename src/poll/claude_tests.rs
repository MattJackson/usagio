use super::*;
use crate::poll::trackers::{
    get_usage_fetch_tracker, record_fetch_attempt_failed, record_usage_fetch_429,
    reset_usage_fetch_tracker,
};
use crate::providers::CLAUDE_SLUG;
use crate::store::{CachedUsage, State};
use crate::test_support::{
    account_with_cache, claude_account, claude_expired, cu, fetches, run_refresh,
    spawn_mock_token_server, uniq, usage_at, usage_body, Fx, POLL_ACTIVE_LOCK,
};
use crate::{notifications, usage_log};
use chrono::{Duration, Utc};

// ---------------------------------------------------------------------------
// Inactive-account refresh still uses the plain network-outside-the-lock
// path (unaffected by the active-account CAS redesign above).
// ---------------------------------------------------------------------------

#[test]
fn refresh_usage_cache_still_refreshes_inactive_accounts() {
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use crate::providers::claude::{oauth, usage};
    use crate::store::{Account, ScopedConfigDir};

    let _g = ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_token_server();
    oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));
    usage::set_usage_url_override(Some(&format!("{base_url}/api/oauth/usage")));

    let expired = Utc::now().timestamp_millis() - 1_000;
    let mut inactive = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"inactive-at","refreshToken":"inactive-rt","expiresAt":{expired}}}}}"#
    ))
    .unwrap();
    inactive.email = Some("inactive@example.com".into());
    // No account is marked active, so refresh_usage_cache's active-CAS branch
    // is never entered (and never touches the real keychain) for this test.
    let mut seed = State::default();
    seed.accounts.push(inactive);
    seed.active = None;
    seed.save().unwrap();

    refresh_usage_cache(false);
    oauth::set_token_url_override(None);
    usage::set_usage_url_override(None);

    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly one /token POST for the (only, inactive) account"
    );
    let after = State::load().unwrap();
    let inactive_after = after.find("inactive@example.com").unwrap();
    assert_eq!(inactive_after.access_token, "mock-refreshed-access-token");
    assert_eq!(inactive_after.refresh_token, "mock-refreshed-refresh-token");
}

#[test]
fn reset_boundary_invalidates_recent_cache() {
    let now = Utc::now();
    let mut cu = CachedUsage {
        session_pct: Some(0.0),
        weekly_pct: Some(100.0),
        session_reset: None,
        weekly_reset: Some((now - Duration::seconds(5)).to_rfc3339()),
        opus_pct: None,
        opus_reset: None,
        fetched_at: (now - Duration::seconds(10)).timestamp(),
        ..Default::default()
    };
    assert!(cache_crossed_reset(&cu, now));
    cu.fetched_at = now.timestamp();
    assert!(!cache_crossed_reset(&cu, now));
    cu.weekly_reset = Some((now + Duration::seconds(5)).to_rfc3339());
    assert!(!cache_crossed_reset(&cu, now));
}

#[test]
fn cache_crossed_reset_needs_a_reading_strictly_before_a_reset_that_has_passed() {
    let now = Utc::now();
    let reset = now - Duration::seconds(30);
    let mut cu = usage_at(50.0);
    cu.weekly_reset = Some(reset.to_rfc3339());
    cu.fetched_at = reset.timestamp() - 1;
    assert!(cache_crossed_reset(&cu, now));
    cu.fetched_at = reset.timestamp();
    assert!(!cache_crossed_reset(&cu, now));
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn active_account_is_fetched_first_and_its_429_spares_the_inactive_ones() {
    let inactive = uniq("inactive");
    let active = uniq("active");
    let accounts = || {
        vec![
            account_with_cache(&inactive, Some(cu(10.0, 10.0, 2000))),
            account_with_cache(&active, Some(cu(93.0, 10.0, 1000))),
        ]
    };
    let (rate_limited, hits) = run_refresh(
        Fx::new(accounts(), Some(&active)).status(429, ""),
        |out, hits, _| (out.rate_limited, hits),
    );
    assert!(rate_limited);
    assert_eq!(hits, 1, "only the active account is asked");
    // A forced refresh asks everyone regardless.
    let hits = fetches(Fx::new(accounts(), Some(&active)).status(429, "").force());
    assert_eq!(hits, 2);
    reset_usage_fetch_tracker(&inactive);
    reset_usage_fetch_tracker(&active);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn flagged_accounts_are_skipped_unless_they_are_the_active_one() {
    let flagged = uniq("flagged");
    let active = uniq("act");
    let mut f = account_with_cache(&flagged, Some(cu(10.0, 10.0, 2000)));
    f.needs_relogin = true;
    let hits = fetches(Fx::new(
        vec![f, account_with_cache(&active, Some(cu(10.0, 10.0, 2000)))],
        Some(&active),
    ));
    assert_eq!(hits, 1, "a dead inactive login is never polled");
    // The active account is the only way a flag clears: it is adopted and polled.
    let mut a = account_with_cache(&active, Some(cu(10.0, 10.0, 2000)));
    a.needs_relogin = true;
    run_refresh(Fx::new(vec![a], Some(&active)), |_, hits, _| {
        assert_eq!(hits, 1);
        let st = State::load().unwrap();
        assert!(!st.find(&active).unwrap().needs_relogin);
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn fetch_failures_back_off_inactive_accounts_but_never_the_active_one() {
    let inactive = uniq("fail-inactive");
    let active = uniq("fail-active");
    for (status, body) in [(403, ""), (500, ""), (401, "")] {
        let accounts = vec![
            account_with_cache(&inactive, Some(cu(10.0, 10.0, 2000))),
            account_with_cache(&active, Some(cu(10.0, 10.0, 2000))),
        ];
        // A 429 count on each, to see which the failure path touches.
        record_usage_fetch_429(&inactive);
        record_usage_fetch_429(&active);
        run_refresh(
            Fx::new(accounts, Some(&active)).status(status, body),
            |out, hits, _| {
                assert!(!out.rate_limited, "HTTP {status} is not a rate limit");
                assert_eq!(hits, 2);
            },
        );
        let ti = get_usage_fetch_tracker(&inactive);
        assert!(
            ti.backoff_until_ts.is_some(),
            "HTTP {status}: inactive backs off"
        );
        assert_eq!(ti.consecutive_failures, 1);
        assert_eq!(
            ti.consecutive_429s, 0,
            "HTTP {status}: inactive counter reset"
        );
        let ta = get_usage_fetch_tracker(&active);
        assert!(
            ta.backoff_until_ts.is_none(),
            "HTTP {status}: active never backs off"
        );
        assert_eq!(ta.consecutive_failures, 0);
        if status == 401 {
            // The active account's token changed under us: wait for the vendor
            // CLI instead of treating it as an error that resets the count.
            assert_eq!(ta.consecutive_429s, 1);
        } else {
            assert_eq!(
                ta.consecutive_429s, 0,
                "HTTP {status}: active counter reset"
            );
        }
        reset_usage_fetch_tracker(&inactive);
        reset_usage_fetch_tracker(&active);
    }
}

#[test]
fn a_rejected_refresh_grant_flags_the_account_for_relogin() {
    let e = uniq("grant");
    let mut fx = Fx::new(vec![claude_expired(&e)], None);
    fx.token = Some((400, r#"{"error":"invalid_grant"}"#.to_string()));
    run_refresh(fx, |_, usage_hits, token_hits| {
        assert_eq!(token_hits, 1);
        assert_eq!(usage_hits, 0, "no usable token, no usage request");
        assert!(State::load().unwrap().find(&e).unwrap().needs_relogin);
    });
}

#[test]
fn a_fetched_reading_is_saved_and_its_notification_state_persisted() {
    let e = uniq("notif");
    let seed_email = e.clone();
    let mut fx = Fx::new(
        vec![account_with_cache(&e, Some(cu(10.0, 10.0, 2000)))],
        None,
    )
    .status(200, &usage_body(96.0, 96.0));
    // The previous history point was well under every threshold.
    fx.before = Some(Box::new(move || {
        usage_log::append(&usage_log::Snapshot {
            ts: Utc::now() - Duration::minutes(10),
            provider: CLAUDE_SLUG.to_string(),
            account: seed_email,
            session_pct: Some(40.0),
            weekly_pct: Some(40.0),
            active_model: None,
            ..Default::default()
        })
        .unwrap();
    }));
    run_refresh(fx, |_, hits, _| {
        assert_eq!(hits, 1);
        let st = State::load().unwrap();
        let a = st.find(&e).unwrap();
        assert_eq!(a.cached_usage.as_ref().unwrap().session_pct, Some(96.0));
        // Crossing the thresholds was recorded so it is not announced twice.
        assert_ne!(a.notif_state, crate::notifications::NotifState::default());
    });
}

#[test]
fn replay_rows_carry_the_new_fields_and_old_rows_still_parse() {
    let mut cache = cu(40.0, 80.0, 30);
    cache.weekly_reset = Some((Utc::now() + Duration::days(2)).to_rfc3339());
    let row = replay_row("codex", "x@e.com", &cache, true, "429");
    assert_eq!(row.active, Some(true));
    assert_eq!(row.outcome.as_deref(), Some("429"));
    assert_eq!(row.fetched_at, Some(cache.fetched_at));
    assert!(row.weekly_reset.is_some());
    assert!(!row.is_reading(), "a 429 row repeats the cache");
    let old: usage_log::Snapshot = serde_json::from_str(
        r#"{"ts":"2026-10-01T00:00:00Z","provider":"claude","account":"a","session_pct":1.0}"#,
    )
    .unwrap();
    assert!(old.is_reading());
    assert!(old.outcome.is_none() && old.active.is_none());
}

#[test]
fn v2_active_fetch_skips_an_lb_blocked_active_until_reset_or_stale() {
    let now = Utc::now().timestamp();
    let key = uniq("v2-skip");
    let mut blocked = cu(100.0, 40.0, 30);
    blocked.session_reset = Some((Utc::now() + Duration::hours(2)).to_rfc3339());
    assert_eq!(
        v2_active_fetch_skip(CLAUDE_SLUG, &key, &blocked, 95.0, now),
        Some("lb_blocked")
    );
    blocked.fetched_at = now - MAX_LOCKED_STALENESS_SECS - 1;
    assert_eq!(
        v2_active_fetch_skip(CLAUDE_SLUG, &key, &blocked, 95.0, now),
        None
    );
    // In band: the floor is 60 s, not 30.
    let fresh = cu(10.0, 92.0, 45);
    assert_eq!(
        v2_active_fetch_skip(CLAUDE_SLUG, &key, &fresh, 95.0, now),
        Some("fresh")
    );
    let due = cu(10.0, 92.0, 61);
    assert_eq!(
        v2_active_fetch_skip(CLAUDE_SLUG, &key, &due, 95.0, now),
        None
    );
    // A failed attempt within the provider's interval: honour its backoff.
    record_fetch_attempt_failed(&key, now - 10);
    assert_eq!(
        v2_active_fetch_skip(CLAUDE_SLUG, &key, &due, 95.0, now),
        Some("backoff")
    );
    reset_usage_fetch_tracker(&key);
}

#[test]
fn poll_merge_does_not_reflag_a_renewed_login() {
    let mut stored = claude_account("m@e.com", 3_600_000);
    stored.refresh_token = "renewed-rt".into();
    let mut snap = claude_account("m@e.com", 3_600_000);
    snap.refresh_token = "superseded-rt".into();
    snap.needs_relogin = true;
    merge_poll_update(&mut stored, &snap, None, None);
    assert!(!stored.needs_relogin);
    snap.refresh_token = "renewed-rt".into();
    merge_poll_update(&mut stored, &snap, None, None);
    assert!(stored.needs_relogin);
}

#[test]
fn poll_merge_keeps_newer_usage_and_expiry_warning() {
    let mut stored = claude_account("m@e.com", 3_600_000);
    let mut newer = cu(10.0, 10.0, 2000);
    newer.fetched_at = 2000;
    stored.cached_usage = Some(newer);
    stored.notif_state.login_expiry_warned = Some(42);
    let snap = claude_account("m@e.com", 3_600_000);
    let mut older = cu(50.0, 50.0, 2000);
    older.fetched_at = 1000;
    let ns = notifications::NotifState {
        pace_fired_this_window: true,
        ..Default::default()
    };
    merge_poll_update(&mut stored, &snap, Some(&older), Some(&ns));
    assert_eq!(stored.cached_usage.as_ref().unwrap().fetched_at, 2000);
    assert_eq!(stored.notif_state.login_expiry_warned, Some(42));
    assert!(stored.notif_state.pace_fired_this_window);
}

#[test]
fn poll_merge_replaces_a_reading_stamped_in_the_future() {
    // Stored under a clock that was 3 h ahead, then corrected: a genuine new
    // reading must replace it, not be refused for the whole offset.
    let now = chrono::Utc::now().timestamp();
    let mut stored = claude_account("m@e.com", 3_600_000);
    let mut future = cu(10.0, 10.0, 2000);
    future.fetched_at = now + 3 * 3600;
    stored.cached_usage = Some(future);
    let snap = claude_account("m@e.com", 3_600_000);
    let mut fresh = cu(50.0, 50.0, 2000);
    fresh.fetched_at = now;
    merge_poll_update(&mut stored, &snap, Some(&fresh), None);
    assert_eq!(stored.cached_usage.as_ref().unwrap().fetched_at, now);
}
