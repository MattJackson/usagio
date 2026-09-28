use super::*;
use chrono::{Duration, Utc};

// --- bar ---

#[test]
fn bar_none_is_dash() {
    assert_eq!(bar(None), "-");
}

#[test]
fn bar_zero_percent() {
    assert_eq!(bar(Some(0.0)), "[----------]   0%");
}

#[test]
fn bar_fifty_percent() {
    assert_eq!(bar(Some(50.0)), "[#####-----]  50%");
}

#[test]
fn bar_hundred_percent() {
    assert_eq!(bar(Some(100.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_over_hundred() {
    assert_eq!(bar(Some(250.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_negative() {
    assert_eq!(bar(Some(-5.0)), "[----------]   0%");
}

// --- truncate ---

#[test]
fn truncate_short_string_unchanged() {
    assert_eq!(truncate("hello", 10), "hello");
}

#[test]
fn truncate_exact_length_unchanged() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn truncate_long_string_adds_ellipsis() {
    let out = truncate("abcdefghij", 5);
    assert_eq!(out.chars().count(), 5);
    assert!(out.ends_with('…'));
    assert!(out.starts_with("abcd"));
}

// --- humanize_until ---

#[test]
fn humanize_until_past_is_now() {
    // Sub-minute / already-past reads "<1m", never "now"/"0m" — a countdown
    // flooring to 0 looked like it had already reset when it hadn't.
    assert_eq!(humanize_until(Utc::now() - Duration::hours(1)), "<1m");
    assert_eq!(humanize_until(Utc::now() + Duration::seconds(30)), "<1m");
}

#[test]
fn humanize_until_hours_and_minutes() {
    // 2h30m out (+ a few seconds of slack): shows hours and minutes.
    let s = humanize_until(Utc::now() + Duration::minutes(150) + Duration::seconds(5));
    assert_eq!(s, "2h 30m");
}

#[test]
fn humanize_until_days_and_hours() {
    let s = humanize_until(Utc::now() + Duration::hours(50) + Duration::seconds(5));
    assert_eq!(s, "2d 2h");
}

#[test]
fn humanize_until_minutes_only() {
    let s = humanize_until(Utc::now() + Duration::minutes(30) + Duration::seconds(5));
    assert_eq!(s, "30m");
}

// --- Row helpers ---

fn cell(pct: Option<f64>) -> Cell {
    Cell {
        pct,
        resets_at: None,
    }
}

fn row(session: Option<f64>, weekly: Option<f64>) -> Row {
    Row {
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
        email: "x@e.com".to_string(),
        session: cell(session),
        weekly: cell(weekly),
        opus: None,
        error: None,
        fetched_at: Some(0),
        reported: Default::default(),
    }
}

/// A row with an email and a weekly reset time, for pick/order tests.
fn row_full(email: &str, session: f64, weekly: f64, weekly_reset: DateTime<Utc>) -> Row {
    row_full_with_provider(CLAUDE_SLUG, email, session, weekly, weekly_reset)
}

/// Like `row_full`, but with an explicit provider slug — for tests that need
/// to verify the swap-capability gate on non-Claude / unregistered slugs.
fn row_full_with_provider(
    provider_id: &str,
    email: &str,
    session: f64,
    weekly: f64,
    weekly_reset: DateTime<Utc>,
) -> Row {
    Row {
        provider_id: provider_id.to_string(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
        email: email.to_string(),
        session: Cell {
            pct: Some(session),
            resets_at: None,
        },
        weekly: Cell {
            pct: Some(weekly),
            resets_at: Some(weekly_reset),
        },
        opus: None,
        error: None,
        fetched_at: Some(Utc::now().timestamp()),
        reported: Default::default(),
    }
}

#[test]
fn row_available_when_both_have_headroom() {
    assert!(row(Some(50.0), Some(80.0)).available());
}

#[test]
fn row_unavailable_when_session_maxed() {
    assert!(!row(Some(100.0), Some(10.0)).available());
}

#[test]
fn row_unavailable_when_weekly_maxed() {
    assert!(!row(Some(10.0), Some(100.0)).available());
}

#[test]
fn row_available_when_pct_unknown() {
    assert!(row(None, None).available());
}

#[test]
fn row_max_pct_takes_tightest() {
    assert_eq!(row(Some(30.0), Some(70.0)).max_pct(), 70.0);
    assert_eq!(row(Some(90.0), Some(20.0)).max_pct(), 90.0);
}

#[test]
fn row_headroom_is_complement_of_max() {
    assert_eq!(row(Some(30.0), Some(70.0)).headroom(), 30.0);
}

#[test]
fn row_has_data_tracks_fetched_at() {
    assert!(row(Some(10.0), Some(20.0)).has_data());
    let mut r = row(Some(10.0), Some(20.0));
    r.fetched_at = None;
    assert!(!r.has_data());
}

// --- age_str ---

#[test]
fn age_str_never_without_timestamp() {
    assert_eq!(age_str(None), "never");
}

#[test]
fn age_str_minutes_and_hours() {
    let now = Utc::now().timestamp();
    assert_eq!(age_str(Some(now - 120)), "2m ago");
    assert_eq!(age_str(Some(now - 7200)), "2h ago");
}

// --- cached_from_usage / row_from_account ---

#[test]
fn cached_from_usage_extracts_windows() {
    let u: usage::Usage = serde_json::from_str(
        r#"{"five_hour":{"utilization":9.0,"resets_at":"2026-09-05T08:00:00Z"},
            "seven_day":{"utilization":61.0,"resets_at":"2026-09-09T00:00:00Z"},
            "seven_day_opus":null}"#,
    )
    .unwrap();
    let c = cached_from_usage(&u);
    assert_eq!(c.session_pct, Some(9.0));
    assert_eq!(c.weekly_pct, Some(61.0));
    assert_eq!(c.session_reset.as_deref(), Some("2026-09-05T08:00:00Z"));
    assert!(c.opus_pct.is_none());
    assert!(c.fetched_at > 0);
    assert!(c.reported.is_empty());
}

#[test]
fn claude_and_generic_caches_carry_reported_usage_into_rows() {
    let u: usage::Usage = serde_json::from_value(serde_json::json!({
        "five_hour": { "utilization": 9.0 }, "seven_day": null, "seven_day_opus": null,
        "spend": { "used": { "amount_minor": 310, "currency": "USD", "exponent": 2 }, "enabled": true },
        "seven_day_breakdown": { "rows": [
            { "key": "claude_code", "display_name": "Claude Code", "percent": 82 },
            { "key": "chat", "display_name": "Chats", "percent": 18 }
        ]}
    }))
    .unwrap();
    // Claude: the usage-endpoint path.
    let c = cached_from_usage(&u);
    assert_eq!(c.reported, usage::reported_usage(&u));
    assert_eq!(c.reported.breakdown.len(), 2);
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.cached_usage = Some(c.clone());
    assert_eq!(row_from_account(&a).reported, c.reported);

    // Generic providers: the same shape through the snapshot mapping.
    let snap = UsageSnapshot {
        windows: vec![],
        fetched_at: Utc::now(),
        plan: None,
        reported: c.reported.clone(),
    };
    let g = cached_from_usage_snapshot(&snap);
    assert_eq!(g.reported, c.reported);
    let pa = ProviderAccount {
        key: "k@example.com".into(),
        secret_blob: String::new(),
        access_token: String::new(),
        refresh_token: String::new(),
        expires_at: 0,
        identity_email: None,
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: Some(g),
        notif_state: Default::default(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
    };
    assert_eq!(row_from_provider_account("codex", &pa).reported, c.reported);
}

#[test]
fn row_from_account_without_cache_has_no_data() {
    let a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    assert!(!row_from_account(&a).has_data());
}

// --- candidate_order / auto_pick tie-break (D1) ---

#[test]
fn candidate_order_prefers_more_headroom_on_equal_reset() {
    let reset = Utc::now() + Duration::hours(24);
    // a: 80% used (20% headroom); b: 10% used (90% headroom). Same reset.
    let a = row_full("a@e.com", 80.0, 80.0, reset);
    let b = row_full("b@e.com", 10.0, 10.0, reset);
    // b (more headroom) must sort BEFORE a.
    assert_eq!(candidate_order(&a, &b), std::cmp::Ordering::Greater);
    assert_eq!(candidate_order(&b, &a), std::cmp::Ordering::Less);
}

#[test]
fn auto_pick_tie_break_picks_higher_headroom() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("high@e.com", 80.0, 80.0, reset),
        row_full("low@e.com", 10.0, 10.0, reset),
    ];
    // Equal soonest reset → the account with MORE headroom (lower usage) wins.
    assert_eq!(
        auto_pick(&rows, TRIGGER_PCT, TRIGGER_PCT).unwrap(),
        "low@e.com"
    );
}

#[test]
fn auto_pick_prefers_soonest_reset() {
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    // The soonest-resetting account wins even with slightly less headroom.
    let rows = vec![
        row_full("later@e.com", 5.0, 5.0, later),
        row_full("soon@e.com", 40.0, 40.0, soon),
    ];
    assert_eq!(
        auto_pick(&rows, TRIGGER_PCT, TRIGGER_PCT).unwrap(),
        "soon@e.com"
    );
}

#[test]
fn auto_pick_skips_past_target_account_even_if_it_resets_soonest() {
    // The regression: a 99%-weekly account resetting soonest must NOT beat an
    // under-target account. "Best is not one past target."
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("nearlyfull@e.com", 1.0, 99.0, soon),
        row_full("empty@e.com", 0.0, 0.0, later),
    ];
    assert_eq!(
        auto_pick(&rows, TRIGGER_PCT, TRIGGER_PCT).unwrap(),
        "empty@e.com",
        "an account past the weekly trigger is never the best landing spot"
    );
}

#[test]
fn auto_pick_falls_back_to_least_full_when_all_past_target() {
    // "Unless it's all that's left": every account is past target, so the
    // least-consumed still-usable one wins rather than erroring out.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("full99@e.com", 1.0, 99.0, soon),
        row_full("full97@e.com", 1.0, 97.0, later),
    ];
    // Both past the 95% trigger → fallback tier. Soonest reset still wins.
    assert_eq!(
        auto_pick(&rows, TRIGGER_PCT, TRIGGER_PCT).unwrap(),
        "full99@e.com"
    );
}

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
fn menu_order_keeps_active_locked_account_in_rotation() {
    let now = Utc::now();
    // Active and locked: not sunk, so its sooner weekly reset puts it first.
    let mut locked = row_full("active@e.com", 100.0, 40.0, now + Duration::hours(6));
    locked.session.resets_at = Some(now + Duration::hours(1));
    let free = row_full("free@e.com", 30.0, 30.0, now + Duration::hours(24));
    let mut rows = [free, locked];
    rows.sort_by(|a, b| menu_order(a, b, Some("active@e.com"), now));
    assert_eq!(rows[0].email, "active@e.com");
    assert_eq!(rows[1].email, "free@e.com");
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

// --- choose_swap_target (auto-swap guard) ---

#[test]
fn choose_swap_target_moves_off_over_trigger_account() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_never_picks_lapsed_subscription() {
    let reset = Utc::now() + Duration::hours(24);
    let mut lapsed = row_full("lapsed@e.com", 0.0, 0.0, reset);
    lapsed.no_subscription = true;
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        lapsed,
        row_full("free@e.com", 50.0, 50.0, reset),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_moves_off_lapsed_active_account() {
    // The active account's plan lapsed while its frozen cache looks healthy —
    // it can't serve requests, so move off it.
    let reset = Utc::now() + Duration::hours(24);
    let mut active = row_full("active@e.com", 10.0, 10.0, reset);
    active.no_subscription = true;
    let rows = vec![active, row_full("free@e.com", 50.0, 50.0, reset)];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_stays_below_trigger_when_nothing_better() {
    // Active is healthy AND already resets soonest, so no candidate is a better
    // place to be — the proactive path must not swap sideways.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, soon),
        row_full("free@e.com", 20.0, 20.0, later),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_proactively_flips_back_to_sooner_reset() {
    // Active is healthy (below trigger) but a freed-up account resets its weekly
    // window sooner — use-it-or-lose-it says flip back to it.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("fresh@e.com", 10.0, 10.0, soon),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("fresh@e.com"));
}

#[test]
fn choose_swap_target_no_proactive_swap_within_headroom_margin() {
    // Equal weekly reset and only a small headroom lead (< PROACTIVE_HEADROOM_MARGIN)
    // must not trigger a proactive swap, or two near-equal accounts would ping-pong.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 30.0, 30.0, reset),
        row_full("free@e.com", 25.0, 25.0, reset), // 5-point lead, under the margin
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_proactive_swap_beyond_headroom_margin() {
    // Equal weekly reset but a large headroom lead (>= margin) is worth the swap.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 70.0, 70.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset), // 50-point lead
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("free@e.com"));
}

#[test]
fn choose_swap_target_proactive_respects_no_return_window() {
    // Even when a sooner-resetting account would be a better place to be, the
    // no-return window still excludes an account we just left.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("fresh@e.com", 10.0, 10.0, soon),
    ];
    let mut left_at = std::collections::HashMap::new();
    left_at.insert("fresh@e.com".to_string(), std::time::Instant::now());
    let guard = SwapGuard {
        left_at,
        ..SwapGuard::default()
    };
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_cooldown_blocks_only_optional_swaps() {
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let guard = SwapGuard {
        last_swap: Some(std::time::Instant::now()),
        ..SwapGuard::default()
    };
    // Proactive flip-back (active healthy, a sooner-resetting account freed
    // up) → just swapped, so the cooldown holds it.
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("free@e.com", 10.0, 10.0, soon),
    ];
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 95.0, &guard).is_none());
    // Active over the trigger → leaving is urgent; a recent swap must not
    // hold it there (a just-landed 94% account would run out in the wait).
    let rows = vec![
        row_full("active@e.com", 96.0, 50.0, later),
        row_full("free@e.com", 20.0, 20.0, soon),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard);
    assert!(eval.urgent);
    assert_eq!(eval.target.as_deref(), Some("free@e.com"));
}

#[test]
fn first_429_near_trigger_swaps_immediately_even_after_a_recent_swap() {
    // Replay of the dev@ incident: 93% session, trigger 95, one 429, and the
    // previous swap only moments ago. Must move now, not after a backoff.
    let email = escalation_test_email("replay");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    let reset = Utc::now() + Duration::hours(24);
    let mut active = row_full(&email, 93.0, 76.0, reset);
    active.fetched_at = Some(Utc::now().timestamp());
    let rows = vec![
        active,
        row_full("dev4@e.com", 24.0, 9.0, reset + Duration::hours(24)),
    ];
    let guard = SwapGuard {
        last_swap: Some(std::time::Instant::now()),
        ..SwapGuard::default()
    };
    let eval = evaluate_swap(&rows, &email, 95.0, 95.0, &guard);
    assert!(eval.urgent);
    assert_eq!(eval.target.as_deref(), Some("dev4@e.com"));
    reset_usage_fetch_tracker(&email);
}

#[test]
fn swap_target_ceiling_follows_the_trigger() {
    let reset = Utc::now() + Duration::hours(24);
    // Trigger 95, ceiling = trigger: a 90% session is a usable target now
    // (the old fixed 85% stranded it).
    let rows = vec![
        row_full("active@e.com", 96.0, 50.0, reset),
        row_full("ninety@e.com", 90.0, 50.0, reset),
    ];
    let guard = SwapGuard::default();
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 95.0, &guard).as_deref(),
        Some("ninety@e.com")
    );
    // Trigger 80: an 84% session is already past it — never a target, even
    // with a looser ceiling passed in.
    let rows = vec![
        row_full("active@e.com", 81.0, 50.0, reset),
        row_full("over@e.com", 84.0, 10.0, reset),
    ];
    assert!(choose_swap_target(&rows, "active@e.com", 80.0, 85.0, &guard).is_none());
}

#[test]
fn left_account_needs_a_reading_taken_after_leaving() {
    // We left back@ moments ago with a 93% reading taken before leaving. Even
    // once the no-return window has passed, that pre-leave number can't make
    // it a target — only a reading taken after we left can.
    let reset = Utc::now() + Duration::hours(24);
    let mut back = row_full("back@e.com", 50.0, 50.0, reset);
    let left = std::time::Instant::now();
    let left_ts = Utc::now().timestamp();
    let mut left_at = std::collections::HashMap::new();
    left_at.insert("back@e.com".to_string(), left);
    let guard = SwapGuard {
        left_at,
        ..SwapGuard::default()
    };
    back.fetched_at = Some(left_ts - 60);
    assert!(!fresh_since_left(&back, &guard));
    back.fetched_at = Some(left_ts + 60);
    assert!(fresh_since_left(&back, &guard));
    // An account we never left is unaffected.
    let other = row_full("other@e.com", 50.0, 50.0, reset);
    assert!(fresh_since_left(&other, &guard));
}

#[test]
fn choose_swap_target_skips_target_whose_provider_is_unregistered() {
    // A row tagged with a provider slug that isn't in the registry (a stub /
    // reporting-only agent that phase 4+ hasn't wired up yet) must NOT be
    // selected as a swap target, even if its usage looks great.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full_with_provider("no-such-provider", "free@e.com", 5.0, 5.0, reset),
    ];
    let guard = SwapGuard::default();
    // Only candidate was filtered by the capability gate → no swap.
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_still_picks_claude_target_after_capability_filter() {
    // Regression guard for the capability filter: adding it must not have
    // stopped Claude accounts (the only registered v1 provider) from being
    // chosen. Baseline swap decision unchanged.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).as_deref(),
        Some("free@e.com")
    );
}

#[test]
fn provider_supports_swap_reflects_registered_claude() {
    // Claude is registered, has both usage and switching → swappable.
    assert!(provider_supports_swap(CLAUDE_SLUG));
    // Unknown slugs are treated as non-candidates (safest default).
    assert!(!provider_supports_swap("nope"));
}

#[test]
fn row_from_account_tags_provider_id_claude() {
    // Every v1-migrated row must carry the "claude" slug so the swap gate
    // recognizes it. Phase 3 (state v2) replaces this with a bucket lookup.
    let a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    assert_eq!(row_from_account(&a).provider_id, CLAUDE_SLUG);
}

#[test]
fn choose_swap_target_skips_ceiling_and_maxed_targets() {
    let reset = Utc::now() + Duration::hours(24);
    let guard = SwapGuard::default();
    // A maxed account is never a target, not even as the all-full fallback.
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("maxed@e.com", 100.0, 50.0, reset),
    ];
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
    // Over an explicit 85% ceiling it isn't a normal target, but with the
    // active account at the trigger and nothing else available, the all-full
    // fallback still moves to it: 10 points of room beats 4.
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("alsohigh@e.com", 90.0, 90.0, reset),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert!(eval.fallback);
    assert_eq!(eval.target.as_deref(), Some("alsohigh@e.com"));
}

#[test]
fn choose_swap_target_excludes_recently_left_account() {
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    // We just left free@e.com — the no-return window must exclude it, so with no
    // other candidate there's nothing to swap to.
    let mut left_at = std::collections::HashMap::new();
    left_at.insert("free@e.com".to_string(), std::time::Instant::now());
    let guard = SwapGuard {
        left_at,
        ..SwapGuard::default()
    };
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_returns_to_high_weekly_fresh_session_account() {
    // The account we want to finish draining: weekly is high (88%, above the old
    // 85% max_pct ceiling) but its session just reset, so it's a valid target and
    // we should flip back to keep spending its weekly before it resets.
    let soon = Utc::now() + Duration::hours(2);
    let later = Utc::now() + Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, later),
        row_full("draining@e.com", 5.0, 88.0, soon),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("draining@e.com"));
}

#[test]
fn choose_swap_target_skips_target_whose_weekly_hit_trigger() {
    // Fresh session but weekly already at the trigger → landing there would
    // immediately want to swap away again, so it's not a valid target.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 40.0, reset),
        row_full("spent@e.com", 5.0, 96.0, reset),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

// --- env-override guard (CLAUDE_CODE_OAUTH_TOKEN) ---

#[test]
fn choose_swap_target_skips_env_overridden_provider_end_to_end() {
    // Active is over the trigger and there IS a healthy candidate — without
    // an env-override, we'd swap to it. With `CLAUDE_CODE_OAUTH_TOKEN`
    // active on the Claude provider, `claude` ignores whatever token we
    // write, so any swap is a silent no-op and `watch_cycle` must skip it.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full("free@e.com", 20.0, 20.0, reset),
    ];
    let guard = SwapGuard::default();

    // Baseline sanity: with no override, the swap fires.
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).as_deref(),
        Some("free@e.com"),
        "baseline: without env-override the auto-swap picks free@e.com",
    );

    // With the override active for `claude`, both the active row's provider
    // and every candidate row's provider are gated off → no swap.
    with_env_override_hook(&[CLAUDE_SLUG], || {
        assert!(
            choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none(),
            "env-override active: watch_cycle must NOT swap claude accounts",
        );
    });
}

#[test]
fn env_override_active_reads_shared_slug_map() {
    // The hook drives both the menu (via menubar::env_override_for) and the
    // swap filter — one source of truth for the whole app.
    assert!(!env_override_active(CLAUDE_SLUG));
    with_env_override_hook(&[CLAUDE_SLUG], || {
        assert!(env_override_active(CLAUDE_SLUG));
        // Unknown / non-Claude slugs are unaffected by the Claude env var.
        assert!(!env_override_active("codex"));
    });
    // Restored on exit.
    assert!(!env_override_active(CLAUDE_SLUG));
}

// --- next_interval (backoff + adaptive cadence) ---

const TRIGGER_FOR_TESTS: f64 = 95.0;

