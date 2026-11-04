use super::*;
use crate::providers::claude::usage;
use crate::providers::trait_def::UsageSnapshot;
use crate::providers::CLAUDE_SLUG;
use crate::store::{Account, CachedUsage, ProviderAccount};
use crate::test_support::{claude_account, row};
use chrono::{Duration, Utc};

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

// --- needs_relogin filtering in the swap picker ---

#[test]
fn row_from_account_propagates_needs_relogin_flag() {
    let blob = r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#;
    let mut a = Account::from_keychain_blob(blob).unwrap();
    a.email = Some("x@e.com".into());
    assert!(!row_from_account(&a).needs_relogin);
    a.needs_relogin = true;
    assert!(row_from_account(&a).needs_relogin);
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
fn claude_plan_label_prefers_lapse_and_explicit_name_over_tier_fallback() {
    let mut a = claude_account("p@e.com", 3_600_000);
    assert_eq!(claude_plan_label(&a), None);
    a.oauth_account = Some(serde_json::json!({
        "organizationType": "claude_max",
        "organizationRateLimitTier": "default_claude_max_20x",
    }));
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Max 20x"));
    a.oauth_account.as_mut().unwrap()["planDisplayName"] = "Ultra".into();
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Ultra"));
    a.plan = Some("Max 20x".to_string());
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Ultra"));
    a.plan = Some("Pro".to_string());
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Pro"));
    a.no_subscription = true;
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Pro"));
    a.plan = None;
    assert_eq!(claude_plan_label(&a).as_deref(), Some("Free"));
}

#[test]
fn prepaid_balance_is_fetched_at_most_once_per_refresh_window() {
    use crate::providers::trait_def::{Credits, Money};
    let mut acct = claude_account("prepaid-throttle@e.com", 3_600_000);
    acct.oauth_account = Some(serde_json::json!({ "organizationUuid": "org-1" }));
    let snapshot = || {
        let mut cu = CachedUsage::default();
        cu.reported.credits = Some(Credits {
            enabled: true,
            ..Credits::default()
        });
        cu
    };
    let fetches = std::cell::Cell::new(0);
    let fetch = |_: &str, _: &str| {
        fetches.set(fetches.get() + 1);
        Money::new(1250, 2, "USD")
    };
    let t0 = 1_000_000;
    let mut cu = snapshot();
    attach_prepaid_balance_with(&mut cu, &acct, t0, fetch);
    assert_eq!(
        cu.reported.credits.as_ref().unwrap().balance,
        Money::new(1250, 2, "USD")
    );
    // The next polls inside the window reuse the reading: no request.
    for dt in [30, 60, PREPAID_REFRESH_SECS - 1] {
        let mut cu = snapshot();
        attach_prepaid_balance_with(&mut cu, &acct, t0 + dt, fetch);
        assert_eq!(
            cu.reported.credits.as_ref().unwrap().balance,
            Money::new(1250, 2, "USD")
        );
    }
    assert_eq!(fetches.get(), 1);
    // Once the window passes, it is asked again.
    let mut cu = snapshot();
    attach_prepaid_balance_with(&mut cu, &acct, t0 + PREPAID_REFRESH_SECS, fetch);
    assert_eq!(fetches.get(), 2);
    // Credits off: never fetched.
    let mut off = CachedUsage::default();
    off.reported.credits = Some(Credits::default());
    attach_prepaid_balance_with(&mut off, &acct, t0 + 10 * PREPAID_REFRESH_SECS, fetch);
    assert_eq!(fetches.get(), 2);
}

#[test]
fn a_failed_prepaid_fetch_keeps_the_previous_balance_and_is_not_retried_every_poll() {
    use crate::providers::trait_def::{Credits, Money};
    let mut acct = claude_account("prepaid-fail@e.com", 3_600_000);
    acct.oauth_account = Some(serde_json::json!({ "organizationUuid": "org-2" }));
    let mut prev = CachedUsage::default();
    prev.reported.credits = Some(Credits {
        enabled: true,
        balance: Money::new(500, 2, "USD"),
        ..Credits::default()
    });
    acct.cached_usage = Some(prev);
    let fetches = std::cell::Cell::new(0);
    let fetch = |_: &str, _: &str| {
        fetches.set(fetches.get() + 1);
        None
    };
    for dt in [0, 30] {
        let mut cu = CachedUsage::default();
        cu.reported.credits = Some(Credits {
            enabled: true,
            ..Credits::default()
        });
        attach_prepaid_balance_with(&mut cu, &acct, 2_000_000 + dt, fetch);
        assert_eq!(
            cu.reported.credits.unwrap().balance,
            Money::new(500, 2, "USD")
        );
    }
    assert_eq!(fetches.get(), 1);
}