// All calls pass `actionable` explicitly. It only affects the at/above-trigger
// arm; below the trigger the value is irrelevant, so these pre-v0.5.17 cases
// pass `true` to preserve their original intent.
#[test]
fn next_interval_resets_to_base_when_not_limited_and_far_from_trigger() {
    // A clean cycle with everyone comfortably below the warning band
    // returns to the base cadence, even from a backed-off value.
    assert_eq!(
        next_interval(600, 60, false, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    assert_eq!(
        next_interval(60, 60, false, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // No usage data yet → also base cadence.
    assert_eq!(
        next_interval(60, 60, false, None, TRIGGER_FOR_TESTS, true),
        60
    );
}

#[test]
fn next_interval_doubles_on_rate_limit_capped() {
    // Rate-limit override wins over adaptive cadence.
    assert_eq!(
        next_interval(60, 60, true, Some(50.0), TRIGGER_FOR_TESTS, true),
        120
    );
    // Doubles the CURRENT interval, not base: one 429 at the 30s tier must
    // not jump to 2×base (the dev@ incident went 30s → 360s and ran out).
    assert_eq!(
        next_interval(30, 180, true, Some(93.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // Floored at the TIGHT interval if `current` was stale-small.
    assert_eq!(
        next_interval(1, 60, true, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // Capped at the max.
    assert_eq!(
        next_interval(
            WATCH_MAX_INTERVAL_SECS,
            60,
            true,
            Some(99.0),
            TRIGGER_FOR_TESTS,
            true
        ),
        WATCH_MAX_INTERVAL_SECS
    );
}

#[test]
fn next_interval_tightens_progressively_across_ramp_tiers() {
    // v0.7.3 replaced the binary WARNING band (30s inside 15pt of trigger)
    // with a 4-tier ramp. At trigger=95: TIGHT = [90, 95), MIDDLE = [85, 90),
    // RELAXED = [75, 85), BASE below. The v0.4.3 user miss (94% + 150s wait
    // → lock before next poll) still tightens to 30s TIGHT here.

    // TIGHT band [trigger-5, trigger): 30s regardless of `current`.
    assert_eq!(
        next_interval(180, 180, false, Some(94.9), TRIGGER_FOR_TESTS, true),
        WATCH_TIGHT_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(90.0), TRIGGER_FOR_TESTS, true),
        WATCH_TIGHT_INTERVAL_SECS,
        "trigger-5 lower edge is inside TIGHT"
    );

    // MIDDLE band [trigger-10, trigger-5): 60s. dev3 at 87% today — the
    // account whose stale 429-preserved cache silenced auto-swap in v0.7.2 —
    // lands here.
    assert_eq!(
        next_interval(180, 180, false, Some(89.9), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(87.0), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS,
        "the exact case from the v0.7.2 429 loop"
    );
    assert_eq!(
        next_interval(180, 180, false, Some(85.0), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS,
        "trigger-10 lower edge is inside MIDDLE"
    );

    // RELAXED band [trigger-20, trigger-10): 120s.
    assert_eq!(
        next_interval(180, 180, false, Some(84.9), TRIGGER_FOR_TESTS, true),
        WATCH_RELAXED_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(75.0), TRIGGER_FOR_TESTS, true),
        WATCH_RELAXED_INTERVAL_SECS,
        "trigger-20 lower edge is inside RELAXED"
    );

    // Below the ramp: BASE.
    assert_eq!(
        next_interval(180, 180, false, Some(74.9), TRIGGER_FOR_TESTS, true),
        180
    );
    assert_eq!(
        next_interval(180, 180, false, Some(20.0), TRIGGER_FOR_TESTS, true),
        180
    );

    // The whole ramp ignores `actionable` — tighten to catch the crossing
    // even when no swap target exists yet.
    assert_eq!(
        next_interval(180, 180, false, Some(90.0), TRIGGER_FOR_TESTS, false),
        WATCH_TIGHT_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(87.0), TRIGGER_FOR_TESTS, false),
        WATCH_MIDDLE_INTERVAL_SECS
    );
}

#[test]
fn next_interval_ramp_slides_with_custom_trigger() {
    // The whole ramp is defined as absolute percentage-point offsets from
    // the user-configurable trigger, so trigger=70 and trigger=98 both work
    // without a code change. This is why we care about the sliding property.

    // trigger=70: TIGHT [65,70), MIDDLE [60,65), RELAXED [50,60).
    assert_eq!(next_interval(180, 180, false, Some(66.0), 70.0, true), 30);
    assert_eq!(next_interval(180, 180, false, Some(62.0), 70.0, true), 60);
    assert_eq!(next_interval(180, 180, false, Some(55.0), 70.0, true), 120);
    assert_eq!(next_interval(180, 180, false, Some(45.0), 70.0, true), 180);

    // trigger=98: TIGHT [93,98), MIDDLE [88,93), RELAXED [78,88).
    assert_eq!(next_interval(180, 180, false, Some(95.0), 98.0, true), 30);
    assert_eq!(next_interval(180, 180, false, Some(90.0), 98.0, true), 60);
    assert_eq!(next_interval(180, 180, false, Some(80.0), 98.0, true), 120);
    assert_eq!(next_interval(180, 180, false, Some(60.0), 98.0, true), 180);
}

// --- v0.8.0 smart-cadence tighten (projection can only speed up cadence) ---

#[test]
fn tighten_tier_once_walks_the_ramp_one_step_and_caps_at_tight() {
    // Each call takes us exactly one tier tighter (BASE → RELAXED → MIDDLE →
    // TIGHT), and further calls at TIGHT are idempotent — projection can
    // never make us poll faster than the static ramp's tightest tier.
    let base = 180u64;
    assert_eq!(tighten_tier_once(base, base), WATCH_RELAXED_INTERVAL_SECS);
    assert_eq!(
        tighten_tier_once(WATCH_RELAXED_INTERVAL_SECS, base),
        WATCH_MIDDLE_INTERVAL_SECS
    );
    assert_eq!(
        tighten_tier_once(WATCH_MIDDLE_INTERVAL_SECS, base),
        WATCH_TIGHT_INTERVAL_SECS
    );
    assert_eq!(
        tighten_tier_once(WATCH_TIGHT_INTERVAL_SECS, base),
        WATCH_TIGHT_INTERVAL_SECS,
        "at TIGHT already — projection can't tighten further"
    );
}

#[test]
fn floor_with_projection_tightens_only_when_projected_crosses_trigger() {
    // Base case: no projection → identical to the plain floor.
    let t = TRIGGER_FOR_TESTS;
    let base = 180u64;
    assert_eq!(
        per_account_fetch_floor_secs_with_projection(Some(50.0), t, base, None),
        per_account_fetch_floor_secs(Some(50.0), t, base)
    );

    // Static tier says BASE (comfortable at 50%) but the projection says
    // we'll cross trigger before the next fetch → tighten one step (RELAXED).
    // This is exactly the fast-burn case a static ramp misses.
    assert_eq!(
        per_account_fetch_floor_secs_with_projection(Some(50.0), t, base, Some(95.0)),
        WATCH_RELAXED_INTERVAL_SECS,
        "50% with a projection crossing trigger tightens BASE→RELAXED"
    );

    // A projection below trigger doesn't tighten anything.
    assert_eq!(
        per_account_fetch_floor_secs_with_projection(Some(50.0), t, base, Some(94.9)),
        base
    );

    // Already at TIGHT — projection can't make us any tighter, but also
    // doesn't LOOSEN us. Projection is upper-bound-only.
    assert_eq!(
        per_account_fetch_floor_secs_with_projection(Some(93.0), t, base, Some(200.0)),
        WATCH_TIGHT_INTERVAL_SECS
    );
}

// --- v0.8.0 auto-swap escalation on repeated /oauth/usage 429s ---

fn active_row(email: &str, session: f64, weekly: f64) -> Row {
    let mut r = row_for(email, Some(session), Some(weekly));
    // `has_data()` requires fetched_at Some AND error None — mimic a real cached
    // row so the escalation function's guard clauses see valid input.
    r.fetched_at = Some(Utc::now().timestamp());
    r
}

/// Tests share the process-global `USAGE_FETCH_TRACKERS` map; use unique emails
/// per test so state can't cross-contaminate. Reset the caller's entry at the
/// end of each test to keep the map bounded even in long test-runner sessions.
fn escalation_test_email(tag: &str) -> String {
    format!(
        "escalation-{tag}-{}-{}@test",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[test]
fn escalation_no_429s_returns_raw_max_pct() {
    let email = escalation_test_email("no429s");
    let row = active_row(&email, 94.0, 30.0);
    // No tracker state → not escalated → raw value returned.
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert_eq!(eff, 94.0);
    reset_usage_fetch_tracker(&email);
}

#[test]
fn escalation_fires_on_first_429_in_tight_band() {
    // The dev@ incident: 93% with a 95% trigger, one 429. Waiting for a second
    // strike meant waiting out a backoff while the account ran out, so a
    // single 429 in the TIGHT band must already fire the swap.
    let email = escalation_test_email("fires");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    let row = active_row(&email, 93.0, 76.0);
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert!(
        eff >= 95.0,
        "one 429 in the tight band must escalate, got {eff}"
    );
    reset_usage_fetch_tracker(&email);
}

#[test]
fn escalation_ignored_when_cached_pct_is_far_below_trigger() {
    let email = escalation_test_email("far_below");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    record_usage_fetch_429(&email);
    // 75% is inside the RELAXED band (trigger-20) but OUTSIDE the TIGHT
    // band (trigger-5). Escalation gates on TIGHT to avoid firing on
    // tenant-wide 429 noise when this account isn't actually near limit
    // (Opus adversarial review BAD 3).
    let row = active_row(&email, 75.0, 30.0);
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert_eq!(
        eff, 75.0,
        "escalation must NOT fire outside TIGHT band, got {eff}"
    );
    reset_usage_fetch_tracker(&email);
}

#[test]
fn escalation_counter_resets_on_success() {
    let email = escalation_test_email("reset_on_success");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    record_usage_fetch_429(&email);
    // At this point escalation WOULD fire.
    record_usage_fetch_success(&email);
    let row = active_row(&email, 94.0, 30.0);
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert_eq!(eff, 94.0, "a successful fetch must reset the counter");
    reset_usage_fetch_tracker(&email);
}

#[test]
fn escalation_counter_resets_on_non_429_error() {
    let email = escalation_test_email("reset_on_non_429");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    // A network timeout (or any non-RateLimited error) after a 429 clears the
    // signal — those failures don't mean "the server is asking us to back
    // off" (Sonnet adversarial review BAD 4).
    record_usage_fetch_non_429(&email);
    let row = active_row(&email, 94.0, 30.0);
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert_eq!(eff, 94.0, "non-429 error must reset the consecutive count");
    reset_usage_fetch_tracker(&email);
}

#[test]
fn escalation_reset_tracker_clears_all_state() {
    let email = escalation_test_email("clear_all");
    record_usage_fetch_success(&email);
    record_usage_fetch_429(&email);
    record_usage_fetch_429(&email);
    reset_usage_fetch_tracker(&email);
    // After a switch-away (which calls `reset_usage_fetch_tracker`), the
    // account starts fresh — a returning-active account can't re-escalate
    // instantly on a stale count (Opus adversarial review BAD 4).
    let row = active_row(&email, 94.0, 30.0);
    let eff = effective_active_max_pct_for_swap_fire(&row, 95.0);
    assert_eq!(eff, 94.0);
}

// --- per_account_fetch_floor_secs (v0.7.3 rate-limit guard) ---

#[test]
fn per_account_fetch_floor_matches_the_cadence_ramp() {
    // The per-account fetch floor uses the same ramp as `next_interval`, so
    // an account at 87% has a 60s floor even when the GLOBAL loop wakes every
    // 30s because a *different* account is in the TIGHT band. This is the
    // guard that stops a single at-trigger account from dragging every other
    // account down to 120 req/hr/account against /oauth/usage.
    let t = TRIGGER_FOR_TESTS;
    assert_eq!(per_account_fetch_floor_secs(Some(92.0), t, 180), 30);
    assert_eq!(per_account_fetch_floor_secs(Some(87.0), t, 180), 60);
    assert_eq!(per_account_fetch_floor_secs(Some(80.0), t, 180), 120);
    assert_eq!(per_account_fetch_floor_secs(Some(50.0), t, 180), 180);
    // At-or-above trigger stays on TIGHT so a stale 429-preserved cache
    // refreshes as soon as the endpoint recovers.
    assert_eq!(per_account_fetch_floor_secs(Some(99.0), t, 180), 30);
    // No cached usage yet → BASE (need at least one fetch to place the
    // account on a tier).
    assert_eq!(per_account_fetch_floor_secs(None, t, 180), 180);
}

#[test]
fn next_interval_tightens_to_backstop_at_or_above_trigger_when_actionable() {
    // At or above the trigger AND a swap is actionable: BACKSTOP cadence.
    // The auto-swap should already have fired; this makes sure a transient
    // failure or a soon-clearing cooldown doesn't leave us blind for a full
    // base cycle. Floor is 30s (never faster) — see WATCH_BACKSTOP_INTERVAL_SECS.
    assert_eq!(
        next_interval(150, 150, false, Some(95.0), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(150, 150, false, Some(99.9), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(150, 150, false, Some(100.0), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
}

#[test]
fn next_interval_stays_at_base_at_or_above_trigger_when_not_actionable() {
    // v0.5.17: a maxed ACTIVE account with NO eligible swap target (single
    // account, or every other account full/needs_relogin) must NOT pin the
    // 10s backstop — that only hammers the usage endpoint into HTTP 429 for a
    // week with nothing to catch. Fall back to BASE; the reset-boundary cap
    // still wakes us near the reset.
    assert_eq!(
        next_interval(150, 150, false, Some(95.0), TRIGGER_FOR_TESTS, false),
        150
    );
    assert_eq!(
        next_interval(150, 150, false, Some(100.0), TRIGGER_FOR_TESTS, false),
        150
    );
    // Rate-limit backoff still overrides regardless of actionable.
    assert_eq!(
        next_interval(150, 150, true, Some(100.0), TRIGGER_FOR_TESTS, false),
        300
    );
}

// --- cadence_max_pct (v0.5.13: cadence scoped to the active account) ---

/// Like `row()` but with a caller-chosen email so active-scoping can be tested.
fn row_for(email: &str, session: Option<f64>, weekly: Option<f64>) -> Row {
    Row {
        email: email.to_string(),
        ..row(session, weekly)
    }
}

#[test]
fn cadence_max_pct_folds_weekly_not_just_session() {
    // Regression case from the v0.5.2 addendum, preserved: the ACTIVE account
    // at session=0%, weekly=99%, trigger=95% must fold to Some(99.0) — a
    // weekly-only approach to the trigger must still tighten cadence.
    let rows = vec![row_for("active@e.com", Some(0.0), Some(99.0))];
    let m = cadence_max_pct(&rows, Some("active@e.com"));
    assert_eq!(m, Some(99.0));
    assert_eq!(
        next_interval(150, 150, false, m, TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS,
        "active session=0/weekly=99 at trigger=95 must hit BACKSTOP cadence"
    );
}

#[test]
fn cadence_max_pct_ignores_inactive_maxed_accounts() {
    // The v0.5.13 fix: a healthy ACTIVE account and an inactive account pinned
    // at 100% weekly must NOT pin the cadence at the 10s backstop. Only the
    // active account (15%) drives cadence → BASE, not BACKSTOP.
    let rows = vec![
        row_for("active@e.com", Some(15.0), Some(40.0)),
        row_for("maxed@e.com", Some(100.0), Some(100.0)),
    ];
    let m = cadence_max_pct(&rows, Some("active@e.com"));
    assert_eq!(m, Some(40.0));
    assert_eq!(
        next_interval(150, 150, false, m, TRIGGER_FOR_TESTS, true),
        150,
        "a healthy active account keeps BASE cadence despite an inactive 100% account"
    );
}

#[test]
fn cadence_max_pct_none_without_active_or_data() {
    let rows = vec![row_for("a@e.com", Some(80.0), Some(80.0))];
    // No active account set.
    assert_eq!(cadence_max_pct(&rows, None), None);
    // Active account present but no usage data yet.
    let mut no_data = row_for("a@e.com", Some(80.0), Some(80.0));
    no_data.fetched_at = None;
    assert_eq!(cadence_max_pct(&[no_data], Some("a@e.com")), None);
    // Active email doesn't match any row.
    assert_eq!(cadence_max_pct(&rows, Some("ghost@e.com")), None);
}

// --- identity_matches (keychain adoption gate) ---

#[test]
fn identity_matches_by_uuid_when_both_present() {
    assert!(identity_matches(
        Some("u1"),
        Some("a@e.com"),
        Some("u1"),
        Some("b@e.com")
    ));
    // UUID mismatch wins over an email match — a different account is logged in.
    assert!(!identity_matches(
        Some("u1"),
        Some("a@e.com"),
        Some("u2"),
        Some("a@e.com")
    ));
}

#[test]
fn identity_matches_falls_back_to_email_case_insensitive() {
    assert!(identity_matches(
        None,
        Some("A@E.com"),
        None,
        Some("a@e.com")
    ));
    assert!(!identity_matches(
        None,
        Some("a@e.com"),
        None,
        Some("other@e.com")
    ));
}

#[test]
fn identity_matches_adopts_when_account_has_no_identity() {
    // No known identity yet → adopt (self-heal).
    assert!(identity_matches(None, None, None, Some("a@e.com")));
    // But a known email with nothing to compare against → stay safe, skip.
    assert!(!identity_matches(None, Some("a@e.com"), None, None));
}

// --- write_bytes_atomic_mode (rollback mode preservation) ---

#[cfg(unix)]
#[test]
fn write_bytes_atomic_mode_applies_requested_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // Use a mode DISTINCT from write_private's hardcoded 0o600 default, so this
    // proves the `mode` argument controls the final mode (not write_private's
    // default) — deleting the set_permissions call would make this fail.
    let path = dir.path().join("claude.json");
    write_bytes_atomic_mode(&path, b"{\"x\":1}", 0o640).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"x\":1}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o640,
        "the requested mode must be applied to the final file"
    );
    // No temp file left behind.
    assert!(!path.with_extension("json.usagio.tmp").exists());
}

// --- consumption_deltas (report same-account guard) ---

fn sample(account: &str, session: f64, ts: i64) -> Sample {
    Sample {
        ts,
        account: Some(account.to_string()),
        active: Some(true),
        session: Some(session),
        weekly: None,
        event: None,
    }
}

#[test]
fn consumption_deltas_only_counts_same_account_increases() {
    let a1 = sample("a@e.com", 10.0, 100);
    let a2 = sample("a@e.com", 40.0, 200); // +30, same account -> counted
    let b1 = sample("b@e.com", 5.0, 300); // a->b switch -> NOT counted
    let b2 = sample("b@e.com", 20.0, 400); // +15, same account -> counted
    let a3 = sample("a@e.com", 45.0, 500); // b->a switch -> NOT counted
    let list: Vec<&Sample> = vec![&a1, &a2, &b1, &b2, &a3];
    assert_eq!(consumption_deltas(&list), vec![(200, 30.0), (400, 15.0)]);
}

#[test]
fn consumption_deltas_ignores_negative_deltas() {
    // A reset (session drops) is not consumption.
    let a1 = sample("a@e.com", 90.0, 100);
    let a2 = sample("a@e.com", 5.0, 200); // reset, negative -> skipped
    let list: Vec<&Sample> = vec![&a1, &a2];
    assert!(consumption_deltas(&list).is_empty());
}

// --- merged_cached_usage (re-capture preserves usage) ---

fn acct_with_cache(cache: Option<CachedUsage>) -> Account {
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.cached_usage = cache;
    a
}

#[test]
fn merged_cached_usage_prefers_existing_snapshot() {
    let existing = acct_with_cache(Some(CachedUsage {
        session_pct: Some(42.0),
        weekly_pct: Some(61.0),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: 123,
        ..Default::default()
    }));
    let merged = merged_cached_usage(Some(&existing));
    assert_eq!(merged.unwrap().session_pct, Some(42.0));
}

#[test]
fn merged_cached_usage_none_for_new_account() {
    assert!(merged_cached_usage(None).is_none());
}

// --- rotate_if_large (shared log rotation) ---

#[test]
fn rotate_if_large_rotates_over_threshold_with_correct_name() {
    let dir = tempfile::tempdir().unwrap();
    for (name, rotated) in [
        ("usagio.log", "usagio.log.1"),
        ("history.jsonl", "history.jsonl.1"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"0123456789").unwrap();
        logging::rotate_if_large(&path, 5); // 10 bytes > 5 -> rotate
        assert!(!path.exists(), "{name} should have been moved aside");
        assert!(
            dir.path().join(rotated).exists(),
            "expected rotated file {rotated}"
        );
    }
}

#[test]
fn rotate_if_large_leaves_small_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usagio.log");
    std::fs::write(&path, b"tiny").unwrap();
    logging::rotate_if_large(&path, 1_000_000);
    assert!(path.exists());
    assert!(!dir.path().join("usagio.log.1").exists());
}

// --- needs_relogin filtering in the swap picker ---

/// Construct a row with the needs_relogin flag set. Everything else mirrors
/// `row_full` so we can build side-by-side "both would otherwise win" tests.
fn row_full_flagged(email: &str, session: f64, weekly: f64, weekly_reset: DateTime<Utc>) -> Row {
    let mut r = row_full(email, session, weekly, weekly_reset);
    r.needs_relogin = true;
    r
}

#[test]
fn choose_swap_target_skips_needs_relogin_candidate() {
    // The would-be candidate has the flag: even though it's healthier and
    // would win auto-pick, choose_swap_target must not return it. With only
    // one candidate and it's flagged, we get back None.
    let reset = Utc::now() + chrono::Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full_flagged("dead@e.com", 10.0, 10.0, reset),
    ];
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
}

#[test]
fn choose_swap_target_prefers_unflagged_candidate_over_flagged_winner() {
    // The flagged candidate would win by auto-pick priority (soonest reset,
    // more headroom), but must be filtered out; the unflagged runner-up
    // wins instead.
    let soon = Utc::now() + chrono::Duration::hours(2);
    let later = Utc::now() + chrono::Duration::hours(48);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, later),
        row_full_flagged("dead@e.com", 10.0, 10.0, soon),
        row_full("ok@e.com", 20.0, 20.0, later),
    ];
    let guard = SwapGuard::default();
    let target = choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert_eq!(target.as_deref(), Some("ok@e.com"));
}

#[test]
fn row_from_account_propagates_needs_relogin_flag() {
    let blob = r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#;
    let mut a = Account::from_keychain_blob(blob).unwrap();
    a.email = Some("x@e.com".into());
    assert!(!row_from_account(&a).needs_relogin);
    a.needs_relogin = true;
    assert!(row_from_account(&a).needs_relogin);
}

// -----------------------------------------------------------------------------
// switch_to_guarded lock-closure invariant (H1 round-1 + H_R2_1 round-2)
//
// switch_to_guarded's contract is: any in-memory mutation to the state
// snapshot that happens BEFORE `st = State::load()?;` (the reload that
// absorbs `absorb_before_switch`'s disk writes) is DISCARDED and must not be
// relied on. Mutations AFTER the reload survive `st.save()`.
//
// This pins that contract at the state-lock/save layer without dragging in
// the keychain + apply_account plumbing. Red-before-green: swap the order
// (mutate after reload → mutate before reload) and this test fails.
// -----------------------------------------------------------------------------
#[test]
fn switch_lock_closure_invariant_mutations_after_reload_survive() {
    use crate::credentials::with_state_lock;
    use crate::store::{Account, ScopedConfigDir, State};

    let _g = ScopedConfigDir::new();

    // Seed: two accounts, A active with a "stale" access token.
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"stale","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.email = Some("a@e.com".into());
    let mut b = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"bt","refreshToken":"br","expiresAt":0}}"#,
    )
    .unwrap();
    b.email = Some("b@e.com".into());
    let mut seed = State::default();
    seed.accounts.push(a);
    seed.accounts.push(b);
    seed.active = Some("a@e.com".into());
    seed.save().unwrap();

    // Model switch_to_guarded's ordering: (1) load, (2) simulate
    // absorb_before_switch by writing a fresher token for A on disk via a
    // NESTED with_state_lock (matches the real reentrant absorb path), (3)
    // reload, (4) mutate AFTER reload, (5) save.
    with_state_lock(|| {
        let _st = State::load()?;

        // Nested lock frame simulates absorb_before_switch's on-disk write
        // for the outgoing account A.
        with_state_lock(|| {
            let mut st_nested = State::load()?;
            if let Some(x) = st_nested.find_mut("a@e.com") {
                x.access_token = "absorbed_fresh".into();
                x.expires_at = 999_999;
            }
            st_nested.save()
        })?;

        // Reload — pre-reload mutations would be discarded here.
        let mut st = State::load()?;
        // Post-reload mutation: bump B's expiry as a stand-in for what
        // sync_active_from_keychain / apply_account bookkeeping do.
        if let Some(x) = st.find_mut("b@e.com") {
            x.expires_at = 111_111;
        }
        st.active = Some("b@e.com".into());
        st.save()
    })
    .unwrap();

    // A's absorbed rotation survived (fresher token persisted, not clobbered).
    let disk = State::load().unwrap();
    let a_on_disk = disk.find("a@e.com").expect("A still present");
    assert_eq!(
        a_on_disk.access_token, "absorbed_fresh",
        "reload+save must preserve the fresher token absorbed for the outgoing account (H1)"
    );
    // And B's post-reload mutation persisted.
    let b_on_disk = disk.find("b@e.com").expect("B still present");
    assert_eq!(
        b_on_disk.expires_at, 111_111,
        "post-reload mutations must survive the final save (H_R2_1 sibling)"
    );
    assert_eq!(disk.active.as_deref(), Some("b@e.com"));
}

// --- launch_agent_exe_path / sibling_app_bundle_exe ---

// Used only by macOS launch-agent / bundle tests below (all `#[cfg(target_os = "macos")]`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mock_cellar_bundle_layout(
    prefix: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let version_dir = dir.join("Cellar/usagio/0.9.9");
    let bin_dir = version_dir.join("bin");
    let bundle_macos_dir = version_dir.join("usagio.app/Contents/MacOS");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&bundle_macos_dir).unwrap();

    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();
    let bundle_exe = bundle_macos_dir.join("usagio");
    std::fs::write(&bundle_exe, b"bundled").unwrap();

    (dir, bare_exe, bundle_exe)
}

// macOS-only: exercises the .app bundle path-resolution helper (Homebrew
// Cellar layout) — no such concept on Linux/Windows. Uses cfg(unix)
// which the strict-cfg guard leaves alone.
#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_finds_bundle_next_to_bin_dir() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-bundle-test");

    let found = sibling_app_bundle_exe(&bare_exe).expect("bundle exe should be found");
    assert_eq!(found, bundle_exe);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_none_when_bundle_absent() {
    let dir = std::env::temp_dir().join(format!(
        "usagio-nobundle-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin_dir = dir.join("Cellar/usagio/0.9.9/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();

    assert!(sibling_app_bundle_exe(&bare_exe).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Mocks a Homebrew-style `<Cellar>/usagio/<version>/{bin,usagio.app}` layout
/// and reproduces the exact plist-building the real `MacOsAutostart::install`
/// does (minus the `launchctl load` shell-out, which needs a real launchd job
/// and a valid Mach-O binary — orthogonal to what's under test here), then
/// asserts the written LaunchAgent plist's `ProgramArguments` points at the
/// bundle's `Contents/MacOS/usagio`, not the bare binary. This is what makes
/// Login Items show the app icon instead of the generic "exec" glyph.
#[cfg(unix)]
#[test]
fn usagio_install_prefers_app_bundle_path_when_available() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-install-bundle-test");

    // Resolve exactly the way `cmd_install` -> `launch_agent_exe_path` would,
    // given this mocked Cellar layout (bypassing `current_exe()`/
    // `stable_exe_path()`, which can't be pointed at a scratch dir).
    let resolved = sibling_app_bundle_exe(&bare_exe).expect("bundle should resolve");
    assert_eq!(resolved, bundle_exe);

    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let plist_path = home
        .join("Library/LaunchAgents")
        .join("com.mattjackson.usagio.test-bundle.plist");

    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let mut prog_args = format!("    <string>{}</string>\n", resolved.display());
        prog_args.push_str("    <string>menubar</string>\n");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key><string>com.mattjackson.usagio.test-bundle</string>
  <key>ProgramArguments</key>
  <array>
{prog_args}  </array>
</dict>
</plist>
"#
        );
        std::fs::create_dir_all(plist_path.parent().unwrap()).unwrap();
        std::fs::write(&plist_path, &plist).unwrap();

        let written = std::fs::read_to_string(&plist_path).unwrap();
        assert!(
            written.contains("usagio.app/Contents/MacOS/usagio"),
            "expected ProgramArguments to point into the app bundle, got:\n{written}"
        );
        assert!(
            !written.contains(&bare_exe.display().to_string()),
            "must not fall back to the bare binary path when a bundle exists"
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// M12 — .app bundle direct-launch (Finder double-click) must default to the
// menu bar, not the one-shot `list` a bare CLI invocation defaults to.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_menubar_when_invoked_from_app_bundle() {
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    assert_eq!(effective_first_arg(&[], &bundle_exe), Some("menubar"));
}

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_list_for_bare_binary() {
    let bare_exe = std::path::PathBuf::from("/opt/homebrew/bin/usagio");
    assert_eq!(effective_first_arg(&[], &bare_exe), None);
}

#[cfg(unix)]
#[test]
fn run_dispatch_prefers_an_explicit_arg_over_bundle_detection() {
    // Even when launched from inside a bundle, an explicit argument (e.g.
    // `usagio list` run via Terminal against the bundled binary) wins.
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    let args = vec!["list".to_string()];
    assert_eq!(effective_first_arg(&args, &bundle_exe), Some("list"));
}

// ---------------------------------------------------------------------------
// Active-account CAS refresh (v0.5.0). `active_refresh_cas` is exercised
// directly against a `MockActiveSlotProvider` rather than through the full
// `refresh_usage_cache()` — the real `ClaudeProvider::read_active_slot` /
// `mirror_rotated_token` on macOS route through the REAL login keychain
// (`Platform::secrets()`), which unit tests must never touch (see the
// `#[ignore]`d `secret_store_roundtrip` in `platform/macos.rs` for the same
// rule). The mock provider gives `active_refresh_cas` an in-memory "OS-native
// slot" it can read/write freely, while the `/token` POST itself still goes
// through the same thread-local URL override + in-process mock HTTP server
// every other refresh test in this file uses (`oauth::refresh` is
// Claude-specific and not swappable per-provider).
// ---------------------------------------------------------------------------

/// Minimal `Provider` stand-in whose "OS-native active slot" is an in-memory
/// `Mutex<Option<String>>` instead of the real keychain / credentials file.
/// Every method besides `read_active_slot` / `mirror_rotated_token` is an
/// inert default — `active_refresh_cas` never calls them.
struct MockActiveSlotProvider {
    slot: std::sync::Mutex<Option<String>>,
}

impl MockActiveSlotProvider {
    fn new(initial: &str) -> Self {
        Self {
            slot: std::sync::Mutex::new(Some(initial.to_string())),
        }
    }

    /// Simulate Claude Code rotating the slot out from under usagio, as if a
    /// live `claude` session refreshed independently.
    #[allow(dead_code)]
    fn rotate_externally(&self, new_blob: &str) {
        *self.slot.lock().unwrap() = Some(new_blob.to_string());
    }
}

impl crate::providers::Provider for MockActiveSlotProvider {
    fn provider_id(&self) -> &'static str {
        "mock-active-slot"
    }
    fn display_name(&self) -> &'static str {
        "Mock"
    }
    fn capabilities(&self) -> crate::providers::trait_def::Capabilities {
        crate::providers::trait_def::Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: false,
            supports_email_capture: false,
            secret_backend: crate::providers::trait_def::SecretBackend::Keychain,
            capture_mode: crate::providers::trait_def::CaptureMode::CredsOnDisk,
        }
    }
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
    fn parse_stored_blob(
        &self,
        _blob: &str,
    ) -> crate::providers::trait_def::PResult<crate::providers::trait_def::TokenGrant> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn patch_stored_blob(
        &self,
        _blob: &str,
        _grant: &crate::providers::trait_def::TokenGrant,
    ) -> crate::providers::trait_def::PResult<String> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn read_active_slot(&self) -> crate::providers::trait_def::PResult<Option<String>> {
        Ok(self.slot.lock().unwrap().clone())
    }
    fn mirror_rotated_token(&self, blob: &str) -> crate::providers::trait_def::PResult<()> {
        *self.slot.lock().unwrap() = Some(blob.to_string());
        Ok(())
    }
}

fn mock_claude_blob(access: &str, refresh: &str, expires_at: i64) -> String {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    })
    .to_string()
}

// The mock-server bootstrap race that used to flake this path on macOS CI
// ("parsing token response: Failed to read JSON: Invalid argument (os error
// 22)") is fixed: `spawn_mock_token_server` now drains the full request before
// responding and half-closes the write side gracefully (see its doc comment),
// which is the read-loop-until-\r\n\r\n fix the old TODO called for.
#[test]
fn active_refresh_never_posts_token_for_active_account() {
    use crate::providers::claude::oauth;
    use crate::store::ScopedConfigDir;

    // `active_refresh_cas` logs via `logging::log`, which resolves
    // `store::config_dir()` — that panics in tests without a
    // `HOME_OVERRIDE` installed (see the tripwire in `store::config_dir`).
    let _g = ScopedConfigDir::new();
    // Point the OAuth refresh at a counting server. The whole point of the
    // v0.5.5 fix is that usagio NEVER mints a token for the active account
    // (a POST would rotate Anthropic's single-use refresh token and burn the
    // copy Claude Code holds → /login). So this server must receive ZERO hits.
    let (base_url, hits) = spawn_mock_token_server();
    oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));

    let blob = mock_claude_blob("active-at", "active-rt", 0);
    let provider = MockActiveSlotProvider::new(&blob);
    let mut acct = Account::from_keychain_blob(&blob).unwrap();
    acct.email = Some("active@example.com".into());

    let outcome = active_refresh_cas(&provider, &mut acct);

    oauth::set_token_url_override(None);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    // The contract: no HTTP refresh was ever attempted.
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "usagio must not POST /token for the active account"
    );
    // Tokens are unchanged (adopted straight from the slot — Claude Code's).
    assert_eq!(acct.access_token, "active-at");
    assert_eq!(acct.refresh_token, "active-rt");
    // The slot itself is untouched — usagio wrote nothing back.
    let slot_blob = provider.slot.lock().unwrap().clone().unwrap();
    let slot_acct = Account::from_keychain_blob(&slot_blob).unwrap();
    assert_eq!(slot_acct.access_token, "active-at");
}

#[test]
fn active_refresh_adopts_cc_rotation_from_slot() {
    let _g = crate::store::ScopedConfigDir::new();
    // Claude Code has rotated its token behind us: the slot holds a DIFFERENT
    // (fresher) blob than our cached state. usagio must adopt it verbatim —
    // never refresh, never write back.
    let cc_blob = mock_claude_blob("cc-rotated-at", "cc-rotated-rt", 999_999_999_999);
    let provider = MockActiveSlotProvider::new(&cc_blob);

    let stale_blob = mock_claude_blob("stale-at", "stale-rt", 0);
    let mut acct = Account::from_keychain_blob(&stale_blob).unwrap();
    acct.email = Some("active@example.com".into());
    acct.needs_relogin = true; // was flagged; adopting a live grant clears it

    let outcome = active_refresh_cas(&provider, &mut acct);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    assert_eq!(acct.access_token, "cc-rotated-at");
    assert_eq!(acct.refresh_token, "cc-rotated-rt");
    assert_eq!(acct.expires_at, 999_999_999_999);
    assert!(
        !acct.needs_relogin,
        "adopting the active slot clears a stale needs_relogin flag"
    );
}

#[test]
fn active_refresh_in_sync_is_a_noop_adopt() {
    let _g = crate::store::ScopedConfigDir::new();
    // Slot and state already agree — the common case. Adopt (no-op) and never
    // POST.
    let blob = mock_claude_blob("same-at", "same-rt", 7);
    let provider = MockActiveSlotProvider::new(&blob);
    let mut acct = Account::from_keychain_blob(&blob).unwrap();
    acct.email = Some("active@example.com".into());

    let outcome = active_refresh_cas(&provider, &mut acct);

    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "outcome={outcome:?}"
    );
    assert_eq!(acct.access_token, "same-at");
    assert_eq!(acct.refresh_token, "same-rt");
    assert_eq!(acct.expires_at, 7);
}

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

/// Minimal single-threaded mock token endpoint: accepts TCP connections,
/// reads the full request, and replies with a fixed valid refresh-grant JSON
/// body. Returns (base_url, hits) where `hits` is bumped once per accepted
/// connection.
///
/// The request is read to completion (request line + headers up to the blank
/// line + any `Content-Length` body) BEFORE the response is written, and the
/// write half is shut down explicitly after flushing. A prior version issued a
/// single `stream.read(&mut buf)` and closed the socket as soon as it had
/// written the response. Under parallel load that raced ureq's request write
/// two ways: the single read could return before ureq finished sending the
/// body (a request split across TCP segments), and closing the socket with
/// unread inbound data still buffered makes the kernel send an RST, which ureq
/// surfaces mid-response as `Transient("parsing token response: Failed to read
/// JSON: Invalid argument (os error 22)")`. That dropped the refresh on the
/// floor (kept-cache path) and flaked
/// `refresh_usage_cache_still_refreshes_inactive_accounts` ~25-50% of full
/// parallel runs. Draining the request first, then a graceful `shutdown(Write)`,
/// closes both windows (the fix the old `active_refresh_*` TODO called for).
fn spawn_mock_token_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock token server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            // Read the whole request before responding. `TcpStream` is `Read`
            // + `Write` through a shared `&TcpStream`, so a `BufReader` borrow
            // for the request and direct writes for the response can coexist.
            let mut reader = BufReader::new(&stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            // Consume headers up to the blank line, tracking Content-Length so
            // the body is fully drained (avoids a close-with-unread-data RST).
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header == "\r\n" || header == "\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            if content_length > 0 {
                let mut body_buf = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body_buf);
            }
            let is_token_post = request_line.starts_with("POST");
            let body = if is_token_post {
                hits_thread.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "access_token": "mock-refreshed-access-token",
                    "refresh_token": "mock-refreshed-refresh-token",
                    "expires_in": 3600,
                })
                .to_string()
            } else {
                // Usage GET — any well-formed empty snapshot avoids a parse
                // error; this test doesn't assert on usage contents.
                serde_json::json!({
                    "five_hour": null,
                    "seven_day": null,
                    "seven_day_opus": null,
                })
                .to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            drop(reader);
            let mut stream = stream;
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            // Graceful half-close: signal EOF to ureq once the full response is
            // out, rather than dropping the socket abruptly.
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

// ---------------------------------------------------------------------------
// codex-switch-e2e: generic (non-Claude) capture / switch / CAS dispatch.
// Every test below uses `ScopedConfigDir` (state.json → tempdir) and
// `env_lock::scoped_env_var("CODEX_HOME", ...)` (auth.json → tempdir), so
// none of them ever touch a real `~/.codex/auth.json` or the real OS
// keychain — consistent with the crate-wide test-hermeticity contract.
// ---------------------------------------------------------------------------

fn codex_id_token(email: &str, exp: i64) -> String {
    use base64::Engine;
    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({ "email": email, "exp": exp })
            .to_string()
            .as_bytes(),
    );
    format!("{hdr}.{payload}.sig")
}

/// A full Codex `auth.json` blob for `email`, expiring at `exp` (unix
/// seconds). Mirrors `providers::codex::mod::tests::make_blob`, duplicated
/// here rather than imported since it's `#[cfg(test)]`-private to that
/// module and this file's edit scope doesn't include changing that.
fn codex_auth_json(email: &str, exp: i64, access: &str, refresh: &str) -> String {
    let id_token = codex_id_token(email, exp);
    serde_json::json!({
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
        }
    })
    .to_string()
}

#[test]
fn capture_current_generic_persists_a_codex_account_into_state_v2() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("codex-user@example.com", 4_000_000_000, "at1", "rt1");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();

        let (key, existed) = capture_current_generic("codex").expect("capture must succeed");
        assert_eq!(key, "codex-user@example.com");
        assert!(
            !existed,
            "first capture of this account must report existed=false"
        );

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "codex-user@example.com")
            .expect("captured account must be persisted");
        assert_eq!(acct.access_token, "at1");
        assert_eq!(acct.refresh_token, "rt1");
        assert_eq!(acct.secret_blob, blob);
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("codex-user@example.com"),
            "capture must also become the active account for its provider"
        );

        // Re-capturing the same account (same auth.json) reports existed=true
        // and doesn't lose the identity.
        let (key2, existed2) = capture_current_generic("codex").unwrap();
        assert_eq!(key2, "codex-user@example.com");
        assert!(existed2);
    });
}

#[test]
fn switch_to_provider_account_writes_auth_json_and_updates_active() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Capture two accounts (A then B) so B ends up active, then switch
        // back to A and confirm auth.json + state.providers["codex"].active
        // both flip.
        let blob_a = codex_auth_json("a@example.com", 4_000_000_000, "at-a", "rt-a");
        std::fs::write(dir.path().join("auth.json"), &blob_a).unwrap();
        capture_current_generic("codex").unwrap();

        let blob_b = codex_auth_json("b@example.com", 4_000_000_000, "at-b", "rt-b");
        std::fs::write(dir.path().join("auth.json"), &blob_b).unwrap();
        capture_current_generic("codex").unwrap();

        // A over the trigger, so the manual pick below holds.
        let mut st = State::load().unwrap();
        st.find_provider_account_mut("codex", "a@example.com")
            .unwrap()
            .cached_usage = Some(usage_at(97.0));
        st.save().unwrap();

        let label = switch_to_provider_account("codex", "a@example.com", true).unwrap();
        assert_eq!(label, "a@example.com");

        let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
        assert_eq!(on_disk, blob_a, "auth.json must now hold account A's blob");

        let state = State::load().unwrap();
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("a@example.com")
        );
        // Both accounts are still there — switching must not drop B.
        assert!(state
            .find_provider_account("codex", "b@example.com")
            .is_some());
        // A manual switch past the trigger holds; an auto-swap clears the hold.
        assert_eq!(state.manual_hold("codex"), Some("a@example.com"));
        switch_to_provider_account("codex", "b@example.com", false).unwrap();
        assert_eq!(State::load().unwrap().manual_hold("codex"), None);
        // A manual switch under the trigger (B has no usage yet) doesn't hold.
        switch_to_provider_account("codex", "a@example.com", true).unwrap();
        switch_to_provider_account("codex", "b@example.com", true).unwrap();
        assert_eq!(State::load().unwrap().manual_hold("codex"), None);
    });
}

#[test]
fn switch_to_provider_account_errors_for_unknown_key() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let err = switch_to_provider_account("codex", "nobody@example.com", false).unwrap_err();
        assert!(format!("{err}").contains("no codex account matches"));
    });
}

#[test]
fn resolve_provider_selector_matches_exact_and_unique_prefix() {
    let mut state = State::default();
    state.upsert_provider_account(
        "codex",
        crate::store::ProviderAccount {
            key: "dev@example.com".into(),
            secret_blob: "{}".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            identity_email: Some("dev@example.com".into()),
            identity_uuid: None,
            identity_display_name: None,
            identity_native_blob: serde_json::Value::Null,
            cached_usage: None,
            notif_state: Default::default(),
            needs_relogin: false,
            no_subscription: false,
            plan: None,
        },
    );

    assert_eq!(
        resolve_provider_selector(&state, "dev@example.com"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(
        resolve_provider_selector(&state, "dev"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(resolve_provider_selector(&state, "nobody"), None);
}

#[test]
fn refresh_provider_active_account_noop_when_provider_does_not_support_active_refresh() {
    // `opencode` is registered but doesn't override `supports_active_refresh`
    // (defaults `false`) — the gate this test pins must stop
    // `refresh_provider_active_account` before it ever looks at
    // `state.providers["opencode"]`, let alone tries a network call.
    let _cfg = crate::store::ScopedConfigDir::new();
    let seed = crate::store::ProviderAccount {
        key: "x@example.com".into(),
        secret_blob: "untouched".into(),
        access_token: "untouched-at".into(),
        refresh_token: "untouched-rt".into(),
        expires_at: 1,
        identity_email: Some("x@example.com".into()),
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: None,
        notif_state: Default::default(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
    };
    with_state_lock(|| {
        let mut st = State::load()?;
        st.upsert_provider_account("opencode", seed);
        st.provider_accounts_mut("opencode").active = Some("x@example.com".to_string());
        st.save()
    })
    .unwrap();

    refresh_provider_active_account("opencode");

    let after = State::load().unwrap();
    let acct = after
        .find_provider_account("opencode", "x@example.com")
        .unwrap();
    assert_eq!(acct.secret_blob, "untouched");
    assert_eq!(acct.access_token, "untouched-at");
}

#[test]
fn refresh_provider_active_account_codex_fresh_token_is_a_noop() {
    // The stored blob still matches the slot on disk (the Codex CLI hasn't
    // rotated it), so the active-account follower reports `Fresh` — no network
    // call is made, and the account must be byte-for-byte unchanged.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("fresh@example.com", 4_000_000_000, "fresh-at", "fresh-rt");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();
        capture_current_generic("codex").unwrap();

        refresh_provider_active_account("codex");

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "fresh@example.com")
            .unwrap();
        assert_eq!(acct.access_token, "fresh-at");
        assert_eq!(acct.secret_blob, blob);
    });
}

#[test]
fn refresh_provider_active_account_codex_adopts_cli_rotation_without_posting() {
    // End-to-end for the NEVER-POST active-account contract: usagio captured
    // one blob, then the Codex CLI rotated `auth.json` on its own cadence.
    // `refresh_provider_active_account` must ADOPT the CLI's new blob into
    // `state.providers["codex"]` WITHOUT ever POSTing `/token` — a usagio POST
    // for the active account would consume the single-use refresh token and
    // force a `codex login`. We prove zero POSTs by pointing the token-URL
    // override at a mock server and asserting its hit counter stays 0.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().to_str().unwrap().to_string();

    // `scoped_env_var` isn't reentrant, so both env vars are set inside one
    // call (mirrors `providers::codex::oauth_tests::with_codex_env`, which
    // this file's edit scope doesn't extend to import from).
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(&codex_home), || {
        // usagio's last-observed blob (captured into state).
        let captured_blob = codex_auth_json("user@example.com", 1, "old-at", "old-rt");
        std::fs::write(dir.path().join("auth.json"), &captured_blob).unwrap();
        capture_current_generic("codex").unwrap();

        // The Codex CLI now rotates auth.json to a fresh grant behind our back.
        let cli_rotated = codex_auth_json(
            "user@example.com",
            4_000_000_000,
            "cli-rotated-at",
            "cli-rotated-rt",
        );
        std::fs::write(dir.path().join("auth.json"), &cli_rotated).unwrap();

        let (base_url, hits) = spawn_mock_token_server();
        #[allow(clippy::disallowed_methods)]
        let prev_url = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)]
        std::env::set_var(
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            format!("{base_url}/v1/oauth/token"),
        );

        refresh_provider_active_account("codex");

        #[allow(clippy::disallowed_methods)]
        match prev_url {
            Some(v) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", v),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }

        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "active-account refresh must NEVER POST /token"
        );

        // State adopted the CLI's rotated grant verbatim.
        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "user@example.com")
            .expect("account survives the adopt");
        assert_eq!(acct.access_token, "cli-rotated-at");
        assert_eq!(acct.secret_blob, cli_rotated);

        // usagio never wrote auth.json — the CLI's blob stands untouched.
        let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
        assert_eq!(on_disk, cli_rotated);
    });
}

// usagio must NEVER mint a token for the active account — Anthropic's refresh
// tokens are single-use, so a usagio POST would rotate the family server-side
// and invalidate the copy Claude Code holds (→ /login). The active-account
// path (`active_refresh_cas`) is therefore a pure adopt-from-slot, exercised in
// isolation above against `MockActiveSlotProvider` so no test here ever touches
// the real OS keychain that `refresh_usage_cache` would resolve
// `ClaudeProvider::read_active_slot()` to.

// --- apply_account keychain-write-failure rollback (v0.5.17 codeaudit) --------
//
// The core "switching ALWAYS works, never a half-applied switch" guard:
// apply_account writes ~/.claude.json FIRST, then the keychain LAST as the
// commit point, and rolls ~/.claude.json back (returning Err) if the keychain
// write fails. Before v0.5.17 the cfg(test) keychain mock always succeeded and
// no failure-injection seam existed, so this rollback branch had ZERO coverage
// — a regression dropping the rollback (leaving ~/.claude.json on the new
// account while the keychain still holds the old) would have passed the suite.
// Runs on every platform: the failure seam now lives in platform/mod.rs and is
// honored by all three SecretStore::set impls (macOS mock, Linux keyring, Windows
// Credential Manager), so the rollback guarantee is enforced in CI everywhere.
#[test]
fn apply_account_rolls_back_claude_json_when_keychain_write_fails() {
    use crate::store::{Account, ScopedConfigDir};

    // ScopedConfigDir isolates config_dir (logging::log resolves it); the HOME
    // override points ~/.claude.json (claude_json_path uses $HOME) at the same
    // tempdir. env_lock serializes the $HOME mutation against other tests.
    let scd = ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let claude_json = home.join(".claude.json");
        let prior = r#"{"oauthAccount":{"emailAddress":"old@example.com"},"other":"keep"}"#;
        std::fs::write(&claude_json, prior).unwrap();

        let acct = Account::from_keychain_blob(
            r#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"r","expiresAt":0}}"#,
        )
        .unwrap();
        let identity = serde_json::json!({
            "oauthAccount": { "emailAddress": "new@example.com" }
        });

        // Arm the mock keychain to fail the (single) set apply_account performs.
        crate::platform::arm_keychain_set_failure();

        let provider = crate::providers::get("claude").expect("claude provider registered");
        let res = apply_account(
            provider,
            &acct,
            &identity,
            Some("old@example.com"),
            "new@example.com",
        );

        assert!(
            res.is_err(),
            "a keychain write failure must fail the switch, never report success"
        );
        let after = std::fs::read_to_string(&claude_json).unwrap();
        assert_eq!(
            after, prior,
            "~/.claude.json must be rolled back to its prior contents on keychain failure \
             (no half-applied switch)"
        );
    });
}

#[test]
fn reset_recovery_requires_refresh_then_selects_recovered_account() {
    let now = Utc::now();
    let reset = now - Duration::seconds(5);
    let mut rows = vec![
        row_full("active@e.com", 0.0, 100.0, now + Duration::days(5)),
        row_full("recovering@e.com", 0.0, 100.0, reset),
        row_full("later@e.com", 0.0, 100.0, now + Duration::days(2)),
    ];
    rows[1].fetched_at = Some((reset - Duration::seconds(10)).timestamp());
    let guard = SwapGuard::default();
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
    // Even a low pre-reset sample is not confirmation of availability.
    rows[1].weekly.pct = Some(0.0);
    assert!(choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).is_none());
    rows[1].fetched_at = Some(now.timestamp());
    rows[1].weekly.resets_at = Some(now + Duration::days(7));
    assert_eq!(
        choose_swap_target(&rows, "active@e.com", 95.0, 85.0, &guard).as_deref(),
        Some("recovering@e.com")
    );
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
fn all_blocked_prepares_earliest_fully_recovered_account_without_churn() {
    let now = Utc::now();
    let mut rows = vec![
        row_full("active@e.com", 0.0, 100.0, now + Duration::days(5)),
        row_full("both@e.com", 100.0, 100.0, now + Duration::days(3)),
        row_full("next@e.com", 0.0, 100.0, now + Duration::hours(2)),
    ];
    rows[1].session.resets_at = Some(now + Duration::hours(1));
    let guard = SwapGuard::default();
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert!(eval.preparing);
    assert_eq!(eval.target.as_deref(), Some("next@e.com"));
    assert!(!rows[2].available(), "preselection must not imply capacity");
    assert!(evaluate_swap(&rows, "next@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
    rows[0].weekly.resets_at = rows[2].weekly.resets_at;
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
}

#[test]
fn blocked_preparation_preserves_manual_choice_but_allows_recovered_target() {
    let now = Utc::now();
    let mut rows = vec![
        row_full("manual@e.com", 0.0, 100.0, now + Duration::days(5)),
        row_full("next@e.com", 0.0, 100.0, now + Duration::hours(2)),
    ];
    let guard = SwapGuard {
        manual_locked_choice: Some((CLAUDE_SLUG.to_string(), "manual@e.com".to_string())),
        ..SwapGuard::default()
    };
    assert!(evaluate_swap(&rows, "manual@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
    rows[1].weekly.pct = Some(0.0);
    let eval = evaluate_swap(&rows, "manual@e.com", 95.0, 85.0, &guard);
    assert!(!eval.preparing);
    assert_eq!(eval.target.as_deref(), Some("next@e.com"));
}

#[test]
fn manual_hold_stays_past_trigger_until_exhausted() {
    let now = Utc::now();
    let mut rows = vec![
        row_full("manual@e.com", 0.0, 85.0, now + Duration::days(1)),
        row_full("roomy@e.com", 0.0, 53.0, now + Duration::days(2)),
    ];
    let held = SwapGuard {
        manual_locked_choice: Some((CLAUDE_SLUG.to_string(), "manual@e.com".to_string())),
        ..SwapGuard::default()
    };
    // Unheld, 85% against an 85% trigger is an urgent swap.
    assert_eq!(
        evaluate_swap(&rows, "manual@e.com", 85.0, 85.0, &SwapGuard::default())
            .target
            .as_deref(),
        Some("roomy@e.com")
    );
    assert!(evaluate_swap(&rows, "manual@e.com", 85.0, 85.0, &held)
        .target
        .is_none());
    rows[0].weekly.pct = Some(99.0);
    assert!(evaluate_swap(&rows, "manual@e.com", 85.0, 85.0, &held)
        .target
        .is_none());
    // Exhausted: the hold no longer protects it.
    rows[0].weekly.pct = Some(100.0);
    assert_eq!(
        evaluate_swap(&rows, "manual@e.com", 85.0, 85.0, &held)
            .target
            .as_deref(),
        Some("roomy@e.com")
    );
    // Lapsed subscription: same.
    rows[0].weekly.pct = Some(90.0);
    rows[0].no_subscription = true;
    assert_eq!(
        evaluate_swap(&rows, "manual@e.com", 85.0, 85.0, &held)
            .target
            .as_deref(),
        Some("roomy@e.com")
    );
}

#[test]
fn no_flip_back_into_the_429_escalation_band() {
    // v0.8.7 thrash: dev@ at 84% (85% trigger) resets soonest, so every
    // flip-back walked back to it — and the next 429 forced it out again.
    let now = Utc::now();
    let rows = vec![
        row_full("dev4@e.com", 0.0, 53.0, now + Duration::days(2)),
        row_full("dev@e.com", 0.0, 84.0, now + Duration::days(1)),
    ];
    let eval = evaluate_swap(&rows, "dev4@e.com", 85.0, 85.0, &SwapGuard::default());
    assert!(eval.target.is_none() && eval.target_ignoring_cooldown.is_none());
    // Just under the band it is a flip-back target again.
    let mut rows = rows;
    rows[1].weekly.pct = Some(79.0);
    assert_eq!(
        evaluate_swap(&rows, "dev4@e.com", 85.0, 85.0, &SwapGuard::default())
            .target
            .as_deref(),
        Some("dev@e.com")
    );
}

#[test]
fn reactive_swap_prefers_targets_below_the_band() {
    let now = Utc::now();
    let rows = vec![
        row_full("active@e.com", 0.0, 96.0, now + Duration::days(4)),
        row_full("banded@e.com", 0.0, 90.0, now + Duration::days(1)),
        row_full("roomy@e.com", 0.0, 40.0, now + Duration::days(3)),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    assert_eq!(eval.target.as_deref(), Some("roomy@e.com"));
    assert!(!eval.fallback);
    // Only banded accounts left: the fallback still moves to the most room.
    let mut rows = rows;
    rows.pop();
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    assert_eq!(eval.target.as_deref(), Some("banded@e.com"));
    assert!(eval.fallback);
}

fn usage_at(pct: f64) -> CachedUsage {
    CachedUsage {
        session_pct: Some(pct),
        weekly_pct: Some(10.0),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: Utc::now().timestamp(),
        reported: Default::default(),
    }
}

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
fn manual_hold_blocks_proactive_flip_back() {
    let now = Utc::now();
    let rows = vec![
        row_full("manual@e.com", 0.0, 60.0, now + Duration::days(5)),
        row_full("sooner@e.com", 0.0, 10.0, now + Duration::hours(3)),
    ];
    let unheld = evaluate_swap(&rows, "manual@e.com", 95.0, 95.0, &SwapGuard::default());
    assert_eq!(
        unheld.target_ignoring_cooldown.as_deref(),
        Some("sooner@e.com"),
        "precondition: an unheld account would flip to the sooner reset"
    );
    let held = SwapGuard {
        manual_locked_choice: Some((CLAUDE_SLUG.to_string(), "manual@e.com".to_string())),
        ..SwapGuard::default()
    };
    let eval = evaluate_swap(&rows, "manual@e.com", 95.0, 95.0, &held);
    assert!(eval.target.is_none() && eval.target_ignoring_cooldown.is_none());
}

#[test]
fn switch_reports_ambiguous_prefix_instead_of_no_match() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    for email in ["dev@e.com", "dev2@e.com"] {
        let mut a = acct_with_cache(None);
        a.email = Some(email.to_string());
        state.upsert(a);
    }
    state.save().unwrap();
    let err = cmd_switch(Some("dev"), None).unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(
        err.contains("dev@e.com") && err.contains("dev2@e.com"),
        "{err}"
    );
}

#[test]
fn blocked_preparation_requires_known_future_resets_and_valid_login() {
    let now = Utc::now();
    let mut rows = vec![
        row_full("active@e.com", 0.0, 100.0, now + Duration::days(5)),
        row_full("next@e.com", 0.0, 100.0, now + Duration::hours(2)),
    ];
    let guard = SwapGuard::default();
    rows[1].weekly.resets_at = None;
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
    rows[1].weekly.resets_at = Some(now - Duration::seconds(1));
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
    rows[1].weekly.resets_at = Some(now + Duration::hours(2));
    rows[1].needs_relogin = true;
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
    rows[1].needs_relogin = false;
    rows[1].error = Some("usage unavailable".to_string());
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
}

#[test]
fn blocked_preparation_respects_disabled_autoswap_cooldown_and_no_return() {
    let now = Utc::now();
    let rows = vec![
        row_full("active@e.com", 0.0, 100.0, now + Duration::days(5)),
        row_full("next@e.com", 0.0, 100.0, now + Duration::hours(2)),
    ];
    let mut guard = SwapGuard::default();
    assert!(evaluate_swap(&rows, "active@e.com", 101.0, 85.0, &guard)
        .target
        .is_none());
    guard.last_swap = Some(std::time::Instant::now());
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard);
    assert!(eval.blocked_by_cooldown);
    assert!(eval.target.is_none());
    assert_eq!(eval.target_ignoring_cooldown.as_deref(), Some("next@e.com"));
    guard.last_swap = None;
    guard
        .left_at
        .insert("next@e.com".to_string(), std::time::Instant::now());
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 85.0, &guard)
        .target
        .is_none());
}

// --- v0.8.5: active account gets the request budget; inactive backs off alone ---

#[test]
fn inactive_fetch_backoff_doubles_from_floor_and_caps() {
    assert_eq!(inactive_fetch_backoff_secs(1), INACTIVE_FETCH_FLOOR_SECS);
    assert_eq!(
        inactive_fetch_backoff_secs(2),
        INACTIVE_FETCH_FLOOR_SECS * 2
    );
    assert_eq!(
        inactive_fetch_backoff_secs(3),
        INACTIVE_FETCH_FLOOR_SECS * 4
    );
    assert_eq!(
        inactive_fetch_backoff_secs(4),
        INACTIVE_FETCH_MAX_BACKOFF_SECS
    );
    assert_eq!(
        inactive_fetch_backoff_secs(u32::MAX),
        INACTIVE_FETCH_MAX_BACKOFF_SECS
    );
}

#[test]
fn inactive_fetch_failure_sets_backoff_and_success_clears_it() {
    let email = escalation_test_email("inactive-backoff");
    let now_ts = 1_000_000;
    record_inactive_fetch_failure(&email, now_ts);
    record_inactive_fetch_failure(&email, now_ts);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 2);
    assert_eq!(
        t.backoff_until_ts,
        Some(now_ts + inactive_fetch_backoff_secs(2) as i64)
    );
    record_usage_fetch_success(&email);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 0);
    assert_eq!(t.backoff_until_ts, None);
    reset_usage_fetch_tracker(&email);
}

/// Mock usage endpoint that answers every request with HTTP 429 and counts hits.
fn spawn_mock_429_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock 429 server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                    break;
                }
            }
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

/// The v0.8.5 incident: a 429 on an INACTIVE account set the global
/// `rate_limited` flag, pushing the loop to 1200s so the active account ran
/// out between polls. Now it must not slow the loop, and must back that
/// account off on its own so the next cycle doesn't fetch it again.
#[test]
fn inactive_429_does_not_rate_limit_loop_and_backs_off_that_account() {
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use crate::providers::claude::usage;
    use crate::store::{Account, ScopedConfigDir};

    let _g = ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_429_server();
    usage::set_usage_url_override(Some(&format!("{base_url}/api/oauth/usage")));

    let email = escalation_test_email("inactive-429");
    let fresh = Utc::now().timestamp_millis() + 3_600_000;
    let mut inactive = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"inactive-at","refreshToken":"inactive-rt","expiresAt":{fresh}}}}}"#
    ))
    .unwrap();
    inactive.email = Some(email.clone());
    let mut seed = State::default();
    seed.accounts.push(inactive);
    seed.active = None;
    seed.save().unwrap();

    let first = refresh_usage_cache(false);
    let second = refresh_usage_cache(false);
    usage::set_usage_url_override(None);

    assert!(
        !first.rate_limited,
        "an inactive 429 must not slow the global loop"
    );
    assert!(!second.rate_limited);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the backed-off inactive account must not be re-fetched next cycle"
    );
    assert!(get_usage_fetch_tracker(&email).backoff_until_ts.is_some());
    reset_usage_fetch_tracker(&email);
}

// --- one core, multiple providers (v0.8.8) ---

#[test]
fn all_accounts_past_trigger_falls_back_to_most_room() {
    // Trigger 80: the active account is locked at 100%; nothing else is under
    // the trigger, but one account still has 15 points left. Staying on the
    // full account is the worst option — move to the one with the most room.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 100.0, 60.0, reset),
        row_full("some@e.com", 85.0, 40.0, reset),
        row_full("less@e.com", 97.0, 40.0, reset),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 80.0, 80.0, &SwapGuard::default());
    assert!(eval.fallback && eval.urgent);
    assert_eq!(eval.target.as_deref(), Some("some@e.com"));
}

#[test]
fn fallback_needs_a_real_gain() {
    // 96% → 97%-full alternatives isn't worth a swap.
    let reset = Utc::now() + Duration::hours(24);
    let rows = vec![
        row_full("active@e.com", 96.0, 60.0, reset),
        row_full("other@e.com", 94.0, 40.0, reset),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 90.0, 90.0, &SwapGuard::default());
    assert!(!eval.fallback);
    assert_ne!(eval.target.as_deref(), Some("other@e.com"));
}

fn cycle(slug: &str, active: &str, max_pct: f64, rate_limited: bool) -> ProviderCycle {
    ProviderCycle {
        slug: slug.to_string(),
        active: Some(active.to_string()),
        swapped: None,
        rate_limited,
        max_pct: Some(max_pct),
        actionable: true,
    }
}

#[test]
fn cadence_keeps_one_providers_429_from_slowing_another() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut c = Cadence::new(180);
    let out = CycleOutcome {
        providers: vec![
            cycle(CLAUDE_SLUG, "a@e.com", 93.0, false),
            cycle("codex", "x@e.com", 40.0, true),
        ],
    };
    // Claude near the trigger polls at 30s even though Codex is backing off.
    assert_eq!(c.advance(&out, 95.0), 30);
    assert_eq!(c.current_for("codex"), 360);
    assert_eq!(c.current_for(CLAUDE_SLUG), 30);
}

#[test]
fn cadence_restarts_backoff_tight_after_a_switch() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut c = Cadence::new(180);
    // Backed off on account a.
    let out = CycleOutcome {
        providers: vec![cycle(CLAUDE_SLUG, "a@e.com", 93.0, true)],
    };
    c.advance(&out, 95.0);
    c.advance(&out, 95.0);
    assert!(c.current_for(CLAUDE_SLUG) > 60);
    // Auto-swap to b this cycle: the 429 was a's; b starts from 30s.
    let mut swapped = cycle(CLAUDE_SLUG, "b@e.com", 20.0, true);
    swapped.swapped = Some(("a@e.com".into(), "b@e.com".into()));
    c.advance(
        &CycleOutcome {
            providers: vec![swapped],
        },
        95.0,
    );
    assert_eq!(c.current_for(CLAUDE_SLUG), WATCH_TIGHT_INTERVAL_SECS);
    // A first 429 on b only doubles that: 60s, not 360s.
    c.advance(
        &CycleOutcome {
            providers: vec![cycle(CLAUDE_SLUG, "b@e.com", 93.0, true)],
        },
        95.0,
    );
    assert_eq!(c.current_for(CLAUDE_SLUG), 60);
}

#[test]
fn fetch_tracker_keys_are_namespaced_per_provider() {
    assert_eq!(fetch_tracker_key(CLAUDE_SLUG, "a@e.com"), "a@e.com");
    assert_eq!(fetch_tracker_key("codex", "a@e.com"), "codex/a@e.com");
}

#[test]
fn escalation_reads_the_providers_own_tracker() {
    // A Codex 429 must escalate the Codex row, and never a Claude row with the
    // same address.
    let email = escalation_test_email("codex");
    record_usage_fetch_429(&fetch_tracker_key("codex", &email));
    let reset = Utc::now() + Duration::hours(24);
    let codex = row_full_with_provider("codex", &email, 93.0, 50.0, reset);
    let claude = row_full(&email, 93.0, 50.0, reset);
    assert!(effective_active_max_pct_for_swap_fire(&codex, 95.0) >= 95.0);
    assert_eq!(effective_active_max_pct_for_swap_fire(&claude, 95.0), 93.0);
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &email));
}

// --- mutation-hardening: swap decision core ---

/// An `Instant` `secs` in the past, or `None` on a host whose monotonic clock
/// is younger than that (callers skip the check rather than fail).
fn ago(secs: u64) -> Option<std::time::Instant> {
    std::time::Instant::now().checked_sub(std::time::Duration::from_secs(secs))
}

/// `Row` is not `Clone`; rebuild one field by field.
fn dup(r: &Row) -> Row {
    Row {
        provider_id: r.provider_id.clone(),
        needs_relogin: r.needs_relogin,
        no_subscription: r.no_subscription,
        plan: r.plan.clone(),
        email: r.email.clone(),
        session: Cell {
            pct: r.session.pct,
            resets_at: r.session.resets_at,
        },
        weekly: Cell {
            pct: r.weekly.pct,
            resets_at: r.weekly.resets_at,
        },
        opus: None,
        error: r.error.clone(),
        fetched_at: r.fetched_at,
        reported: r.reported.clone(),
    }
}

fn manual_guard(email: &str) -> SwapGuard {
    SwapGuard {
        manual_locked_choice: Some((CLAUDE_SLUG.to_string(), email.to_string())),
        ..SwapGuard::default()
    }
}

#[test]
fn eligible_target_requires_session_and_weekly_strictly_below_band() {
    let reset = Utc::now() + Duration::hours(5);
    // Trigger 95 -> limit 90: a window sitting exactly on the limit is inside
    // the 429-escalation band and must be refused.
    assert!(row_full("a@e.com", 89.9, 89.9, reset).eligible_target(95.0, 95.0));
    assert!(!row_full("a@e.com", 90.0, 10.0, reset).eligible_target(95.0, 95.0));
    assert!(!row_full("a@e.com", 10.0, 90.0, reset).eligible_target(95.0, 95.0));
}

#[test]
fn effective_max_pct_escalates_from_exactly_the_band_edge() {
    let email = escalation_test_email("band-edge");
    record_usage_fetch_429(&email);
    let reset = Utc::now() + Duration::hours(5);
    // 90 = trigger(95) - band(5): the lowest reading a 429 still escalates.
    let on_edge = row_full(&email, 90.0, 10.0, reset);
    assert_eq!(effective_active_max_pct_for_swap_fire(&on_edge, 95.0), 95.0);
    let below = row_full(&email, 89.0, 10.0, reset);
    assert_eq!(effective_active_max_pct_for_swap_fire(&below, 95.0), 89.0);
    reset_usage_fetch_tracker(&email);
}

#[test]
fn held_on_needs_both_provider_and_account_to_match() {
    let reset = Utc::now() + Duration::hours(5);
    let guard = manual_guard("Manual@e.com");
    assert!(held_on(
        &guard,
        &row_full("manual@e.com", 10.0, 10.0, reset)
    ));
    // Same address under another provider is a different login.
    assert!(!held_on(
        &guard,
        &row_full_with_provider("codex", "manual@e.com", 10.0, 10.0, reset)
    ));
    // Same provider, different account.
    assert!(!held_on(
        &guard,
        &row_full("other@e.com", 10.0, 10.0, reset)
    ));
    assert!(!held_on(
        &SwapGuard::default(),
        &row_full("manual@e.com", 10.0, 10.0, reset)
    ));
}

#[test]
fn hold_lets_go_exactly_at_99_5() {
    let reset = Utc::now() + Duration::hours(5);
    let guard = manual_guard("manual@e.com");
    assert!(hold_applies(
        &guard,
        &row_full("manual@e.com", 0.0, 99.4, reset)
    ));
    assert!(!hold_applies(
        &guard,
        &row_full("manual@e.com", 0.0, 99.5, reset)
    ));
}

#[test]
fn fresh_since_left_compares_the_reading_to_the_moment_of_leaving() {
    let Some(left) = ago(100) else { return };
    let mut guard = SwapGuard::default();
    guard.left_at.insert("gone@e.com".to_string(), left);
    let reset = Utc::now() + Duration::hours(5);
    let mut r = row_full("gone@e.com", 10.0, 10.0, reset);
    // Retry until the wall-clock second is stable across the call.
    for _ in 0..5 {
        let t0 = Utc::now().timestamp();
        r.fetched_at = Some(t0 - 50); // taken 50s ago: after leaving 100s ago
        let after = fresh_since_left(&r, &guard);
        r.fetched_at = Some(t0 - 150); // taken before leaving
        let before = fresh_since_left(&r, &guard);
        r.fetched_at = Some(t0 - 100); // exactly at leaving: not after it
        let at = fresh_since_left(&r, &guard);
        if Utc::now().timestamp() != t0 {
            continue;
        }
        assert!(after);
        assert!(!before);
        assert!(!at);
        return;
    }
    panic!("wall clock never stable");
}

#[test]
fn prune_swap_guard_drops_only_entries_older_than_retention() {
    let (Some(old), Some(exact)) = (
        ago(LEFT_AT_RETENTION_SECS + 3600),
        ago(LEFT_AT_RETENTION_SECS),
    ) else {
        return;
    };
    let mut guard = SwapGuard::default();
    guard.left_at.insert("old@e.com".to_string(), old);
    guard.left_at.insert("exact@e.com".to_string(), exact);
    guard
        .left_at
        .insert("new@e.com".to_string(), std::time::Instant::now());
    prune_swap_guard(&mut guard);
    let mut kept: Vec<&str> = guard.left_at.keys().map(String::as_str).collect();
    kept.sort();
    // An entry that has reached the retention age is dropped; a fresh one is kept.
    assert_eq!(kept, ["new@e.com"]);
}

#[test]
fn lapsed_active_without_data_is_still_left_but_dataless_active_is_not() {
    let reset = Utc::now() + Duration::hours(5);
    let mut active = row_full("active@e.com", 0.0, 0.0, reset);
    active.fetched_at = None;
    let rows = vec![active, row_full("free@e.com", 10.0, 10.0, reset)];
    let guard = SwapGuard::default();
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
        .target
        .is_none());
    let mut rows = rows;
    rows[0].no_subscription = true;
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
            .target
            .as_deref(),
        Some("free@e.com")
    );
}

#[test]
fn trigger_of_exactly_100_still_swaps_off_a_full_account() {
    let reset = Utc::now() + Duration::hours(5);
    let rows = vec![
        row_full("active@e.com", 0.0, 100.0, reset),
        row_full("free@e.com", 10.0, 10.0, reset),
    ];
    let guard = SwapGuard::default();
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 100.0, 100.0, &guard)
            .target
            .as_deref(),
        Some("free@e.com")
    );
    // Above 100 is how auto-swap is disabled.
    assert!(evaluate_swap(&rows, "active@e.com", 100.5, 100.0, &guard)
        .target
        .is_none());
}

#[test]
fn swap_targets_come_only_from_the_active_accounts_provider() {
    let reset = Utc::now() + Duration::hours(5);
    // Codex is a registered, swap-capable provider; its roomy account must
    // never be offered to a Claude active account.
    assert!(provider_supports_swap("codex"));
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset),
        row_full_with_provider("codex", "codex@e.com", 0.0, 0.0, reset),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    assert!(eval.target.is_none());
    // The same row is the right target when the active account is Codex's.
    let rows = vec![
        row_full_with_provider("codex", "active@e.com", 96.0, 96.0, reset),
        row_full_with_provider("codex", "codex@e.com", 0.0, 0.0, reset),
    ];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    assert_eq!(eval.target.as_deref(), Some("codex@e.com"));
}

#[test]
fn account_without_a_reading_is_never_a_swap_target() {
    let reset = Utc::now() + Duration::hours(5);
    let mut blank = row_full("blank@e.com", 0.0, 0.0, reset);
    blank.fetched_at = None;
    let mut errored = row_full("errored@e.com", 0.0, 0.0, reset);
    errored.error = Some("boom".to_string());
    let rows = vec![row_full("active@e.com", 96.0, 96.0, reset), blank, errored];
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    assert!(eval.target.is_none() && eval.target_ignoring_cooldown.is_none());
}

#[test]
fn reset_clock_alone_never_clears_a_target_but_a_reading_at_the_reset_does() {
    let now = Utc::now();
    let reset = now - Duration::seconds(100);
    let mut cand = row_full("cand@e.com", 10.0, 10.0, now + Duration::days(2));
    cand.session.resets_at = Some(reset);
    let active = row_full("active@e.com", 96.0, 96.0, now + Duration::days(2));
    let guard = SwapGuard::default();
    // Read just before the reset passed: the reading predates the reset.
    cand.fetched_at = Some(reset.timestamp() - 1);
    let rows = vec![dup(&active), dup(&cand)];
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
        .target
        .is_none());
    // Read at the very second of the reset: counted as post-reset.
    cand.fetched_at = Some(reset.timestamp());
    let rows = vec![active, cand];
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
            .target
            .as_deref(),
        Some("cand@e.com")
    );
}

#[test]
fn no_return_window_lifts_after_twenty_minutes_and_cooldown_after_five() {
    let (Some(past_window), Some(at_edge), Some(cool_edge)) = (
        ago(NO_RETURN_SECS + 60),
        ago(NO_RETURN_SECS),
        ago(SWAP_COOLDOWN_SECS),
    ) else {
        return;
    };
    let reset = Utc::now() + Duration::hours(5);
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, reset + Duration::days(1)),
        row_full("back@e.com", 10.0, 10.0, reset),
    ];
    // Left 21 minutes ago, read again since: a normal target again.
    let mut guard = SwapGuard::default();
    guard.left_at.insert("back@e.com".to_string(), past_window);
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
            .target
            .as_deref(),
        Some("back@e.com")
    );
    // Exactly at the window edge it has served its time.
    guard.left_at.insert("back@e.com".to_string(), at_edge);
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
            .target
            .as_deref(),
        Some("back@e.com")
    );
    // Optional (proactive) swaps: a swap exactly 300s ago no longer cools down.
    let rows = vec![
        row_full("active@e.com", 40.0, 40.0, reset + Duration::days(1)),
        row_full("soon@e.com", 10.0, 10.0, reset),
    ];
    let guard = SwapGuard {
        last_swap: Some(cool_edge),
        ..SwapGuard::default()
    };
    let eval = evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard);
    assert!(!eval.blocked_by_cooldown);
    assert_eq!(eval.target.as_deref(), Some("soon@e.com"));
}

#[test]
fn proactive_flip_back_needs_a_full_margin_when_the_active_reset_is_unknown() {
    let reset = Utc::now() + Duration::hours(5);
    let guard = SwapGuard::default();
    let mut active = row_full("active@e.com", 0.0, 50.0, reset);
    active.weekly.resets_at = None;
    // 5 points of extra headroom: not worth leaving a working account.
    let rows = vec![dup(&active), row_full("c@e.com", 0.0, 45.0, reset)];
    assert!(evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
        .target
        .is_none());
    // Exactly the 10-point margin: worth it.
    let rows = vec![active, row_full("c@e.com", 0.0, 40.0, reset)];
    assert_eq!(
        evaluate_swap(&rows, "active@e.com", 95.0, 95.0, &guard)
            .target
            .as_deref(),
        Some("c@e.com")
    );
}

#[test]
fn blocked_target_is_chosen_only_by_a_strictly_future_reset_and_ties_keep_the_first() {
    let now = Utc::now();
    let guard = SwapGuard::default();
    let soon = now + Duration::hours(2);
    let active = row_full("active@e.com", 0.0, 100.0, now + Duration::days(5));
    // A reset that is exactly now has not recovered anything yet.
    let rows = vec![dup(&active), row_full("due@e.com", 0.0, 100.0, now)];
    assert!(earliest_blocked_target(&rows, &active, &guard, now).is_none());
    // Two equally early accounts: the first listed wins, no churn between them.
    let rows = vec![
        dup(&active),
        row_full("first@e.com", 0.0, 100.0, soon),
        row_full("second@e.com", 0.0, 100.0, soon),
    ];
    let best = earliest_blocked_target(&rows, &active, &guard, now).unwrap();
    assert_eq!(best.email, "first@e.com");
}

#[test]
fn blocked_preparation_skips_an_account_left_within_the_no_return_window() {
    let (Some(inside), Some(edge)) = (ago(NO_RETURN_SECS - 60), ago(NO_RETURN_SECS)) else {
        return;
    };
    let now = Utc::now();
    let active = row_full("active@e.com", 0.0, 100.0, now + Duration::days(5));
    let rows = vec![
        dup(&active),
        row_full("next@e.com", 0.0, 100.0, now + Duration::hours(2)),
    ];
    let mut guard = SwapGuard::default();
    guard.left_at.insert("next@e.com".to_string(), inside);
    assert!(earliest_blocked_target(&rows, &active, &guard, now).is_none());
    guard.left_at.insert("next@e.com".to_string(), edge);
    assert!(earliest_blocked_target(&rows, &active, &guard, now).is_some());
}

// --- mutation-hardening: swap verification + stale rescue ---

fn active_in_trouble_fixture() -> Vec<Row> {
    let reset = Utc::now() + Duration::hours(5);
    vec![
        row_full("active@e.com", 10.0, 10.0, reset),
        row_full("other@e.com", 99.0, 99.0, reset),
    ]
}

#[test]
fn active_in_trouble_means_lapsed_or_at_the_trigger_with_data() {
    let mut rows = active_in_trouble_fixture();
    // A healthy active account is not in trouble, whatever the others are at.
    assert!(!active_in_trouble(&rows, "active@e.com", 95.0));
    // An unknown account never is.
    assert!(!active_in_trouble(&rows, "ghost@e.com", 95.0));
    // At the trigger exactly.
    rows[0].weekly.pct = Some(95.0);
    assert!(active_in_trouble(&rows, "active@e.com", 95.0));
    rows[0].weekly.pct = Some(94.9);
    assert!(!active_in_trouble(&rows, "active@e.com", 95.0));
    // No reading: not in trouble by usage, but a lapsed plan is, data or not.
    rows[0].weekly.pct = Some(99.0);
    rows[0].fetched_at = None;
    assert!(!active_in_trouble(&rows, "active@e.com", 95.0));
    rows[0].no_subscription = true;
    assert!(active_in_trouble(&rows, "active@e.com", 95.0));
}

fn stale_row(email: &str, session: f64, age_secs: i64) -> Row {
    // One fixed reset for every row so ordering is decided by headroom alone.
    let reset = DateTime::from_timestamp(4_000_000_000, 0).unwrap();
    let mut r = row_full(email, session, 10.0, reset);
    r.fetched_at = Some(Utc::now().timestamp() - age_secs);
    r
}

#[test]
fn stale_swap_candidate_picks_the_best_row_that_could_become_a_target() {
    let guard = SwapGuard::default();
    let mut rows = vec![
        // Another provider's account first: must not set the active provider.
        {
            let mut r = stale_row("codex@e.com", 0.0, 900);
            r.provider_id = "codex".to_string();
            r
        },
        stale_row("active@e.com", 0.0, 900),
        {
            let mut r = stale_row("relogin@e.com", 0.0, 900);
            r.needs_relogin = true;
            r
        },
        {
            let mut r = stale_row("lapsed@e.com", 0.0, 900);
            r.no_subscription = true;
            r
        },
        {
            let mut r = stale_row("blank@e.com", 0.0, 900);
            r.fetched_at = None;
            r
        },
        stale_row("worse@e.com", 40.0, 900),
        stale_row("best@e.com", 5.0, 900),
    ];
    let pick =
        |rows: &[Row], tried: &[String]| stale_swap_candidate(rows, "active@e.com", &guard, tried);
    assert_eq!(pick(&rows, &[]).as_deref(), Some("best@e.com"));
    // Already re-checked this cycle: the next best.
    assert_eq!(
        pick(&rows, &["best@e.com".to_string()]).as_deref(),
        Some("worse@e.com")
    );
    // An unknown active account has no provider to match against.
    assert!(stale_swap_candidate(&rows, "ghost@e.com", &guard, &[]).is_none());
    // Fresh readings are not stale.
    for r in rows.iter_mut() {
        r.fetched_at = Some(Utc::now().timestamp() - 5);
    }
    assert!(pick(&rows, &[]).is_none());
}

#[test]
fn stale_swap_candidate_treats_a_reading_as_stale_only_past_the_verify_age() {
    let guard = SwapGuard::default();
    for _ in 0..5 {
        let t0 = Utc::now().timestamp();
        let rows = |age: i64| {
            vec![
                row_full("active@e.com", 96.0, 96.0, Utc::now() + Duration::days(1)),
                {
                    let mut r = stale_row("c@e.com", 5.0, 0);
                    r.fetched_at = Some(t0 - age);
                    r
                },
            ]
        };
        let old = stale_swap_candidate(
            &rows(TARGET_VERIFY_MAX_AGE_SECS + 1),
            "active@e.com",
            &guard,
            &[],
        );
        let edge = stale_swap_candidate(
            &rows(TARGET_VERIFY_MAX_AGE_SECS),
            "active@e.com",
            &guard,
            &[],
        );
        let young = stale_swap_candidate(
            &rows(TARGET_VERIFY_MAX_AGE_SECS - 1),
            "active@e.com",
            &guard,
            &[],
        );
        if Utc::now().timestamp() != t0 {
            continue;
        }
        assert_eq!(old.as_deref(), Some("c@e.com"));
        assert!(edge.is_none());
        assert!(young.is_none());
        return;
    }
    panic!("wall clock never stable");
}

#[test]
fn stale_swap_candidate_includes_an_account_left_before_its_last_reading_once_the_window_passes() {
    let (Some(inside), Some(edge)) = (ago(NO_RETURN_SECS - 60), ago(NO_RETURN_SECS)) else {
        return;
    };
    let rows = vec![
        row_full("active@e.com", 96.0, 96.0, Utc::now() + Duration::days(1)),
        // Read 25 minutes ago, i.e. before we left it ~20 minutes ago.
        stale_row("left@e.com", 5.0, 1500),
    ];
    let mut guard = SwapGuard::default();
    guard.left_at.insert("left@e.com".to_string(), inside);
    assert!(stale_swap_candidate(&rows, "active@e.com", &guard, &[]).is_none());
    guard.left_at.insert("left@e.com".to_string(), edge);
    assert_eq!(
        stale_swap_candidate(&rows, "active@e.com", &guard, &[]).as_deref(),
        Some("left@e.com")
    );
}

/// Mock usage endpoint: answers every request with `status` and `body`,
/// counting hits.
fn spawn_mock_usage_server(
    status: u16,
    body: String,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock usage server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut content_length = 0usize;
            {
                use std::io::Read;
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0
                        || line == "\r\n"
                        || line == "\n"
                    {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                // Drain the body so closing never resets the connection mid-request.
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
            }
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

/// A `/api/oauth/usage` body: both windows reset in the future.
fn usage_body(session: f64, weekly: f64) -> String {
    let session_reset = (Utc::now() + Duration::hours(3)).to_rfc3339();
    let weekly_reset = (Utc::now() + Duration::days(3)).to_rfc3339();
    serde_json::json!({
        "five_hour": {"utilization": session, "resets_at": session_reset},
        "seven_day": {"utilization": weekly, "resets_at": weekly_reset},
        "seven_day_opus": null,
    })
    .to_string()
}

/// A Claude account whose token is good for another hour (`expires_in_ms`
/// negative for an already-expired one).
fn claude_account(email: &str, expires_in_ms: i64) -> Account {
    let exp = Utc::now().timestamp_millis() + expires_in_ms;
    let mut a = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"at-{email}","refreshToken":"rt-{email}","expiresAt":{exp}}}}}"#
    ))
    .unwrap();
    a.email = Some(email.to_string());
    a
}

fn seed_claude_state(accounts: Vec<Account>, active: Option<&str>) {
    let mut st = State::default();
    for a in accounts {
        st.upsert(a);
    }
    st.active = active.map(str::to_string);
    st.save().unwrap();
}

fn hit_count(h: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
    h.load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn verify_swap_target_reads_and_persists_a_fresh_claude_reading() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(30.0, 20.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    seed_claude_state(vec![claude_account("t@e.com", 3_600_000)], None);
    let row = verify_swap_target(CLAUDE_SLUG, "t@e.com");
    usage::set_usage_url_override(None);
    let row = row.expect("a reachable target yields its fresh row");
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(row.email, "t@e.com");
    assert_eq!(row.session.pct, Some(30.0));
    assert_eq!(row.weekly.pct, Some(20.0));
    let saved = State::load().unwrap();
    let cu = saved.find("t@e.com").unwrap().cached_usage.clone().unwrap();
    assert_eq!(cu.session_pct, Some(30.0));
}

#[test]
fn verify_swap_target_never_fetches_for_a_dead_login() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(30.0, 20.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    let mut flagged = claude_account("flagged@e.com", 3_600_000);
    flagged.needs_relogin = true;
    seed_claude_state(
        vec![flagged, claude_account("expired@e.com", -60_000)],
        None,
    );
    let a = verify_swap_target(CLAUDE_SLUG, "flagged@e.com");
    let b = verify_swap_target(CLAUDE_SLUG, "expired@e.com");
    let c = verify_swap_target(CLAUDE_SLUG, "missing@e.com");
    usage::set_usage_url_override(None);
    assert!(a.is_none() && b.is_none() && c.is_none());
    assert_eq!(hit_count(&hits), 0);
}

#[test]
fn verify_swap_target_failure_is_none_and_backs_the_account_off() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(429, String::new());
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    let email = escalation_test_email("verify-429");
    seed_claude_state(vec![claude_account(&email, 3_600_000)], None);
    let row = verify_swap_target(CLAUDE_SLUG, &email);
    usage::set_usage_url_override(None);
    assert!(row.is_none());
    assert_eq!(hit_count(&hits), 1);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_429s, 1);
    assert!(t.backoff_until_ts.is_some());
    reset_usage_fetch_tracker(&email);
}

#[test]
fn verify_swap_target_skips_dead_non_claude_logins_without_fetching() {
    use crate::store::{ProviderAccount, ScopedConfigDir};
    let _g = ScopedConfigDir::new();
    let now = Utc::now().timestamp();
    let mk = |key: &str, expires_at: i64, needs_relogin: bool| ProviderAccount {
        key: key.to_string(),
        secret_blob: "{}".to_string(),
        access_token: "at".to_string(),
        refresh_token: String::new(),
        expires_at,
        identity_email: None,
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: None,
        notif_state: Default::default(),
        needs_relogin,
        no_subscription: false,
        plan: None,
    };
    let flagged = escalation_test_email("v-flagged");
    let expired = escalation_test_email("v-expired");
    let mut st = State::default();
    st.upsert_provider_account("codex", mk(&flagged, now + 3600, true));
    st.upsert_provider_account("codex", mk(&expired, now - 60, false));
    st.save().unwrap();
    assert!(verify_swap_target("codex", &flagged).is_none());
    assert!(verify_swap_target("codex", &expired).is_none());
    // Neither reached the usage endpoint: a fetch attempt (success or not)
    // would have left a failure on the account's tracker.
    for key in [&flagged, &expired] {
        let t = get_usage_fetch_tracker(&fetch_tracker_key("codex", key));
        assert_eq!(t.consecutive_failures, 0, "{key} must not be fetched");
    }
}

#[test]
fn verified_swap_rechecks_a_stale_target_and_trusts_a_fresh_one() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(20.0, 20.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    seed_claude_state(
        vec![
            claude_account("active@e.com", 3_600_000),
            claude_account("t@e.com", 3_600_000),
        ],
        Some("active@e.com"),
    );
    let guard = SwapGuard::default();
    // Read 10s ago: trusted as is, no request.
    let mut rows = vec![
        stale_row("active@e.com", 96.0, 5),
        stale_row("t@e.com", 10.0, 10),
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    assert_eq!(eval.target.as_deref(), Some("t@e.com"));
    assert_eq!(hit_count(&hits), 0);
    // Read 500s ago: re-checked, and the fresh reading replaces the row.
    let mut rows = vec![
        stale_row("active@e.com", 96.0, 5),
        stale_row("t@e.com", 10.0, 500),
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    usage::set_usage_url_override(None);
    assert_eq!(eval.target.as_deref(), Some("t@e.com"));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(rows[1].session.pct, Some(20.0));
    assert_eq!(
        rows[0].session.pct,
        Some(96.0),
        "only the target is re-read"
    );
}

#[test]
fn verified_swap_drops_a_target_whose_fresh_reading_is_at_the_trigger() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(99.0, 99.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    seed_claude_state(
        vec![
            claude_account("active@e.com", 3_600_000),
            claude_account("t1@e.com", 3_600_000),
            claude_account("t2@e.com", 3_600_000),
        ],
        Some("active@e.com"),
    );
    // t1 looks best but is stale; once re-read it is full, so t2 (read 10s ago)
    // is the next best.
    let mut rows = vec![
        stale_row("active@e.com", 96.0, 5),
        stale_row("t1@e.com", 1.0, 500),
        stale_row("t2@e.com", 30.0, 10),
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    usage::set_usage_url_override(None);
    assert_eq!(eval.target.as_deref(), Some("t2@e.com"));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(rows[1].session.pct, Some(99.0));
}

#[test]
fn verified_swap_looks_up_the_active_accounts_provider_by_email() {
    use crate::providers::claude::usage;
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(20.0, 20.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    seed_claude_state(
        vec![
            claude_account("active@e.com", 3_600_000),
            claude_account("t@e.com", 3_600_000),
        ],
        Some("active@e.com"),
    );
    // A Codex row listed first must not decide which provider is re-read.
    let mut other = stale_row("codex@e.com", 0.0, 10);
    other.provider_id = "codex".to_string();
    let mut rows = vec![
        other,
        stale_row("active@e.com", 96.0, 5),
        stale_row("t@e.com", 10.0, 500),
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &SwapGuard::default());
    usage::set_usage_url_override(None);
    assert_eq!(eval.target.as_deref(), Some("t@e.com"));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(rows[2].session.pct, Some(20.0));
}

#[test]
fn stale_rescue_rereads_an_excluded_account_when_the_active_one_must_be_left() {
    use crate::providers::claude::usage;
    let Some(left) = ago(NO_RETURN_SECS + 300) else {
        return;
    };
    let _g = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(200, usage_body(20.0, 20.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    seed_claude_state(
        vec![
            claude_account("active@e.com", 3_600_000),
            claude_account("back@e.com", 3_600_000),
        ],
        Some("active@e.com"),
    );
    // back@ was left ~25 min ago and its last reading predates that, so it is
    // excluded until re-read.
    let mut guard = SwapGuard::default();
    guard.left_at.insert("back@e.com".to_string(), left);
    let mut rows = vec![
        stale_row("active@e.com", 96.0, 5),
        stale_row("back@e.com", 10.0, 2400),
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    usage::set_usage_url_override(None);
    assert_eq!(eval.target.as_deref(), Some("back@e.com"));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(rows[1].session.pct, Some(20.0));
    assert_eq!(rows[0].session.pct, Some(96.0));
}

#[test]
fn stale_rescue_is_bounded_and_only_for_an_active_account_in_trouble() {
    use crate::providers::claude::usage;
    let Some(left) = ago(NO_RETURN_SECS + 300) else {
        return;
    };
    let _g = crate::store::ScopedConfigDir::new();
    // Re-reads come back full, so every rescue fails to produce a target.
    let (url, hits) = spawn_mock_usage_server(200, usage_body(99.0, 99.0));
    usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
    let names = ["a@e.com", "b@e.com", "c@e.com"];
    let mut accts = vec![claude_account("active@e.com", 3_600_000)];
    accts.extend(names.iter().map(|n| claude_account(n, 3_600_000)));
    seed_claude_state(accts, Some("active@e.com"));
    let mut guard = SwapGuard::default();
    for n in names {
        guard.left_at.insert(n.to_string(), left);
    }
    let build = |active_pct: f64| {
        let mut rows = vec![stale_row("active@e.com", active_pct, 5)];
        rows.extend(names.iter().map(|n| stale_row(n, 10.0, 2400)));
        rows
    };
    // Healthy active account: nothing to rescue, nothing fetched.
    let mut rows = build(40.0);
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    assert!(eval.target.is_none());
    assert_eq!(hit_count(&hits), 0);
    // Active account at the trigger: at most two re-reads per cycle.
    let mut rows = build(96.0);
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    usage::set_usage_url_override(None);
    assert!(eval.target.is_none());
    assert_eq!(hit_count(&hits), 2);
}

#[test]
fn unreachable_target_is_used_only_by_an_urgent_swap_with_ample_room() {
    let _g = crate::store::ScopedConfigDir::new();
    // No such account on disk: the re-check cannot happen.
    let guard = SwapGuard::default();
    let run = |active_pct: f64, target_pct: f64| {
        let mut rows = vec![
            stale_row("active@e.com", active_pct, 5),
            stale_row("t@e.com", target_pct, 500),
        ];
        let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
        (eval, rows)
    };
    // Urgent, target 15+ points under the trigger -> still the target.
    let (eval, _) = run(96.0, 80.0);
    assert!(eval.urgent);
    assert_eq!(eval.target.as_deref(), Some("t@e.com"));
    // Exactly UNVERIFIED_TARGET_MIN_HEADROOM_PTS (10) under the trigger.
    let (eval, _) = run(96.0, 85.0);
    assert_eq!(eval.target.as_deref(), Some("t@e.com"));
    // Closer to the trigger than that: dropped and marked.
    let (eval, rows) = run(96.0, 88.0);
    assert!(eval.target.is_none());
    assert!(rows[1].error.is_some());
    // Not urgent (a proactive flip-back): an unverified reading is never enough.
    let mut rows = vec![
        {
            let mut r = stale_row("active@e.com", 50.0, 5);
            r.weekly.resets_at = Some(Utc::now() + Duration::days(6));
            r
        },
        {
            let mut r = stale_row("t@e.com", 5.0, 500);
            r.weekly.resets_at = Some(Utc::now() + Duration::days(1));
            r
        },
    ];
    let eval = evaluate_swap_verified(&mut rows, "active@e.com", 95.0, 95.0, &guard);
    assert!(!eval.urgent);
    assert!(eval.target.is_none());
}

// --- mutation-hardening: burn-rate projection, trackers, plan checks ---

/// Seed `history` for a Claude account: `n` snapshots 20 minutes apart ending
/// now, with the session / weekly percent given by `pct(age_hours)`.
fn seed_history(email: &str, n: usize, pct: impl Fn(f64, usize) -> (f64, f64)) {
    for i in 0..n {
        let age_h = (n - 1 - i) as f64 / 3.0;
        let (s, w) = pct(age_h, i);
        usage_log::append(&usage_log::Snapshot {
            ts: Utc::now() - Duration::seconds((age_h * 3600.0) as i64),
            provider: CLAUDE_SLUG.to_string(),
            account: email.to_string(),
            session_pct: Some(s as f32),
            weekly_pct: Some(w as f32),
            active_model: None,
        })
        .unwrap();
    }
}

#[test]
fn projection_extends_each_window_by_its_burn_rate_and_takes_the_worse() {
    let _g = crate::store::ScopedConfigDir::new();
    // Session burns 10 pts/h, weekly 4 pts/h, both perfectly linear.
    seed_history("proj@e.com", 10, |age_h, _| {
        (60.0 - 10.0 * age_h, 80.0 - 4.0 * age_h)
    });
    let now = Utc::now();
    let project = |s: Option<f64>, w: Option<f64>, horizon: u64| {
        project_active_max_pct_at_horizon("proj@e.com", CLAUDE_SLUG, s, w, horizon, now)
    };
    // Half an hour ahead: session 60 -> 65, weekly 80 -> 82.
    let both = project(Some(60.0), Some(80.0), 1800).expect("both windows burn");
    assert!((both - 82.0).abs() < 0.5, "got {both}");
    let only_session = project(Some(60.0), None, 1800).unwrap();
    assert!((only_session - 65.0).abs() < 0.5, "got {only_session}");
    let only_weekly = project(None, Some(80.0), 1800).unwrap();
    assert!((only_weekly - 82.0).abs() < 0.5, "got {only_weekly}");
    // Clamped at 100%.
    assert_eq!(project(Some(60.0), None, 10 * 3600), Some(100.0));
    // Nothing to project from.
    assert!(project(None, None, 1800).is_none());
    // No history for another account.
    assert!(project_active_max_pct_at_horizon(
        "nobody@e.com",
        CLAUDE_SLUG,
        Some(60.0),
        Some(80.0),
        1800,
        now
    )
    .is_none());
}

#[test]
fn projection_ignores_flat_and_noisy_histories() {
    use burn_rate::{estimate, CONFIDENCE_FLOOR, FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR};
    let _g = crate::store::ScopedConfigDir::new();
    let now = Utc::now();
    // Creeping 0.3 pts/h: below the flat threshold, however clean the fit.
    seed_history("flat@e.com", 10, |age_h, _| {
        (40.0 - 0.3 * age_h, 40.0 - 0.3 * age_h)
    });
    let key = usage_log::AccountKey::new(CLAUDE_SLUG, "flat@e.com");
    let est = estimate(&key, providers::trait_def::Window::Session, now).unwrap();
    assert!(est.rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR);
    assert!(est.confidence >= CONFIDENCE_FLOOR);
    assert!(project_active_max_pct_at_horizon(
        "flat@e.com",
        CLAUDE_SLUG,
        Some(40.0),
        Some(40.0),
        3600,
        now
    )
    .is_none());
    // Rising overall but zig-zagging wildly: steep slope, no confidence.
    seed_history("noisy@e.com", 12, |age_h, i| {
        let jitter = if i % 2 == 0 { 35.0 } else { -35.0 };
        (50.0 - 8.0 * age_h + jitter, 50.0 - 8.0 * age_h + jitter)
    });
    let key = usage_log::AccountKey::new(CLAUDE_SLUG, "noisy@e.com");
    let est = estimate(&key, providers::trait_def::Window::Session, now).unwrap();
    assert!(est.rate_pct_per_hour > FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR);
    assert!(
        est.confidence < CONFIDENCE_FLOOR,
        "fixture: {}",
        est.confidence
    );
    assert!(project_active_max_pct_at_horizon(
        "noisy@e.com",
        CLAUDE_SLUG,
        Some(50.0),
        Some(50.0),
        3600,
        now
    )
    .is_none());
}

#[test]
fn loop_interval_is_remembered_for_the_projection_horizon() {
    let before = current_loop_interval_secs();
    set_current_loop_interval_secs(1200);
    assert_eq!(current_loop_interval_secs(), 1200);
    set_current_loop_interval_secs(30);
    assert_eq!(current_loop_interval_secs(), 30);
    set_current_loop_interval_secs(before);
}

#[test]
fn subscription_check_is_claimed_once_per_recheck_window() {
    let email = escalation_test_email("claim");
    let t0 = 1_000_000;
    assert!(claim_subscription_check(&email, t0, false), "first is due");
    assert!(!claim_subscription_check(&email, t0 + 1, false));
    let edge = SUBSCRIPTION_RECHECK_SECS as i64;
    assert!(!claim_subscription_check(&email, t0 + edge - 1, false));
    // A forced check is always due, even right after one.
    assert!(claim_subscription_check(&email, t0 + 2, true));
    // ...and restarts the window from the forced check.
    assert!(!claim_subscription_check(&email, t0 + 2 + edge - 1, false));
    assert!(claim_subscription_check(&email, t0 + 2 + edge, false));
    reset_usage_fetch_tracker(&email);
}

#[test]
fn clearing_inactive_backoff_keeps_the_429_count() {
    let email = escalation_test_email("clear-backoff");
    record_usage_fetch_429(&email);
    record_inactive_fetch_failure(&email, 1_000);
    record_inactive_fetch_failure(&email, 1_000);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 2);
    assert!(t.backoff_until_ts.is_some());
    clear_inactive_fetch_backoff(&email);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 0);
    assert_eq!(t.backoff_until_ts, None);
    assert_eq!(t.consecutive_429s, 1);
    // An account never seen is a no-op, not an entry.
    clear_inactive_fetch_backoff(&escalation_test_email("never-seen"));
    reset_usage_fetch_tracker(&email);
}

#[test]
fn non_429_error_only_resets_an_existing_counter() {
    let email = escalation_test_email("non429");
    record_usage_fetch_429(&email);
    record_usage_fetch_429(&email);
    assert_eq!(get_usage_fetch_tracker(&email).consecutive_429s, 2);
    record_usage_fetch_non_429(&email);
    assert_eq!(get_usage_fetch_tracker(&email).consecutive_429s, 0);
    reset_usage_fetch_tracker(&email);
}

/// Serializes the tests that observe or move the process-global "last polled
/// active account" (every `refresh_usage_cache` call touches it).
static POLL_ACTIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn note_polled_active_reports_only_a_change_of_active_account() {
    let _l = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Other (unlocked) tests may poll concurrently; accept the first clean pass.
    for _ in 0..50 {
        note_polled_active(Some("a@e.com"));
        let same = note_polled_active(Some("a@e.com"));
        let switched = note_polled_active(Some("b@e.com"));
        let same_again = note_polled_active(Some("b@e.com"));
        let cleared = note_polled_active(None);
        let cleared_again = note_polled_active(None);
        if !same && switched && !same_again && cleared && !cleared_again {
            return;
        }
    }
    panic!("note_polled_active never behaved like a change detector");
}

/// A provider whose plan check answers with a canned `PlanStatus`.
struct PlanProvider(Option<crate::providers::trait_def::PlanStatus>);

impl crate::providers::Provider for PlanProvider {
    fn provider_id(&self) -> &'static str {
        "mock-plan"
    }
    fn display_name(&self) -> &'static str {
        "Mock"
    }
    fn capabilities(&self) -> crate::providers::trait_def::Capabilities {
        crate::providers::trait_def::Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: false,
            supports_email_capture: false,
            secret_backend: crate::providers::trait_def::SecretBackend::Keychain,
            capture_mode: crate::providers::trait_def::CaptureMode::CredsOnDisk,
        }
    }
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
    fn parse_stored_blob(
        &self,
        _blob: &str,
    ) -> crate::providers::trait_def::PResult<crate::providers::trait_def::TokenGrant> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn patch_stored_blob(
        &self,
        _blob: &str,
        _grant: &crate::providers::trait_def::TokenGrant,
    ) -> crate::providers::trait_def::PResult<String> {
        Err(crate::providers::trait_def::ProviderError::Unsupported)
    }
    fn check_plan(&self, _access_token: &str) -> Option<crate::providers::trait_def::PlanStatus> {
        self.0.clone()
    }
}

fn plan(label: &str, active: bool) -> crate::providers::trait_def::PlanStatus {
    crate::providers::trait_def::PlanStatus {
        label: Some(label.to_string()),
        active,
    }
}

#[test]
fn lapsed_plan_recheck_distinguishes_restored_still_lapsed_and_unknown() {
    let _g = crate::store::ScopedConfigDir::new();
    let key = escalation_test_email("recheck");
    // Failures earned while lapsed are forgotten once the plan is back.
    record_inactive_fetch_failure(&key, 1_000);
    let restored = PlanProvider(Some(plan("Pro", true)));
    assert!(matches!(
        recheck_lapsed_plan(&restored, &key, "tok"),
        Recheck::Restored(p) if p.active
    ));
    assert_eq!(get_usage_fetch_tracker(&key).backoff_until_ts, None);
    // Still lapsed: keeps its backoff.
    record_inactive_fetch_failure(&key, 1_000);
    let lapsed = PlanProvider(Some(plan("Free", false)));
    assert!(matches!(
        recheck_lapsed_plan(&lapsed, &key, "tok"),
        Recheck::StillLapsed(p) if !p.active
    ));
    assert!(get_usage_fetch_tracker(&key).backoff_until_ts.is_some());
    assert!(matches!(
        recheck_lapsed_plan(&PlanProvider(None), &key, "tok"),
        Recheck::Unknown
    ));
    reset_usage_fetch_tracker(&key);
}

#[test]
fn lapse_detection_is_throttled_and_only_reports_a_confirmed_lapse() {
    let key = escalation_test_email("detect");
    let lapsed = PlanProvider(Some(plan("Free", false)));
    let active = PlanProvider(Some(plan("Pro", true)));
    // Active plan: not a lapse (and this consumes the throttle window).
    assert!(detect_lapsed_plan(&active, &key, "tok", 5_000).is_none());
    // Within the window nothing is even asked.
    assert!(detect_lapsed_plan(&lapsed, &key, "tok", 5_001).is_none());
    let later = 5_000 + SUBSCRIPTION_RECHECK_SECS as i64;
    let got = detect_lapsed_plan(&lapsed, &key, "tok", later).expect("confirmed lapse");
    assert!(!got.active);
    reset_usage_fetch_tracker(&key);
}

fn read_log() -> String {
    std::fs::read_to_string(store::config_dir().unwrap().join("usagio.log")).unwrap_or_default()
}

#[test]
fn persist_plan_records_lapse_and_renewal_for_claude() {
    let _g = crate::store::ScopedConfigDir::new();
    seed_claude_state(vec![claude_account("p@e.com", 3_600_000)], None);
    persist_plan(CLAUDE_SLUG, "p@e.com", &plan("Free", false), None);
    let a = State::load().unwrap().find("p@e.com").cloned().unwrap();
    assert!(a.no_subscription);
    assert_eq!(a.plan.as_deref(), Some("Free"));
    assert!(read_log().contains("event=subscription_lapsed provider=claude account=p@e.com"));
    // Same state again is not a transition.
    let lapsed_events = read_log().matches("event=subscription_lapsed").count();
    persist_plan(CLAUDE_SLUG, "p@e.com", &plan("Free", false), None);
    assert_eq!(
        read_log().matches("event=subscription_lapsed").count(),
        lapsed_events
    );
    persist_plan(CLAUDE_SLUG, "p@e.com", &plan("Pro", true), None);
    let a = State::load().unwrap().find("p@e.com").cloned().unwrap();
    assert!(!a.no_subscription);
    assert_eq!(a.plan.as_deref(), Some("Pro"));
    assert!(read_log().contains("event=subscription_restored provider=claude account=p@e.com"));
}

#[test]
fn persist_plan_for_other_providers_also_keeps_rotated_tokens() {
    use crate::store::ProviderAccount;
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    st.upsert_provider_account(
        "codex",
        ProviderAccount {
            key: "c@e.com".to_string(),
            secret_blob: "{}".to_string(),
            access_token: "old-at".to_string(),
            refresh_token: "old-rt".to_string(),
            expires_at: 1,
            identity_email: None,
            identity_uuid: None,
            identity_display_name: None,
            identity_native_blob: serde_json::Value::Null,
            cached_usage: None,
            notif_state: Default::default(),
            needs_relogin: false,
            no_subscription: false,
            plan: None,
        },
    );
    st.save().unwrap();
    let rotated = ("new-at".to_string(), "new-rt".to_string(), 99);
    persist_plan("codex", "c@e.com", &plan("Plus", false), Some(&rotated));
    let st = State::load().unwrap();
    let a = st.find_provider_account("codex", "c@e.com").unwrap();
    assert!(a.no_subscription);
    assert_eq!(a.plan.as_deref(), Some("Plus"));
    assert_eq!(
        (
            a.access_token.as_str(),
            a.refresh_token.as_str(),
            a.expires_at
        ),
        ("new-at", "new-rt", 99)
    );
    assert!(read_log().contains("event=subscription_lapsed provider=codex account=c@e.com"));
}

#[test]
fn flag_needs_relogin_marks_only_that_account() {
    let _g = crate::store::ScopedConfigDir::new();
    seed_claude_state(
        vec![
            claude_account("dead@e.com", 3_600_000),
            claude_account("fine@e.com", 3_600_000),
        ],
        None,
    );
    flag_needs_relogin("dead@e.com");
    let st = State::load().unwrap();
    assert!(st.find("dead@e.com").unwrap().needs_relogin);
    assert!(!st.find("fine@e.com").unwrap().needs_relogin);
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
fn consumption_deltas_ignore_unchanged_readings() {
    let a = sample("a@e.com", 10.0, 100);
    let same = sample("a@e.com", 10.0, 200);
    let up = sample("a@e.com", 12.0, 300);
    let got = consumption_deltas(&[&a, &same, &up]);
    assert_eq!(got, vec![(300, 2.0)]);
}

#[test]
fn env_override_names_the_variable_that_bypasses_the_login() {
    assert_eq!(
        env_override_var_for(CLAUDE_SLUG),
        Some("CLAUDE_CODE_OAUTH_TOKEN")
    );
    assert_eq!(CLAUDE_ENV_OVERRIDE_VAR, "CLAUDE_CODE_OAUTH_TOKEN");
    assert_eq!(env_override_var_for("codex"), None);
    assert_eq!(env_override_var_for("nope"), None);
}

/// Child half of `env_override_is_active_only_for_a_non_empty_variable`: a
/// no-op unless the parent launched it with an expectation.
#[test]
fn env_override_child_probe() {
    let Ok(expect) = std::env::var("USAGIO_PROBE_EXPECT_OVERRIDE") else {
        return;
    };
    assert_eq!(env_override_active(CLAUDE_SLUG), expect == "1");
    assert!(!env_override_active("codex"));
}

/// Real-environment check, run in a child process so flipping the variable
/// can never race other tests reading it.
#[test]
fn env_override_is_active_only_for_a_non_empty_variable() {
    let exe = std::env::current_exe().unwrap();
    let probe = |value: Option<&str>, expect: &str| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["--exact", "tests::env_override_child_probe", "--nocapture"])
            .env("USAGIO_PROBE_EXPECT_OVERRIDE", expect)
            .env_remove(CLAUDE_ENV_OVERRIDE_VAR);
        if let Some(v) = value {
            cmd.env(CLAUDE_ENV_OVERRIDE_VAR, v);
        }
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success(), "probe({value:?}) failed:\n{text}");
        assert!(text.contains("1 passed"), "probe did not run:\n{text}");
    };
    probe(Some("tok"), "1");
    probe(Some(""), "0");
    probe(None, "0");
}

#[test]
fn swap_support_needs_both_usage_and_switching() {
    for p in providers::all() {
        let c = p.capabilities();
        assert_eq!(
            provider_supports_swap(p.provider_id()),
            c.supports_usage && c.supports_switching,
            "{}",
            p.provider_id()
        );
    }
    assert!(!provider_supports_swap("not-a-provider"));
}

// --- mutation-hardening: switching, identity, keychain sync ---

fn claude_account_with_identity(email: &str, expires_in_ms: i64) -> Account {
    let mut a = claude_account(email, expires_in_ms);
    a.oauth_account = Some(serde_json::json!({
        "emailAddress": email,
        "accountUuid": format!("uuid-{email}"),
    }));
    a
}

/// Run `f` with `$HOME` and the config dir both pointing at a scratch dir
/// holding a `~/.claude.json` for `json_email`.
fn with_claude_home<R>(json_email: &str, f: impl FnOnce(&std::path::Path) -> R) -> R {
    let scd = crate::store::ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        std::fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "emailAddress": json_email,
                    "accountUuid": format!("uuid-{json_email}"),
                },
                "userID": "user-1",
            })
            .to_string(),
        )
        .unwrap();
        f(&home)
    })
}

/// Put `blob` where the vendor CLI's active login lives on this OS.
fn prime_active_slot(blob: &str) {
    providers::get(CLAUDE_SLUG)
        .unwrap()
        .mirror_rotated_token(blob)
        .unwrap();
}

fn keychain_access_token() -> Option<String> {
    keychain_read().map(|b| Account::from_keychain_blob(&b).unwrap().access_token)
}

#[test]
fn claude_json_helpers_read_write_and_report_the_file() {
    with_claude_home("who@e.com", |home| {
        assert_eq!(claude_json_path().unwrap(), home.join(".claude.json"));
        let (oauth, uid) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("who@e.com")
        );
        assert_eq!(uid.as_deref(), Some("user-1"));

        write_claude_identity(&serde_json::json!({"emailAddress": "new@e.com"}), None).unwrap();
        let (oauth, uid) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("new@e.com")
        );
        assert_eq!(uid, None, "an unknown userID must not be left behind");

        let (bytes, _mode) = read_claude_json_raw().unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("new@e.com"));

        std::fs::remove_file(home.join(".claude.json")).unwrap();
        assert_eq!(read_claude_identity(), (None, None));
        assert!(read_claude_json_raw().is_none());
    });
}

#[test]
fn claude_json_mode_reports_the_file_permissions_with_a_private_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.json");
    assert_eq!(claude_json_mode(&missing), 0o600);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.path().join("f.json");
        std::fs::write(&p, "{}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(claude_json_mode(&p) & 0o777, 0o640);
    }
    #[cfg(not(unix))]
    {
        let p = dir.path().join("f.json");
        std::fs::write(&p, "{}").unwrap();
        assert_eq!(claude_json_mode(&p), 0o600);
    }
}

#[test]
fn resolve_identity_prefers_the_stored_identity_without_the_network() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let acct = claude_account_with_identity("id@e.com", 3_600_000);
    let (identity, backfilled) = resolve_identity(provider, &acct).unwrap();
    assert_eq!(
        identity.get("emailAddress").and_then(|v| v.as_str()),
        Some("id@e.com")
    );
    assert!(!backfilled);
}

#[test]
fn ensure_fresh_with_fallback_reports_whether_it_refreshed() {
    use crate::providers::claude::oauth;
    let _g = crate::store::ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_token_server();
    oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let mut fresh = claude_account("f@e.com", 3_600_000);
    let r1 = ensure_fresh_with_fallback(provider, "f@e.com", &mut fresh);
    let after_fresh = hit_count(&hits);
    let mut expired = claude_account("x@e.com", -60_000);
    let r2 = ensure_fresh_with_fallback(provider, "x@e.com", &mut expired);
    oauth::set_token_url_override(None);
    assert!(matches!(r1, Ok(false)));
    assert_eq!(after_fresh, 0);
    assert!(matches!(r2, Ok(true)));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(expired.access_token, "mock-refreshed-access-token");
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn switching_refreshes_an_inactive_account_but_adopts_for_the_active_one() {
    use crate::providers::claude::oauth;
    with_claude_home("a@e.com", |_home| {
        let (base_url, hits) = spawn_mock_token_server();
        oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));
        let a = claude_account_with_identity("a@e.com", -60_000);
        let b = claude_account_with_identity("b@e.com", -60_000);
        prime_active_slot(&a.keychain_blob);
        seed_claude_state(vec![a, b], Some("a@e.com"));

        // b is inactive and expired: usagio owns its token, so it refreshes.
        let label = switch_to("b@e.com", false).unwrap();
        assert_eq!(label, "b@e.com");
        assert_eq!(hit_count(&hits), 1);
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("b@e.com"));
        assert_eq!(
            st.find("b@e.com").unwrap().access_token,
            "mock-refreshed-access-token"
        );

        // Expire b again. It is now ACTIVE: its token belongs to the vendor
        // CLI and must never be POSTed.
        let mut st = State::load().unwrap();
        st.find_mut("b@e.com").unwrap().expires_at = Utc::now().timestamp_millis() - 60_000;
        st.save().unwrap();
        switch_to("b@e.com", false).unwrap();
        oauth::set_token_url_override(None);
        assert_eq!(hit_count(&hits), 1, "the active account is never refreshed");
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_commits_only_while_the_expected_account_is_active() {
    with_claude_home("a@e.com", |_home| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();

        // The decision was made while "z" was active; it is not any more.
        let skipped = switch_to_guarded(
            provider,
            "b@e.com",
            &acct,
            &identity,
            false,
            Some("z@e.com"),
            false,
        )
        .unwrap();
        assert!(skipped.is_none());
        assert_eq!(State::load().unwrap().active.as_deref(), Some("a@e.com"));

        let done = switch_to_guarded(
            provider,
            "b@e.com",
            &acct,
            &identity,
            false,
            Some("a@e.com"),
            false,
        )
        .unwrap();
        assert_eq!(done.as_deref(), Some("b@e.com"));
        assert_eq!(State::load().unwrap().active.as_deref(), Some("b@e.com"));
        assert_eq!(keychain_access_token().as_deref(), Some("at-b@e.com"));
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_writes_the_fresher_of_the_two_token_sets() {
    with_claude_home("a@e.com", |_home| {
        let mut state_side = claude_account_with_identity("b@e.com", 3_600_000);
        state_side.set_tokens("at-state".into(), "rt-state".into(), 5_000);
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                state_side,
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let identity = serde_json::json!({"emailAddress": "b@e.com"});
        let phase1 = |expires_at: i64| {
            let mut a = claude_account_with_identity("b@e.com", 3_600_000);
            a.set_tokens("at-phase1".into(), "rt-phase1".into(), expires_at);
            a
        };
        // A concurrent poll rotated b after our snapshot: the newer set wins.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(4_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-state"));
        // Same expiry: our own snapshot is kept.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(5_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-phase1"));
        // Ours is newer: ours.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(6_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-phase1"));
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn only_an_explicit_hold_request_on_an_over_trigger_account_holds() {
    with_claude_home("a@e.com", |_home| {
        let mut over = claude_account_with_identity("over@e.com", 3_600_000);
        over.cached_usage = Some(usage_at(99.0));
        let mut calm = claude_account_with_identity("calm@e.com", 3_600_000);
        calm.cached_usage = Some(usage_at(10.0));
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                over,
                calm,
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let go = |email: &str, hold: bool| {
            let acct = State::load().unwrap().find(email).cloned().unwrap();
            let identity = acct.oauth_account.clone().unwrap();
            switch_to_guarded(provider, email, &acct, &identity, false, None, hold).unwrap();
            State::load()
                .unwrap()
                .manual_hold(CLAUDE_SLUG)
                .map(str::to_string)
        };
        assert_eq!(go("over@e.com", true).as_deref(), Some("over@e.com"));
        assert_eq!(
            go("over@e.com", false),
            None,
            "an auto move clears the hold"
        );
        assert_eq!(
            go("calm@e.com", true),
            None,
            "under the trigger nothing to ack"
        );
    });
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
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn keychain_rotation_is_adopted_only_for_the_matching_identity() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let rotated = claude_account_with_identity("a@e.com", 9_000_000);
    let mut rotated_blob = rotated.clone();
    rotated_blob.set_tokens("at-rotated".into(), "rt-rotated".into(), 9_999);
    let make_state = || {
        let mut st = State::default();
        st.upsert(claude_account_with_identity("a@e.com", 3_600_000));
        st.active = Some("a@e.com".to_string());
        st
    };
    // The vendor CLI is logged in as a@: its rotation is ours.
    with_claude_home("a@e.com", |_| {
        keychain_write(&rotated_blob.keychain_blob).unwrap();
        let mut st = make_state();
        sync_active_from_keychain(provider, &mut st);
        let a = st.find("a@e.com").unwrap();
        assert_eq!(a.access_token, "at-rotated");
        assert_eq!(a.expires_at, 9_999);
    });
    // Logged in as someone else: the keychain is not our account, hands off.
    with_claude_home("stranger@e.com", |_| {
        keychain_write(&rotated_blob.keychain_blob).unwrap();
        let mut st = make_state();
        sync_active_from_keychain(provider, &mut st);
        assert_eq!(st.find("a@e.com").unwrap().access_token, "at-a@e.com");
    });
}

#[test]
fn inactive_rotation_is_mirrored_only_while_the_vendor_cli_is_that_account() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let mut acct = claude_account("a@e.com", 3_600_000);
    acct.set_tokens("at-mirror".into(), "rt-mirror".into(), 7_777);
    with_claude_home("a@e.com", |_| {
        prime_active_slot(&claude_account("a@e.com", 1_000).keychain_blob);
        let got = mirror_inactive_rotation(provider, &acct);
        assert!(matches!(got, Some(Ok(()))), "{got:?}");
        let slot = provider.read_active_slot().unwrap().unwrap();
        assert_eq!(
            Account::from_keychain_blob(&slot).unwrap().access_token,
            "at-mirror"
        );
    });
    with_claude_home("stranger@e.com", |_| {
        let before = claude_account("stranger@e.com", 1_000).keychain_blob;
        prime_active_slot(&before);
        assert!(mirror_inactive_rotation(provider, &acct).is_none());
        assert_eq!(provider.read_active_slot().unwrap().unwrap(), before);
    });
}

#[test]
fn provider_active_reads_each_providers_own_slot() {
    let mut st = State {
        active: Some("c@e.com".to_string()),
        ..State::default()
    };
    st.provider_accounts_mut("codex").active = Some("x@e.com".to_string());
    assert_eq!(
        provider_active(&st, CLAUDE_SLUG).as_deref(),
        Some("c@e.com")
    );
    assert_eq!(provider_active(&st, "codex").as_deref(), Some("x@e.com"));
    assert_eq!(provider_active(&st, "opencode"), None);
}

#[test]
fn select_email_resolves_a_selector_or_auto_picks_without_one() {
    let mut st = State::default();
    let mut busy = claude_account("busy@e.com", 3_600_000);
    busy.cached_usage = Some(usage_at(80.0));
    let mut calm = claude_account("calm@e.com", 3_600_000);
    calm.cached_usage = Some(usage_at(10.0));
    st.upsert(busy);
    st.upsert(calm);
    assert_eq!(select_email(&st, None).unwrap(), "calm@e.com");
    assert_eq!(select_email(&st, Some("busy")).unwrap(), "busy@e.com");
    assert!(select_email(&st, Some("nobody")).is_err());
}

#[test]
fn auto_pick_fallback_only_considers_accounts_with_a_reading_and_room() {
    let reset = Utc::now() + Duration::days(1);
    let mut blank = row_full("blank@e.com", 0.0, 0.0, reset);
    blank.fetched_at = None;
    let rows = vec![
        blank,
        row_full("full@e.com", 100.0, 100.0, reset),
        row_full("near@e.com", 97.0, 97.0, reset),
    ];
    // Everything with a reading is past the 95% target: the least-full one
    // that still has room, not the unread account and not the maxed one.
    assert_eq!(auto_pick(&rows, 95.0, 95.0).unwrap(), "near@e.com");
}

#[test]
fn switching_codex_resets_the_tracker_of_the_account_left_behind_only() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        for (who, at, rt) in [("a", "at-a", "rt-a"), ("b", "at-b", "rt-b")] {
            let blob = codex_auth_json(&format!("{who}@example.com"), 4_000_000_000, at, rt);
            std::fs::write(dir.path().join("auth.json"), blob).unwrap();
            capture_current_generic("codex").unwrap();
        }
        let ka = fetch_tracker_key("codex", "a@example.com");
        let kb = fetch_tracker_key("codex", "b@example.com");
        record_usage_fetch_429(&ka);
        record_usage_fetch_429(&kb);
        // b is active; moving to a forgets b's 429s, not a's.
        switch_to_provider_account("codex", "a@example.com", false).unwrap();
        assert_eq!(get_usage_fetch_tracker(&kb).consecutive_429s, 0);
        assert_eq!(get_usage_fetch_tracker(&ka).consecutive_429s, 1);
        // Re-selecting the active account is not a departure.
        switch_to_provider_account("codex", "a@example.com", false).unwrap();
        assert_eq!(get_usage_fetch_tracker(&ka).consecutive_429s, 1);
        reset_usage_fetch_tracker(&ka);
        reset_usage_fetch_tracker(&kb);
    });
}

#[test]
fn compare_and_set_switch_for_other_providers_respects_the_expected_active_account() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        for (who, at, rt) in [("a", "at-a", "rt-a"), ("b", "at-b", "rt-b")] {
            let blob = codex_auth_json(&format!("{who}@example.com"), 4_000_000_000, at, rt);
            std::fs::write(dir.path().join("auth.json"), blob).unwrap();
            capture_current_generic("codex").unwrap();
        }
        // b is active. A decision made while a was active is stale.
        let stale = switch_if_still_active("codex", "a@example.com", "a@example.com").unwrap();
        assert_eq!(stale, None);
        assert_eq!(
            provider_active(&State::load().unwrap(), "codex").as_deref(),
            Some("b@example.com")
        );
        // The expectation compares case-insensitively.
        let moved = switch_if_still_active("codex", "a@example.com", "B@Example.com").unwrap();
        assert_eq!(moved.as_deref(), Some("a@example.com"));
        assert_eq!(
            provider_active(&State::load().unwrap(), "codex").as_deref(),
            Some("a@example.com")
        );
    });
}

// --- mutation-hardening: the Claude usage refresh policy ---

fn uniq(tag: &str) -> String {
    escalation_test_email(tag)
}

/// A cached reading `age` seconds old.
fn cu(session: f64, weekly: f64, age: i64) -> CachedUsage {
    CachedUsage {
        session_pct: Some(session),
        weekly_pct: Some(weekly),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: Utc::now().timestamp() - age,
        reported: Default::default(),
    }
}

/// A session-locked reading (100% until a reset three hours out), `age` old.
fn cu_locked(age: i64) -> CachedUsage {
    let mut c = cu(100.0, 10.0, age);
    c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
    c
}

fn account_with_cache(email: &str, cache: Option<CachedUsage>) -> Account {
    let mut a = claude_account_with_identity(email, 3_600_000);
    a.cached_usage = cache;
    a
}

struct Fx {
    accounts: Vec<Account>,
    active: Option<String>,
    force: bool,
    status: u16,
    body: String,
    /// Make this look like the first poll since a switch.
    just_switched: bool,
    /// Token endpoint behaviour, if the scenario refreshes tokens.
    token: Option<(u16, String)>,
    /// Runs inside the scratch config dir before the pass (e.g. seed history).
    before: Option<Box<dyn FnOnce()>>,
}

impl Fx {
    fn new(accounts: Vec<Account>, active: Option<&str>) -> Self {
        Fx {
            accounts,
            active: active.map(str::to_string),
            force: false,
            status: 200,
            body: usage_body(10.0, 10.0),
            just_switched: false,
            token: None,
            before: None,
        }
    }
    fn status(mut self, status: u16, body: &str) -> Self {
        self.status = status;
        self.body = body.to_string();
        self
    }
    fn force(mut self) -> Self {
        self.force = true;
        self
    }
    fn just_switched(mut self) -> Self {
        self.just_switched = true;
        self
    }
}

/// Run one `refresh_usage_cache` pass against mock endpoints inside a scratch
/// `$HOME`; `check` sees the outcome, the usage-endpoint hit count and the
/// token-endpoint hit count while the scratch state still exists.
fn run_refresh<R>(fx: Fx, check: impl FnOnce(&RefreshOutcome, usize, usize) -> R) -> R {
    use crate::providers::claude::{oauth, usage};
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(fx.status, fx.body.clone());
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        let token = fx.token.clone().map(|(s, b)| spawn_mock_usage_server(s, b));
        if let Some((u, _)) = &token {
            oauth::set_token_url_override(Some(&format!("{u}/v1/oauth/token")));
        }
        let now = Utc::now().timestamp();
        for a in &fx.accounts {
            // Throttle the lapse probe so a 403/429 never reaches the profile
            // endpoint.
            claim_subscription_check(a.key(), now, false);
        }
        if let Some(active) = &fx.active {
            let blob = fx
                .accounts
                .iter()
                .find(|a| a.key() == active)
                .unwrap()
                .keychain_blob
                .clone();
            prime_active_slot(&blob);
        }
        seed_claude_state(fx.accounts.clone(), fx.active.as_deref());
        if let Some(before) = fx.before {
            before();
        }
        if fx.just_switched {
            note_polled_active(Some("someone-else@e.com"));
        } else {
            note_polled_active(fx.active.as_deref());
        }
        let out = refresh_usage_cache(fx.force);
        usage::set_usage_url_override(None);
        oauth::set_token_url_override(None);
        let token_hits = token.as_ref().map(|(_, h)| hit_count(h)).unwrap_or(0);
        check(&out, hit_count(&hits), token_hits)
    })
}

/// Number of usage requests a pass makes.
fn fetches(fx: Fx) -> usize {
    run_refresh(fx, |_, hits, _| hits)
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
fn skipping_is_decided_per_poll_not_remembered_by_the_subscription_throttle() {
    use crate::providers::claude::usage;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let email = uniq("repoll");
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(500, String::new());
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        seed_claude_state(
            vec![account_with_cache(&email, Some(cu(10.0, 10.0, 2000)))],
            None,
        );
        note_polled_active(None);
        refresh_usage_cache(false);
        assert_eq!(hit_count(&hits), 1);
        // The failure put the account in backoff; forget that and poll again.
        // Nothing else stands in the way of fetching it again.
        clear_inactive_fetch_backoff(&email);
        refresh_usage_cache(false);
        usage::set_usage_url_override(None);
        assert_eq!(hit_count(&hits), 2);
    });
    reset_usage_fetch_tracker(&email);
}

#[test]
fn locked_inactive_accounts_are_skipped_until_stale_or_forced() {
    let e = uniq("locked");
    let one = |cache: CachedUsage, force: bool| {
        let fx = Fx::new(vec![account_with_cache(&e, Some(cache))], None);
        fetches(if force { fx.force() } else { fx })
    };
    let hour = 3600;
    // Locked until a reset hours away, read 5h ago: nothing to learn.
    assert_eq!(one(cu_locked(5 * hour), false), 0);
    // The same account read 13h ago may have been reset early: look again.
    assert_eq!(one(cu_locked(13 * hour), false), 1);
    // A manual refresh always looks.
    assert_eq!(one(cu_locked(5 * hour), true), 1);
    // Not locked, merely old: polled.
    assert_eq!(one(cu(10.0, 10.0, 5 * hour), false), 1);
    reset_usage_fetch_tracker(&e);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn the_active_account_is_polled_even_when_it_looks_locked() {
    let e = uniq("locked-active");
    let hits = fetches(Fx::new(
        vec![account_with_cache(&e, Some(cu_locked(5 * 3600)))],
        Some(&e),
    ));
    assert_eq!(hits, 1);
}

#[test]
fn inactive_accounts_wait_out_their_floor_and_their_failure_backoff() {
    let e = uniq("floor");
    let one =
        |cache: CachedUsage| fetches(Fx::new(vec![account_with_cache(&e, Some(cache))], None));
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    assert_eq!(one(cu(10.0, 10.0, floor - 300)), 0, "read 5 minutes ago");
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1, "past the floor");
    // A clock that ran backwards (reading from the future) is not "fresh".
    assert_eq!(one(cu(10.0, 10.0, -100)), 1);
    // A reset crossed since the reading: recheck at once.
    let mut crossed = cu(10.0, 10.0, 300);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
    assert_eq!(one(crossed), 1);
    // Failure backoff holds a stale account off...
    record_inactive_fetch_failure(&e, Utc::now().timestamp());
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 0);
    // ...unless a reset crossing makes its reading untrustworthy.
    let mut crossed = cu(10.0, 10.0, floor + 100);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
    assert_eq!(one(crossed), 1);
    reset_usage_fetch_tracker(&e);
    // Once the backoff has lapsed it is polled again.
    record_inactive_fetch_failure(&e, Utc::now().timestamp() - 10_000);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    reset_usage_fetch_tracker(&e);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn the_active_account_follows_its_own_tier_not_the_inactive_floor() {
    let e = uniq("active-floor");
    let one = |cache: CachedUsage, fx: fn(Fx) -> Fx| {
        fetches(fx(Fx::new(
            vec![account_with_cache(&e, Some(cache))],
            Some(&e),
        )))
    };
    let same = |fx: Fx| fx;
    // 93% of a 95% trigger polls every 30s: a 5-minute-old reading is stale
    // even though an inactive account would still be inside its floor.
    assert_eq!(one(cu(93.0, 10.0, 300), same), 1);
    // A comfortable account polls every 3 minutes.
    assert_eq!(one(cu(10.0, 10.0, 10), same), 0);
    assert_eq!(one(cu(10.0, 10.0, 1000), same), 1);
    // Reading from the future is not fresh.
    assert_eq!(one(cu(10.0, 10.0, -100), same), 1);
    // Manual refresh and the first poll after a switch bypass the floor.
    assert_eq!(one(cu(10.0, 10.0, 10), Fx::force), 1);
    assert_eq!(one(cu(10.0, 10.0, 10), Fx::just_switched), 1);
    // So does a crossed reset.
    let mut crossed = cu(10.0, 10.0, 10);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(5)).to_rfc3339());
    crossed.fetched_at = Utc::now().timestamp() - 60;
    assert_eq!(one(crossed, same), 1);
    reset_usage_fetch_tracker(&e);
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

fn claude_expired(email: &str) -> Account {
    let mut a = claude_account_with_identity(email, -60_000);
    a.cached_usage = Some(cu(10.0, 10.0, 2000));
    a
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

// --- mutation-hardening: the provider (Codex) usage refresh policy ---
//
// Codex's usage endpoint has no test seam, so every account here carries an
// expired token and a refresh token, and the token endpoint (which does have
// one) is a mock that refuses: a request reaching it proves the account got
// past every skip rule and no usage request is ever attempted.

fn codex_account(key: &str, cache: Option<CachedUsage>) -> crate::store::ProviderAccount {
    crate::store::ProviderAccount {
        key: key.to_string(),
        secret_blob: "{}".to_string(),
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
        expires_at: Utc::now().timestamp() - 100,
        identity_email: None,
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: cache,
        notif_state: Default::default(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
    }
}

/// Token-endpoint requests made by one `refresh_provider_usage_caches` pass
/// over Codex `accounts` (the first is active when `active_first`).
fn codex_gate(
    accounts: Vec<crate::store::ProviderAccount>,
    active: Option<&str>,
    force: bool,
) -> usize {
    let _cfg = crate::store::ScopedConfigDir::new();
    let (url, hits) = spawn_mock_usage_server(500, String::new());
    let mut st = State::default();
    for a in accounts {
        st.upsert_provider_account("codex", a);
    }
    st.provider_accounts_mut("codex").active = active.map(str::to_string);
    st.save().unwrap();
    crate::env_lock::scoped_env_var(
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        Some(&format!("{url}/oauth/token")),
        || {
            refresh_provider_usage_caches(force, 95.0);
        },
    );
    hit_count(&hits)
}

fn codex_one(key: &str, cache: CachedUsage, active: bool, force: bool) -> usize {
    codex_gate(
        vec![codex_account(key, Some(cache))],
        active.then_some(key),
        force,
    )
}

#[test]
fn codex_account_past_every_skip_rule_gets_its_token_refreshed() {
    let k = uniq("codex-basic");
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 2000), false, false), 1);
    // A token inside the refresh skew counts as due, an unexpired one beyond it
    // would not (that case needs the real endpoint, so only the edge is pinned).
    let mut soon = codex_account(&k, Some(cu(10.0, 10.0, 2000)));
    soon.expires_at = Utc::now().timestamp() + 100;
    assert_eq!(codex_gate(vec![soon], None, false), 1);
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
}

#[test]
fn codex_dead_logins_are_never_polled() {
    let k = uniq("codex-dead");
    let mut flagged = codex_account(&k, Some(cu(10.0, 10.0, 2000)));
    flagged.needs_relogin = true;
    assert_eq!(codex_gate(vec![flagged], Some(&k), true), 0);
}

#[test]
fn codex_lapsed_accounts_are_rechecked_once_per_window_or_when_forced() {
    let k = uniq("codex-lapsed");
    let lapsed = |key: &str| {
        let mut a = codex_account(key, Some(cu(10.0, 10.0, 2000)));
        a.no_subscription = true;
        a
    };
    let tracker = fetch_tracker_key("codex", &k);
    // First look is due...
    assert_eq!(codex_gate(vec![lapsed(&k)], None, false), 1);
    // ...and then it is throttled for the recheck window.
    assert_eq!(codex_gate(vec![lapsed(&k)], None, false), 0);
    // A manual refresh is always due.
    assert_eq!(codex_gate(vec![lapsed(&k)], None, true), 1);
    reset_usage_fetch_tracker(&tracker);
}

#[test]
fn codex_locked_accounts_wait_for_their_reset_unless_stale_or_forced() {
    let k = uniq("codex-locked");
    let hour = 3600;
    assert_eq!(codex_one(&k, cu_locked(5 * hour), false, false), 0);
    assert_eq!(codex_one(&k, cu_locked(13 * hour), false, false), 1);
    assert_eq!(codex_one(&k, cu_locked(5 * hour), false, true), 1);
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 5 * hour), false, false), 1);
}

#[test]
fn codex_inactive_accounts_wait_out_their_floor_and_failure_backoff() {
    let k = uniq("codex-floor");
    let tracker = fetch_tracker_key("codex", &k);
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let one = |c: CachedUsage| codex_one(&k, c, false, false);
    assert_eq!(one(cu(10.0, 10.0, floor - 300)), 0);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    assert_eq!(one(cu(10.0, 10.0, -100)), 1);
    let crossed = |age: i64| {
        let mut c = cu(10.0, 10.0, age);
        c.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
        c
    };
    assert_eq!(one(crossed(300)), 1);
    record_inactive_fetch_failure(&tracker, Utc::now().timestamp());
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 0);
    assert_eq!(one(crossed(floor + 100)), 1);
    reset_usage_fetch_tracker(&tracker);
    record_inactive_fetch_failure(&tracker, Utc::now().timestamp() - 10_000);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    reset_usage_fetch_tracker(&tracker);
}

#[test]
fn codex_active_account_follows_its_tier() {
    let k = uniq("codex-active");
    let one = |c: CachedUsage, force: bool| codex_one(&k, c, true, force);
    assert_eq!(one(cu(93.0, 10.0, 300), false), 1, "93% polls every 30s");
    assert_eq!(one(cu(10.0, 10.0, 10), false), 0);
    assert_eq!(one(cu(10.0, 10.0, 1000), false), 1);
    assert_eq!(one(cu(10.0, 10.0, -100), false), 1);
    assert_eq!(one(cu(10.0, 10.0, 10), true), 1);
    let mut crossed = cu(10.0, 10.0, 60);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(5)).to_rfc3339());
    assert_eq!(one(crossed, false), 1);
}

// --- mutation-hardening: cadence log, provider swap cycle, watch cycle ---

#[test]
fn cadence_logs_only_when_the_interval_actually_tightens() {
    let _g = crate::store::ScopedConfigDir::new();
    let count = || read_log().matches("event=cadence").count();
    let mut c = Cadence::new(180);
    let step = |c: &mut Cadence, pct: f64| {
        c.advance(
            &CycleOutcome {
                providers: vec![cycle(CLAUDE_SLUG, "a@e.com", pct, false)],
            },
            95.0,
        )
    };
    // 180s -> 30s as the account closes on the trigger: logged.
    assert_eq!(step(&mut c, 93.0), 30);
    assert_eq!(count(), 1);
    assert!(read_log().contains("event=cadence provider=claude prev=180s new=30s"));
    // Unchanged: nothing new to say.
    assert_eq!(step(&mut c, 93.0), 30);
    assert_eq!(count(), 1);
    // Relaxing back to the base interval is not a tightening.
    assert_eq!(step(&mut c, 10.0), 180);
    assert_eq!(count(), 1);
    // 180s -> 60s: tightening again.
    assert_eq!(step(&mut c, 85.0), 60);
    assert_eq!(count(), 2);
}

/// Two captured Codex accounts, `active` being the one logged in, with the
/// given cached readings (fresh enough that no poll would fetch them).
fn seed_codex_pair(dir: &std::path::Path, active: &str, readings: [(&str, CachedUsage); 2]) {
    // Capture the non-active one first so the active one is captured last.
    let order: Vec<&str> = if readings[0].0 == active {
        vec![readings[1].0, readings[0].0]
    } else {
        vec![readings[0].0, readings[1].0]
    };
    for who in order {
        let blob = codex_auth_json(
            who,
            4_000_000_000,
            &format!("at-{who}"),
            &format!("rt-{who}"),
        );
        std::fs::write(dir.join("auth.json"), blob).unwrap();
        capture_current_generic("codex").unwrap();
    }
    let mut st = State::load().unwrap();
    for (who, cache) in readings {
        st.find_provider_account_mut("codex", who)
            .unwrap()
            .cached_usage = Some(cache);
    }
    st.save().unwrap();
}

fn codex_rows() -> Vec<Row> {
    provider_rows(&State::load().unwrap(), "codex")
}

fn history_events() -> Vec<serde_json::Value> {
    std::fs::read_to_string(store::config_dir().unwrap().join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn provider_swap_cycle_moves_off_an_exhausted_account_and_records_it() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "hot@example.com",
            [
                ("hot@example.com", cu(96.0, 40.0, 10)),
                ("cool@example.com", cu(12.0, 7.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        // A stale hold on the account being moved TO is dropped by the move.
        let mut guard = SwapGuard {
            stuck_notified: true,
            manual_locked_choice: Some(("codex".to_string(), "cool@example.com".to_string())),
            ..SwapGuard::default()
        };
        let (swapped, actionable) = provider_swap_cycle(
            "codex",
            &mut rows,
            "hot@example.com",
            95.0,
            95.0,
            &mut guard,
        )
        .unwrap();
        assert_eq!(
            swapped,
            Some((
                "hot@example.com".to_string(),
                "cool@example.com".to_string()
            ))
        );
        assert!(actionable);
        assert_eq!(
            provider_active(&State::load().unwrap(), "codex").as_deref(),
            Some("cool@example.com")
        );
        // The guard remembers the move.
        assert!(guard.left_at.contains_key("hot@example.com"));
        assert!(guard.last_swap.is_some());
        assert!(!guard.stuck_notified);
        assert!(guard.manual_locked_choice.is_none());
        // And the history/log say what happened, with the target's numbers.
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .expect("swap event logged");
        assert_eq!(ev["provider"], "codex");
        assert_eq!(ev["reason"], "trigger");
        assert_eq!(ev["from"], "hot@example.com");
        assert_eq!(ev["to"], "cool@example.com");
        assert_eq!(ev["session"], 12.0);
        assert_eq!(ev["weekly"], 7.0);
        let log = read_log();
        assert!(log.contains("event=swap_decision provider=codex active=hot@example.com"));
        assert!(
            log.contains("active_pct=96% target=cool@example.com target_pct=12% action=switching")
        );
    });
}

#[test]
fn provider_swap_cycle_flip_back_is_labelled_proactive_and_not_announced_as_a_decision() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "later@example.com",
            [
                ("later@example.com", cu(40.0, 40.0, 10)),
                ("sooner@example.com", cu(5.0, 5.0, 10)),
            ],
        );
        let mut st = State::load().unwrap();
        let soon = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let later = (Utc::now() + Duration::days(5)).to_rfc3339();
        st.find_provider_account_mut("codex", "sooner@example.com")
            .unwrap()
            .cached_usage
            .as_mut()
            .unwrap()
            .weekly_reset = Some(soon);
        st.find_provider_account_mut("codex", "later@example.com")
            .unwrap()
            .cached_usage
            .as_mut()
            .unwrap()
            .weekly_reset = Some(later);
        st.save().unwrap();
        let mut rows = codex_rows();
        let mut guard = SwapGuard::default();
        let (swapped, _) = provider_swap_cycle(
            "codex",
            &mut rows,
            "later@example.com",
            95.0,
            95.0,
            &mut guard,
        )
        .unwrap();
        assert_eq!(
            swapped,
            Some((
                "later@example.com".to_string(),
                "sooner@example.com".to_string()
            ))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "proactive");
        assert!(!read_log().contains("action=switching"));
    });
}

#[test]
fn provider_swap_cycle_stays_put_and_says_why() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Both accounts exhausted: nowhere to go.
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(100.0, 100.0, 10)),
                ("b@example.com", cu(100.0, 100.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        let mut guard = SwapGuard::default();
        let (swapped, actionable) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        assert_eq!(swapped, None);
        assert!(!actionable);
        assert!(guard.stuck_notified, "the user is told once");
        assert!(read_log().contains("action=stayed reason=no_eligible_target"));
        // A manual hold on the exhausted-but-not-empty account is reported as such.
        let mut guard = SwapGuard::default();
        let mut rows = codex_rows();
        let key = "a@example.com".to_string();
        for r in rows.iter_mut().filter(|r| r.email == key) {
            r.session.pct = Some(96.0);
            r.weekly.pct = Some(96.0);
        }
        guard.manual_locked_choice = Some(("codex".to_string(), key.clone()));
        provider_swap_cycle("codex", &mut rows, &key, 95.0, 95.0, &mut guard).unwrap();
        assert!(read_log().contains("action=stayed reason=manual_hold"));
        assert!(!guard.stuck_notified, "a held account is not 'stuck'");
    });
}

#[test]
fn provider_swap_cycle_tracks_the_stuck_notice_only_while_stuck() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(50.0, 50.0, 10)),
                ("b@example.com", cu(60.0, 60.0, 10)),
            ],
        );
        let run = |guard: &mut SwapGuard, rows: &mut Vec<Row>| {
            provider_swap_cycle("codex", rows, "a@example.com", 95.0, 95.0, guard).unwrap()
        };
        // Healthy and nothing better: no longer stuck, the notice re-arms.
        let mut guard = SwapGuard {
            stuck_notified: true,
            ..SwapGuard::default()
        };
        assert_eq!(run(&mut guard, &mut codex_rows()).0, None);
        assert!(!guard.stuck_notified);
        // Same while the user holds a healthy account.
        let mut guard = SwapGuard {
            stuck_notified: true,
            manual_locked_choice: Some(("codex".to_string(), "a@example.com".to_string())),
            ..SwapGuard::default()
        };
        assert_eq!(run(&mut guard, &mut codex_rows()).0, None);
        assert!(!guard.stuck_notified);
        // An account with no usable reading is neither over the trigger nor stuck.
        let mut rows = codex_rows();
        let a = rows
            .iter_mut()
            .find(|r| r.email == "a@example.com")
            .unwrap();
        a.error = Some("boom".to_string());
        a.session.pct = Some(99.0);
        let mut guard = SwapGuard {
            stuck_notified: true,
            ..SwapGuard::default()
        };
        assert_eq!(run(&mut guard, &mut rows).0, None);
        assert!(!guard.stuck_notified);
    });
}

#[test]
fn provider_swap_cycle_reports_a_cooldown_blocked_preparation() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(0.0, 100.0, 10)),
                ("b@example.com", cu(0.0, 100.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        let now = Utc::now();
        for r in rows.iter_mut() {
            r.weekly.resets_at = Some(if r.email == "a@example.com" {
                now + Duration::days(5)
            } else {
                now + Duration::hours(2)
            });
        }
        let mut guard = SwapGuard {
            last_swap: Some(std::time::Instant::now()),
            ..SwapGuard::default()
        };
        let (swapped, actionable) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        assert_eq!(swapped, None);
        assert!(actionable, "a cooldown-blocked target is still actionable");
        assert!(read_log().contains("target=b@example.com action=blocked reason=cooldown"));
    });
}

#[test]
fn watch_cycle_polls_every_swappable_provider_and_swaps_the_exhausted_one() {
    let dir = tempfile::tempdir().unwrap();
    // Lock order everywhere: poll lock first, then the env lock.
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = with_claude_home("scratch-json@e.com", |_home| {
        // `scoped_env_var` is not reentrant and `with_claude_home` already
        // holds the env lock, so set the second variable by hand.
        struct RestoreCodexHome(Option<std::ffi::OsString>);
        impl Drop for RestoreCodexHome {
            fn drop(&mut self) {
                #[allow(clippy::disallowed_methods)]
                match self.0.take() {
                    Some(v) => std::env::set_var("CODEX_HOME", v),
                    None => std::env::remove_var("CODEX_HOME"),
                }
            }
        }
        #[allow(clippy::disallowed_methods)]
        let _restore = {
            let prev = std::env::var_os("CODEX_HOME");
            std::env::set_var("CODEX_HOME", dir.path());
            RestoreCodexHome(prev)
        };
        {
            // No Claude account is active, so no keychain is involved; its two
            // accounts still feed the history log.
            let calm = claude_account_with_identity("calm@e.com", 3_600_000);
            let mut calm = calm;
            calm.cached_usage = Some(cu(20.0, 20.0, 10));
            seed_claude_state(vec![calm], None);
            seed_codex_pair(
                dir.path(),
                "hot@example.com",
                [
                    ("hot@example.com", cu(96.0, 40.0, 10)),
                    ("cool@example.com", cu(12.0, 7.0, 10)),
                ],
            );
            // The Codex CLI rotated auth.json behind usagio's back.
            // (A lower expiry than we hold: only the active-account follower adopts that.)
            let rotated = codex_auth_json("hot@example.com", 3_900_000_000, "cli-at", "cli-rt");
            std::fs::write(dir.path().join("auth.json"), rotated).unwrap();
            let mut guards = SwapGuards::default();
            let out = watch_cycle(95.0, 95.0, &mut guards, false).unwrap();
            // The rotation was absorbed before the swap (active at decision time).
            let st = State::load().unwrap();
            assert_eq!(
                st.find_provider_account("codex", "hot@example.com")
                    .unwrap()
                    .access_token,
                "cli-at"
            );
            // History records Claude's rows only.
            let history =
                std::fs::read_to_string(store::config_dir().unwrap().join("history.jsonl"))
                    .unwrap_or_default();
            assert!(history.contains("calm@e.com"));
            assert!(!history.contains("hot@example.com") || history.contains("\"event\":\"swap\""));
            assert!(!history.contains("\"account\":\"hot@example.com\""));
            out
        }
    });
    // Claude has accounts but none active: reported, nothing to swap.
    let claude = outcome
        .providers
        .iter()
        .find(|p| p.slug == CLAUDE_SLUG)
        .unwrap();
    assert_eq!(claude.active, None);
    assert_eq!(claude.swapped, None);
    assert!(!claude.actionable);
    let codex = outcome
        .providers
        .iter()
        .find(|p| p.slug == "codex")
        .unwrap();
    assert_eq!(
        codex.swapped,
        Some((
            "hot@example.com".to_string(),
            "cool@example.com".to_string()
        ))
    );
    assert!(!codex.rate_limited);
    assert_eq!(codex.max_pct, Some(96.0));
    assert!(outcome
        .providers
        .iter()
        .all(|p| provider_supports_swap(&p.slug)));
}

// --- mutation-hardening: small formatting / path / CLI helpers ---

/// Retry `f` until the wall-clock second did not change while it ran, so a
/// boundary-exact age or countdown is measured exactly.
fn within_one_second<R>(mut f: impl FnMut(i64) -> R) -> R {
    for _ in 0..20 {
        let t0 = Utc::now().timestamp();
        let r = f(t0);
        if Utc::now().timestamp() == t0 {
            return r;
        }
    }
    panic!("wall clock never stable");
}

#[test]
fn age_str_switches_unit_exactly_at_each_boundary() {
    let at = |age: i64| within_one_second(|t0| age_str(Some(t0 - age)));
    assert_eq!(at(-100), "just now");
    assert_eq!(at(0), "0s ago");
    assert_eq!(at(59), "59s ago");
    assert_eq!(at(60), "1m ago");
    assert_eq!(at(3599), "59m ago");
    assert_eq!(at(3600), "1h ago");
    assert_eq!(at(86_399), "23h ago");
    assert_eq!(at(86_400), "1d ago");
    assert_eq!(at(3 * 86_400 + 5), "3d ago");
}

#[test]
fn humanize_until_reads_a_minute_out_as_one_minute() {
    let at = |ahead: i64| {
        within_one_second(|t0| humanize_until(DateTime::from_timestamp(t0 + ahead, 0).unwrap()))
    };
    assert_eq!(at(59), "<1m");
    assert_eq!(at(60), "1m");
}

#[test]
fn cell_resets_in_is_blank_without_a_reset_time() {
    let none = Cell {
        pct: Some(10.0),
        resets_at: None,
    };
    assert_eq!(none.resets_in(), "");
    let soon = Cell {
        pct: Some(10.0),
        resets_at: Some(Utc::now() + Duration::minutes(150) + Duration::seconds(5)),
    };
    assert_eq!(soon.resets_in(), "2h 30m");
}

#[test]
fn claude_plan_label_prefers_lapse_then_stored_plan_then_the_captured_org() {
    let mut a = claude_account("p@e.com", 3_600_000);
    assert_eq!(claude_plan_label(&a), None);
    a.oauth_account = Some(serde_json::json!({
        "organizationType": "claude_max",
        "organizationRateLimitTier": "default_claude_max_20x",
    }));
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Max 20x"));
    a.plan = Some("Pro".to_string());
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Pro"));
    a.no_subscription = true;
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Pro"));
    a.plan = None;
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Free"));
}

#[test]
fn executable_paths_resolve_to_the_running_binary_outside_a_brew_or_bundle_layout() {
    let exe = std::env::current_exe().unwrap();
    assert!(!exe.as_os_str().is_empty());
    assert_eq!(stable_exe_path(), exe);
    assert_eq!(launch_agent_exe_path(), exe);
}

#[test]
fn parse_context_args_reads_both_flags_and_rejects_the_rest() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(parse_context_args(&[]).unwrap(), (None, None));
    assert_eq!(
        parse_context_args(&args(&["--provider", "claude"])).unwrap(),
        (Some("claude".to_string()), None)
    );
    assert_eq!(
        parse_context_args(&args(&["--project", "/tmp/p", "--provider", "codex"])).unwrap(),
        (
            Some("codex".to_string()),
            Some(std::path::PathBuf::from("/tmp/p"))
        )
    );
    assert!(parse_context_args(&args(&["--provider"])).is_err());
    assert!(parse_context_args(&args(&["--project"])).is_err());
    let err = parse_context_args(&args(&["--bogus"])).unwrap_err();
    assert!(format!("{err}").contains("unknown context option: --bogus"));
}

#[test]
fn history_events_are_appended_one_json_line_each() {
    let _g = crate::store::ScopedConfigDir::new();
    assert!(history_path().unwrap().ends_with("history.jsonl"));
    log_event(&serde_json::json!({"event": "first", "n": 1}));
    log_event(&serde_json::json!({"event": "second", "n": 2}));
    let events = history_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "first");
    assert_eq!(events[1]["n"], 2);
}

#[test]
fn append_history_records_only_accounts_with_a_reading() {
    let _g = crate::store::ScopedConfigDir::new();
    let reset = Utc::now() + Duration::hours(5);
    let mut blank = row_full("blank@e.com", 1.0, 1.0, reset);
    blank.fetched_at = None;
    let rows = vec![
        row_full("active@e.com", 30.0, 40.0, reset),
        row_full("other@e.com", 5.0, 6.0, reset),
        blank,
    ];
    append_history(&rows, Some("active@e.com"));
    let events = history_events();
    assert_eq!(events.len(), 2);
    let active = events
        .iter()
        .find(|e| e["account"] == "active@e.com")
        .unwrap();
    assert_eq!(active["active"], true);
    assert_eq!(active["session"], 30.0);
    assert_eq!(active["weekly"], 40.0);
    let other = events
        .iter()
        .find(|e| e["account"] == "other@e.com")
        .unwrap();
    assert_eq!(other["active"], false);
}

#[test]
fn removing_a_provider_account_requires_it_to_exist() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    st.upsert_provider_account("codex", codex_account("gone@e.com", None));
    st.save().unwrap();
    let err = remove_provider_account_generic("codex", "nobody@e.com").unwrap_err();
    assert!(format!("{err}").contains("no codex account matches 'nobody@e.com'"));
    assert!(State::load()
        .unwrap()
        .find_provider_account("codex", "gone@e.com")
        .is_some());
    remove_provider_account_generic("codex", "gone@e.com").unwrap();
    assert!(State::load()
        .unwrap()
        .find_provider_account("codex", "gone@e.com")
        .is_none());
}

#[test]
fn capturing_the_claude_login_stores_it_as_the_active_account() {
    with_claude_home("cap@e.com", |_| {
        let blob = claude_account("cap@e.com", 3_600_000).keychain_blob;
        prime_active_slot(&blob);
        let (email, existed) = capture_current().unwrap();
        assert_eq!(email, "cap@e.com");
        assert!(!existed);
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("cap@e.com"));
        let a = st.find("cap@e.com").unwrap();
        assert_eq!(a.access_token, "at-cap@e.com");
        assert!(a.oauth_account.is_some(), "the identity travels with it");
        assert_eq!(a.user_id.as_deref(), Some("user-1"));
        // Capturing again refreshes rather than duplicating.
        let (_, existed) = capture_current().unwrap();
        assert!(existed);
        assert_eq!(State::load().unwrap().accounts.len(), 1);
        // The CLI entry point does the same for Claude with no argument.
        cmd_capture(&[]).unwrap();
        assert_eq!(State::load().unwrap().accounts.len(), 1);
        assert!(!State::load().unwrap().providers.contains_key("claude"));
    });
}

#[test]
fn the_capture_command_routes_other_providers_to_their_own_slot() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("cx@example.com", 4_000_000_000, "at", "rt");
        std::fs::write(dir.path().join("auth.json"), blob).unwrap();
        cmd_capture(&["codex".to_string()]).unwrap();
        let st = State::load().unwrap();
        let acct = st.find_provider_account("codex", "cx@example.com").unwrap();
        // Expiry is the id token's own (4e9), not some other sum.
        assert!(
            (acct.expires_at - 4_000_000_000).abs() < 30,
            "{}",
            acct.expires_at
        );
        assert!(st.accounts.is_empty());
        assert!(cmd_capture(&["nope".to_string()]).is_err());
    });
}

#[test]
fn list_with_a_refresh_flag_fetches_once_and_without_it_never_does() {
    use crate::providers::claude::usage;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let email = uniq("list");
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(200, usage_body(10.0, 10.0));
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        seed_claude_state(
            vec![account_with_cache(&email, Some(cu(10.0, 10.0, 2000)))],
            None,
        );
        cmd_list(&[]).unwrap();
        assert_eq!(hit_count(&hits), 0, "plain list reads the cache");
        cmd_list(&["--refresh".to_string()]).unwrap();
        assert_eq!(hit_count(&hits), 1);
        let st = State::load().unwrap();
        st.find(&email).unwrap();
        // The short flag is the same request, and it forces a fetch even
        // though the cache is now fresh.
        cmd_list(&["-r".to_string()]).unwrap();
        usage::set_usage_url_override(None);
        assert_eq!(hit_count(&hits), 2);
    });
    reset_usage_fetch_tracker(&email);
}

#[test]
fn token_command_prints_a_refreshed_inactive_token_and_adopts_for_the_active_one() {
    use crate::providers::claude::oauth;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_claude_home("scratch-json@e.com", |_| {
        let (base_url, hits) = spawn_mock_token_server();
        oauth::set_token_url_override(Some(&format!("{base_url}/v1/oauth/token")));
        // No accounts / several accounts without a selector are errors.
        assert!(format!("{}", cmd_token(None).unwrap_err()).contains("no accounts"));
        seed_claude_state(vec![claude_expired("solo@e.com")], None);
        // Exactly one account needs no selector, and being inactive and expired
        // it is refreshed and the rotation saved.
        cmd_token(None).unwrap();
        assert_eq!(hit_count(&hits), 1);
        assert_eq!(
            State::load()
                .unwrap()
                .find("solo@e.com")
                .unwrap()
                .access_token,
            "mock-refreshed-access-token"
        );
        let mut st = State::load().unwrap();
        st.upsert(claude_expired("duo@e.com"));
        st.save().unwrap();
        assert!(format!("{}", cmd_token(None).unwrap_err()).contains("multiple accounts"));
        // The ACTIVE account's token belongs to the vendor CLI: adopt what its
        // slot holds, never POST.
        let mut st = State::load().unwrap();
        st.active = Some("duo@e.com".to_string());
        st.save().unwrap();
        let mut slot = claude_account("duo@e.com", 3_600_000);
        slot.set_tokens("slot-at".into(), "slot-rt".into(), 9_999_999_999_999);
        prime_active_slot(&slot.keychain_blob);
        cmd_token(Some("duo")).unwrap();
        assert_eq!(hit_count(&hits), 1, "no POST for the active account");
        assert_eq!(
            State::load()
                .unwrap()
                .find("duo@e.com")
                .unwrap()
                .access_token,
            "slot-at"
        );
        oauth::set_token_url_override(None);
    });
}

// --- mutation-hardening: process-level behaviour and CLI output ---

/// Re-run this test binary on `probe`, a test that does nothing unless the
/// parent set `USAGIO_PROBE_CLI`, and return what it printed. Used for code
/// whose observable result is text on stdout.
fn run_cli_probe(which: &str, envs: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--exact", "tests::cli_output_probe", "--nocapture"])
        .env("USAGIO_PROBE_CLI", which);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "probe {which} failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("1 passed"),
        "probe {which} did not run:\n{text}"
    );
    text
}

/// The lines a probe printed between its `BEGIN` and `END` markers.
fn probe_lines(text: &str) -> Vec<String> {
    let begin = text.find("PROBE-BEGIN\n").expect("begin marker") + "PROBE-BEGIN\n".len();
    let end = text.find("PROBE-END").expect("end marker");
    text[begin..end].lines().map(str::to_string).collect()
}

fn history_line(ts: i64, account: &str, active: bool, session: f64, weekly: f64) -> String {
    serde_json::json!({
        "ts": ts, "account": account, "active": active,
        "session": session, "weekly": weekly,
    })
    .to_string()
}

/// Local weekday-from-Monday and hour for a timestamp, as `cmd_report` buckets.
fn local_bucket(ts: i64) -> (usize, usize) {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let dt = Local.timestamp_opt(ts, 0).single().unwrap();
    (
        dt.weekday().num_days_from_monday() as usize,
        dt.hour() as usize,
    )
}

/// A spread of timestamps (three days apart, odd hours) so weekday and hour
/// buckets differ between samples.
fn report_timestamps() -> Vec<i64> {
    (0..6)
        .map(|i| 1_700_000_000 + i * (3 * 86_400 + 5 * 3600 + 120))
        .collect()
}

/// Child half: runs one CLI renderer against fixtures and prints between markers.
#[test]
fn cli_output_probe() {
    let Ok(which) = std::env::var("USAGIO_PROBE_CLI") else {
        return;
    };
    let _cfg = crate::store::ScopedConfigDir::new();
    let reset = Utc::now() + Duration::hours(3) + Duration::seconds(30);
    println!("PROBE-BEGIN");
    match which.as_str() {
        "table" | "table-no-active" => {
            let mut live = row_full("live@e.com", 40.0, 80.0, reset);
            live.session.resets_at = Some(reset);
            live.opus = Some(Cell {
                pct: Some(25.0),
                resets_at: Some(reset),
            });
            let mut blank = row_full("blank@e.com", 0.0, 0.0, reset);
            blank.fetched_at = None;
            let mut lapsed = row_full("lapsed@e.com", 0.0, 0.0, reset);
            lapsed.no_subscription = true;
            let active = (which == "table").then_some("live@e.com");
            render_table(&[live, blank, lapsed], active);
        }
        "table-plain" => {
            render_table(
                &[row_full("solo@e.com", 10.0, 20.0, reset)],
                Some("solo@e.com"),
            );
        }
        "bars" => {
            let labels: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
            print_bars(&labels, &[10.0, 5.0, 0.0]);
            print_bars(&labels, &[0.0, 0.0, 0.0]);
        }
        "report-swaps" | "report-low" | "report-edge" => {
            let ts = report_timestamps();
            let (a_weekly, swap) = match which.as_str() {
                "report-swaps" => (91.0, true),
                "report-low" => (40.0, false),
                _ => (80.0, false),
            };
            let mut lines = Vec::new();
            for (i, t) in ts.iter().enumerate() {
                // Active account A climbs 4 points per sample.
                lines.push(history_line(*t, "a@e.com", true, 4.0 * i as f64, a_weekly));
                // B is never active: its jumps must not count as consumption.
                lines.push(history_line(
                    *t + 1,
                    "b@e.com",
                    false,
                    90.0 * (i % 2) as f64,
                    30.0,
                ));
            }
            // A drop in the active series (a reset) adds nothing.
            lines.push(history_line(ts[5] + 60, "a@e.com", true, 1.0, a_weekly));
            if swap {
                lines.push(
                    serde_json::json!({"ts": ts[2] + 5, "event": "swap", "active": true,
                        "account": "a@e.com", "session": 99.0})
                    .to_string(),
                );
            }
            std::fs::write(history_path().unwrap(), lines.join("\n") + "\n").unwrap();
            cmd_report(&[]).unwrap();
        }
        "report-empty" => {
            std::fs::write(history_path().unwrap(), "not json\n").unwrap();
            cmd_report(&[]).unwrap();
        }
        "pace-none" => {
            cmd_report(&["--pace".to_string()]).unwrap();
        }
        "pace" => {
            let mut a = claude_account("pace@e.com", 3_600_000);
            a.email = Some("pace@e.com".to_string());
            seed_claude_state(vec![a], None);
            seed_history("pace@e.com", 10, |age_h, _| {
                (60.0 - 10.0 * age_h, 80.0 - 4.0 * age_h)
            });
            cmd_report(&["--pace".to_string()]).unwrap();
        }
        "list" => {
            seed_claude_state(
                vec![account_with_cache("cl@e.com", Some(cu(10.0, 10.0, 60)))],
                None,
            );
            let mut st = State::load().unwrap();
            st.upsert_provider_account(
                "codex",
                codex_account("cx@e.com", Some(cu(20.0, 20.0, 60))),
            );
            st.save().unwrap();
            cmd_list(&[]).unwrap();
        }
        "list-empty" => {
            cmd_list(&[]).unwrap();
        }
        "context" => {
            let dir = tempfile::tempdir().unwrap();
            cmd_context(&[
                "--provider".to_string(),
                "claude".to_string(),
                "--project".to_string(),
                dir.path().to_string_lossy().to_string(),
            ])
            .unwrap();
        }
        other => panic!("unknown probe {other}"),
    }
    println!("PROBE-END");
}

#[test]
fn table_marks_the_active_account_and_explains_rows_without_usage() {
    let text = run_cli_probe("table", &[]);
    let lines = probe_lines(&text);
    let row = |prefix: &str| {
        lines
            .iter()
            .find(|l| l.contains(prefix))
            .unwrap_or_else(|| panic!("no row for {prefix}:\n{}", lines.join("\n")))
            .clone()
    };
    let live = row("live@e.com");
    assert!(live.starts_with("▶  live@e.com"), "{live}");
    assert!(live.contains("40%") && live.contains("80%") && live.contains("25%"));
    assert!(live.contains("3h"), "reset countdown shown: {live}");
    let blank = row("blank@e.com");
    assert!(blank.starts_with("   blank@e.com"), "{blank}");
    assert!(blank.contains("no data yet"));
    let lapsed = row("lapsed@e.com");
    assert!(lapsed.starts_with("   lapsed@e.com"), "{lapsed}");
    assert!(lapsed.contains("no subscription (Free plan)"));
    assert!(lapsed.contains("re-checked every 30m"));
    // The Opus column only exists when some row has Opus usage.
    assert!(lines.iter().any(|l| l.contains("WEEKLY OPUS")));
    assert!(!lines
        .iter()
        .any(|l| l.contains("no active account tracked")));
    assert!(lines.iter().any(|l| l.contains("usagio list --refresh")));
}

#[test]
fn table_without_an_active_account_says_to_capture_and_omits_the_opus_column_when_unused() {
    let text = run_cli_probe("table-no-active", &[]);
    let lines = probe_lines(&text);
    assert!(lines
        .iter()
        .any(|l| l.contains("no active account tracked yet")));
    assert!(lines.iter().all(|l| !l.starts_with('▶')));
    let plain = probe_lines(&run_cli_probe("table-plain", &[]));
    assert!(!plain.iter().any(|l| l.contains("WEEKLY OPUS")));
    assert!(plain.iter().any(|l| l.starts_with("▶  solo@e.com")));
}

#[test]
fn bars_scale_to_the_largest_value() {
    let lines = probe_lines(&run_cli_probe("bars", &[]));
    let full = "█".repeat(30);
    let half = format!("{}{}", "█".repeat(15), " ".repeat(15));
    assert_eq!(lines[0], format!("  A    |{full}|  10.0"));
    assert_eq!(lines[1], format!("  B    |{half}|   5.0"));
    assert_eq!(lines[2], format!("  C    |{}|   0.0", " ".repeat(30)));
    // All-zero input never divides by zero and draws empty bars.
    assert_eq!(lines[3], format!("  A    |{}|   0.0", " ".repeat(30)));
}

/// `(label, value)` for the bar rows printed under `heading`.
fn bar_rows(lines: &[String], heading: &str, count: usize) -> Vec<(String, f64)> {
    let start = lines
        .iter()
        .position(|l| l.starts_with(heading))
        .expect(heading)
        + 1;
    lines[start..start + count]
        .iter()
        .map(|l| {
            let label = l.split_whitespace().next().unwrap().to_string();
            let value = l.rsplit('|').next().unwrap().trim().parse().unwrap();
            (label, value)
        })
        .collect()
}

#[test]
fn report_buckets_active_consumption_by_weekday_and_hour() {
    let lines = probe_lines(&run_cli_probe("report-swaps", &[]));
    let ts = report_timestamps();
    let mut by_weekday = [0.0_f64; 7];
    let mut by_hour = [0.0_f64; 24];
    for t in ts.iter().skip(1) {
        let (d, h) = local_bucket(*t);
        by_weekday[d] += 4.0;
        by_hour[h] += 4.0;
    }
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let got = bar_rows(&lines, "Consumption by weekday", 7);
    for (i, (label, v)) in got.iter().enumerate() {
        assert_eq!(label, days[i]);
        assert!(
            (v - by_weekday[i]).abs() < 0.06,
            "{label}: {v} vs {}",
            by_weekday[i]
        );
    }
    let got = bar_rows(&lines, "Consumption by hour", 24);
    for (h, (label, v)) in got.iter().enumerate() {
        assert_eq!(label, &format!("{h:02}"));
        assert!(
            (v - by_hour[h]).abs() < 0.06,
            "{label}: {v} vs {}",
            by_hour[h]
        );
    }
}

#[test]
fn report_summarises_samples_swaps_peaks_and_the_verdict() {
    let lines = probe_lines(&run_cli_probe("report-swaps", &[]));
    let text = lines.join("\n");
    // 6 active + 6 inactive + 1 reset sample + 1 swap event.
    assert!(text.contains("samples: 14   swaps: 1"), "{text}");
    assert!(text.contains("period:"));
    let peak = |who: &str| {
        lines
            .iter()
            .find(|l| l.trim_start().starts_with(who))
            .cloned()
            .unwrap()
    };
    assert!(peak("a@e.com").contains("91%"), "{}", peak("a@e.com"));
    assert!(peak("b@e.com").contains("30%"));
    assert!(text.contains("You hit 1 swap(s)"));
    assert!(!text.contains("never needed a swap"));

    let low = probe_lines(&run_cli_probe("report-low", &[])).join("\n");
    assert!(low.contains("swaps: 0"));
    assert!(
        low.contains("One account peaked at only 40% weekly"),
        "{low}"
    );

    // Exactly 80% is no longer "only": with no swaps it was close to enough.
    let edge = probe_lines(&run_cli_probe("report-edge", &[])).join("\n");
    assert!(edge.contains("never needed a swap"), "{edge}");
    assert!(!edge.contains("peaked at only"));
}

#[test]
fn report_without_usable_history_says_so() {
    assert!(run_cli_probe("report-empty", &[]).contains("No usage samples recorded yet."));
}

#[test]
fn pace_report_lists_each_account_with_its_forecast() {
    assert!(run_cli_probe("pace-none", &[]).contains("No accounts captured yet"));
    let text = probe_lines(&run_cli_probe("pace", &[])).join("\n");
    assert!(text.contains("Burn-rate forecast"), "{text}");
    assert!(text.contains("pace@e.com"));
    assert!(
        text.contains("Session") || text.contains("session"),
        "{text}"
    );
    assert!(text.contains("Weekly") || text.contains("weekly"), "{text}");
}

#[test]
fn list_prints_each_providers_accounts_under_its_own_heading() {
    let text = probe_lines(&run_cli_probe("list", &[])).join("\n");
    assert!(text.contains("cl@e.com"));
    assert!(text.contains("\nCodex:\n"), "{text}");
    assert!(text.contains("cx@e.com"));
    let empty = probe_lines(&run_cli_probe("list-empty", &[])).join("\n");
    assert!(empty.contains("No accounts yet."));
}

#[test]
fn context_command_prints_a_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let text = run_cli_probe("context", &[("HOME", tmp.path().to_str().unwrap())]);
    assert!(text.to_lowercase().contains("context"), "{text}");
}

#[test]
fn keychain_account_is_the_login_name_with_a_fixed_fallback() {
    let exe = std::env::current_exe().unwrap();
    let probe = |user: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--exact",
            "tests::keychain_account_child_probe",
            "--nocapture",
        ])
        .env_remove("USER");
        if let Some(u) = user {
            cmd.env("USER", u);
        }
        let out = cmd.output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert!(probe(Some("alice-test")).contains("ACCOUNT=alice-test;"));
    assert!(probe(None).contains("ACCOUNT=claude;"));
}

#[test]
fn keychain_account_child_probe() {
    println!("ACCOUNT={};", keychain_account());
}

#[cfg(unix)]
#[test]
fn raising_the_open_file_limit_reaches_the_hard_cap_or_logs_why_not() {
    fn limits() -> libc::rlimit {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid resource id and a writable rlimit.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) }, 0);
        rl
    }
    let _g = crate::store::ScopedConfigDir::new();
    let before = limits();
    let target = before.rlim_max.min(65536);
    // Start from a low soft cap so there is something to raise.
    let low = target.min(512);
    if low >= target || before.rlim_cur != low {
        let lowered = libc::rlimit {
            rlim_cur: low,
            rlim_max: before.rlim_max,
        };
        // SAFETY: valid resource id and rlimit struct.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) } != 0 {
            return;
        }
    }
    let lowered_to = limits().rlim_cur;
    raise_nofile_limit();
    let after = limits().rlim_cur;
    // Leave the process as we found it for the tests running beside this one.
    let restore = libc::rlimit {
        rlim_cur: before.rlim_cur.max(after),
        rlim_max: before.rlim_max,
    };
    // SAFETY: valid resource id and rlimit struct.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &restore) };
    let log = read_log();
    assert!(!log.contains("getrlimit(NOFILE) failed"), "{log}");
    if lowered_to >= target {
        return; // nothing to raise on this host
    }
    if after > lowered_to {
        assert!(
            !log.contains("setrlimit(NOFILE"),
            "raised, yet logged a failure: {log}"
        );
        assert_eq!(after, target);
    } else {
        // The OS refused (macOS caps well below 65536); the attempt is logged.
        assert!(
            log.contains("setrlimit(NOFILE"),
            "no raise and no attempt: {log}"
        );
    }
}

#[test]
fn legacy_launchd_plist_is_removed_when_present_and_ignored_otherwise() {
    let scd = crate::store::ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let agents = home.join("Library").join("LaunchAgents");
        let plist = agents.join(format!("{LEGACY_AUTOSTART_LABEL}.plist"));
        // Absent: nothing happens (and nothing is created).
        migrate_launchd_if_needed();
        assert!(!agents.exists());
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(&plist, "not a real plist").unwrap();
        let other = agents.join("com.example.keep.plist");
        std::fs::write(&other, "keep").unwrap();
        migrate_launchd_if_needed();
        assert!(!plist.exists(), "the pre-0.4.0 agent plist must be removed");
        assert!(other.exists(), "unrelated agents are untouched");
    });
}

#[test]
fn render_shot_writes_a_png_for_a_theme_and_needs_an_output_path() {
    let _g = crate::store::ScopedConfigDir::new();
    assert!(cmd_render_shot(&["system".to_string()]).is_err());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("shot.png");
    cmd_render_shot(&[
        "macos".to_string(),
        out.to_string_lossy().to_string(),
        "1.0".to_string(),
    ])
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "round-trips the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn secrets_selftest_round_trips_through_the_secret_store() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    cmd_secrets_selftest(&args(&["usagio-selftest-svc", "acct", "s3cret"])).unwrap();
    assert!(cmd_secrets_selftest(&args(&["only", "two"])).is_err());
    assert!(cmd_secrets_selftest(&[]).is_err());
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn keychain_read_returns_what_keychain_write_stored() {
    with_claude_home("kc@e.com", |_| {
        keychain_write("blob-one").unwrap();
        assert_eq!(keychain_read().as_deref(), Some("blob-one"));
        keychain_write("blob-two").unwrap();
        assert_eq!(keychain_read().as_deref(), Some("blob-two"));
    });
}

// --- mutation-hardening: remaining switch / adopt paths ---

#[test]
fn active_refresh_reports_whether_the_vendor_cli_rotated_since_the_last_cycle() {
    let _g = crate::store::ScopedConfigDir::new();
    let same = mock_claude_blob("same-at", "same-rt", 1);
    let provider = MockActiveSlotProvider::new(&same);
    let mut acct = Account::from_keychain_blob(&same).unwrap();
    acct.email = Some("log@example.com".into());
    active_refresh_cas(&provider, &mut acct);
    assert!(read_log().contains("event=active_refresh_in_sync account=log@example.com"));
    assert!(!read_log().contains("event=active_refresh_adopted"));
    provider.rotate_externally(&mock_claude_blob("new-at", "new-rt", 2));
    active_refresh_cas(&provider, &mut acct);
    assert!(read_log().contains("event=active_refresh_adopted account=log@example.com"));
    assert_eq!(acct.access_token, "new-at");
}

#[test]
fn adopting_a_codex_rotation_records_the_new_expiry() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let stale = codex_auth_json("exp@example.com", 1, "old-at", "old-rt");
        std::fs::write(dir.path().join("auth.json"), &stale).unwrap();
        capture_current_generic("codex").unwrap();
        let rotated = codex_auth_json("exp@example.com", 4_000_000_000, "new-at", "new-rt");
        std::fs::write(dir.path().join("auth.json"), &rotated).unwrap();
        refresh_provider_active_account("codex");
        let st = State::load().unwrap();
        let a = st
            .find_provider_account("codex", "exp@example.com")
            .unwrap();
        assert_eq!(a.access_token, "new-at");
        assert!(
            (a.expires_at - 4_000_000_000).abs() < 30,
            "{}",
            a.expires_at
        );
    });
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn compare_and_set_switch_for_claude_commits_only_while_the_expected_account_is_active() {
    with_claude_home("a@e.com", |_| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
                claude_account_with_identity("c@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        // The decision was taken while c was active; it no longer is.
        let lost = switch_if_still_active(CLAUDE_SLUG, "b@e.com", "c@e.com").unwrap();
        assert_eq!(lost, None);
        assert_eq!(State::load().unwrap().active.as_deref(), Some("a@e.com"));
        let won = switch_if_still_active(CLAUDE_SLUG, "b@e.com", "a@e.com").unwrap();
        assert_eq!(won.as_deref(), Some("b@e.com"));
        assert_eq!(State::load().unwrap().active.as_deref(), Some("b@e.com"));
        let direct =
            switch_to_if_still_active(providers::get(CLAUDE_SLUG).unwrap(), "c@e.com", "b@e.com")
                .unwrap();
        assert_eq!(direct.as_deref(), Some("c@e.com"));
    });
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn switch_command_activates_the_pick_and_holds_it_only_when_over_the_trigger() {
    with_claude_home("a@e.com", |_| {
        let mut over = claude_account_with_identity("over@e.com", 3_600_000);
        over.cached_usage = Some(usage_at(99.0));
        let mut calm = claude_account_with_identity("calm@e.com", 3_600_000);
        calm.cached_usage = Some(usage_at(10.0));
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                over,
                calm,
            ],
            Some("a@e.com"),
        );
        cmd_switch(Some("over"), None).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("over@e.com"));
        assert_eq!(st.manual_hold(CLAUDE_SLUG), Some("over@e.com"));
        cmd_switch(Some("calm"), None).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("calm@e.com"));
        assert_eq!(st.manual_hold(CLAUDE_SLUG), None);
        // With no selector the best account is auto-picked and not held.
        cmd_switch(None, None).unwrap();
        assert_eq!(State::load().unwrap().manual_hold(CLAUDE_SLUG), None);
    });
}

#[test]
fn provider_swap_cycle_labels_urgent_and_prepared_moves_as_non_proactive() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // A 429 near the trigger makes the move urgent although the reading
        // itself is still under it.
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(93.0, 40.0, 10)),
                ("b@example.com", cu(10.0, 10.0, 10)),
            ],
        );
        let key = fetch_tracker_key("codex", "a@example.com");
        record_usage_fetch_429(&key);
        let mut rows = codex_rows();
        let mut guard = SwapGuard::default();
        let (swapped, _) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        reset_usage_fetch_tracker(&key);
        assert_eq!(
            swapped,
            Some(("a@example.com".to_string(), "b@example.com".to_string()))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "trigger");
        assert!(read_log().contains("action=switching"));
    });
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Every account blocked: the move prepares the one that recovers first.
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(0.0, 100.0, 10)),
                ("b@example.com", cu(0.0, 100.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        let now = Utc::now();
        for r in rows.iter_mut() {
            r.weekly.resets_at = Some(if r.email == "a@example.com" {
                now + Duration::days(5)
            } else {
                now + Duration::hours(2)
            });
        }
        let mut guard = SwapGuard::default();
        let (swapped, _) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        assert_eq!(
            swapped,
            Some(("a@example.com".to_string(), "b@example.com".to_string()))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "prepare_reset");
        assert!(read_log().contains("action=switching"));
    });
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn watch_cycle_reports_the_claude_429_and_swaps_claude_accounts() {
    use crate::providers::claude::usage;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(429, String::new());
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        let a = uniq("wc-active");
        let b = uniq("wc-target");
        // Throttle the lapse probe so the 429 never reaches the profile endpoint.
        for k in [&a, &b] {
            claim_subscription_check(k, Utc::now().timestamp(), false);
        }
        let active = account_with_cache(&a, Some(cu(93.0, 10.0, 1000)));
        prime_active_slot(&active.keychain_blob);
        seed_claude_state(
            vec![active, account_with_cache(&b, Some(cu(10.0, 10.0, 10)))],
            Some(&a),
        );
        let out = watch_cycle(95.0, 95.0, &mut SwapGuards::default(), false).unwrap();
        usage::set_usage_url_override(None);
        assert_eq!(hit_count(&hits), 1);
        // The 429 on the active account counts as being at the trigger: moved.
        assert_eq!(State::load().unwrap().active.as_deref(), Some(b.as_str()));
        reset_usage_fetch_tracker(&a);
        reset_usage_fetch_tracker(&b);
        out
    });
    let claude = outcome
        .providers
        .iter()
        .find(|p| p.slug == CLAUDE_SLUG)
        .unwrap();
    assert!(claude.rate_limited);
    assert!(claude.swapped.is_some());
    let codex_limited = outcome
        .providers
        .iter()
        .any(|p| p.slug != CLAUDE_SLUG && p.rate_limited);
    assert!(!codex_limited);
}

// --- mutation-hardening: exact-second boundaries of the refresh policy ---

/// Run `f` with the current wall-clock second until the second did not roll
/// over while it ran, so a boundary-exact timestamp stays exact.
fn at_stable_second<R>(mut f: impl FnMut(i64) -> R) -> R {
    for _ in 0..25 {
        let t0 = Utc::now().timestamp();
        let r = f(t0);
        if Utc::now().timestamp() == t0 {
            return r;
        }
    }
    panic!("wall clock never stable");
}

fn cu_at(session: f64, weekly: f64, fetched_at: i64) -> CachedUsage {
    let mut c = cu(session, weekly, 0);
    c.fetched_at = fetched_at;
    c
}

#[test]
fn claude_inactive_boundaries_are_exact() {
    let e = uniq("boundary");
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let run = |cache: &dyn Fn(i64) -> CachedUsage, prep: &dyn Fn(i64)| {
        at_stable_second(|t0| {
            reset_usage_fetch_tracker(&e);
            prep(t0);
            fetches(Fx::new(vec![account_with_cache(&e, Some(cache(t0)))], None))
        })
    };
    let nothing = |_: i64| {};
    // A reading exactly one floor old is due again.
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - floor), &nothing), 1);
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - floor + 1), &nothing), 0);
    // A backoff that ends exactly now is over.
    let until_now = |t0: i64| {
        record_inactive_fetch_failure(&e, t0 - inactive_fetch_backoff_secs(1) as i64);
    };
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - 2000), &until_now), 1);
    // A locked reading exactly 12h old is still trusted; one second older is not.
    let locked = |age: i64| {
        move |t: i64| {
            let mut c = cu_at(100.0, 10.0, t - age);
            c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
            c
        }
    };
    assert_eq!(run(&locked(MAX_LOCKED_STALENESS_SECS), &nothing), 0);
    assert_eq!(run(&locked(MAX_LOCKED_STALENESS_SECS + 1), &nothing), 1);
    reset_usage_fetch_tracker(&e);
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn claude_active_tier_boundary_is_exact() {
    let e = uniq("boundary-active");
    let base = WATCH_INTERVAL_SECS as i64;
    let run = |age: i64| {
        at_stable_second(|t0| {
            fetches(Fx::new(
                vec![account_with_cache(&e, Some(cu_at(10.0, 10.0, t0 - age)))],
                Some(&e),
            ))
        })
    };
    assert_eq!(run(base), 1, "due exactly one interval after the reading");
    assert_eq!(run(base - 1), 0);
    reset_usage_fetch_tracker(&e);
}

#[test]
fn codex_boundaries_are_exact() {
    let k = uniq("codex-boundary");
    let tracker = fetch_tracker_key("codex", &k);
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let inactive = |cache: &dyn Fn(i64) -> CachedUsage, prep: &dyn Fn(i64)| {
        at_stable_second(|t0| {
            reset_usage_fetch_tracker(&tracker);
            prep(t0);
            codex_one(&k, cache(t0), false, false)
        })
    };
    let nothing = |_: i64| {};
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - floor), &nothing), 1);
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - floor + 1), &nothing), 0);
    let until_now = |t0: i64| {
        record_inactive_fetch_failure(&tracker, t0 - inactive_fetch_backoff_secs(1) as i64);
    };
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - 2000), &until_now), 1);
    let locked = |age: i64| {
        move |t: i64| {
            let mut c = cu_at(100.0, 10.0, t - age);
            c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
            c
        }
    };
    assert_eq!(inactive(&locked(MAX_LOCKED_STALENESS_SECS), &nothing), 0);
    assert_eq!(
        inactive(&locked(MAX_LOCKED_STALENESS_SECS + 1), &nothing),
        1
    );
    // The active account is due exactly one interval after its reading.
    let base = WATCH_INTERVAL_SECS as i64;
    let active =
        |age: i64| at_stable_second(|t0| codex_one(&k, cu_at(10.0, 10.0, t0 - age), true, false));
    assert_eq!(active(base), 1);
    assert_eq!(active(base - 1), 0);
    reset_usage_fetch_tracker(&tracker);
}
