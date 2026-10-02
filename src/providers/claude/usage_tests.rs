use super::*;

fn sample_profile() -> serde_json::Value {
    serde_json::json!({
        "account": {
            "uuid": "acc-uuid",
            "email": "dev@example.com",
            "display_name": "Dev",
            "full_name": "Dev Full",
            "created_at": "2026-01-01T00:00:00Z"
        },
        "organization": {
            "uuid": "org-uuid",
            "name": "Dev's Org",
            "organization_type": "claude_max",
            "rate_limit_tier": "default_claude_max_20x",
            "billing_type": "stripe_subscription",
            "has_extra_usage_enabled": false,
            "subscription_created_at": "2026-01-02T00:00:00Z"
        }
    })
}

#[test]
fn oauth_account_from_profile_maps_fields() {
    let o = oauth_account_from_profile(&sample_profile()).unwrap();
    assert_eq!(o["accountUuid"], "acc-uuid");
    assert_eq!(o["emailAddress"], "dev@example.com");
    assert_eq!(o["displayName"], "Dev");
    assert_eq!(o["fullName"], "Dev Full");
    assert_eq!(o["accountCreatedAt"], "2026-01-01T00:00:00Z");
    assert_eq!(o["organizationUuid"], "org-uuid");
    assert_eq!(o["organizationName"], "Dev's Org");
    assert_eq!(o["organizationType"], "claude_max");
    assert_eq!(o["organizationRateLimitTier"], "default_claude_max_20x");
    assert_eq!(o["billingType"], "stripe_subscription");
    assert_eq!(o["hasExtraUsageEnabled"], false);
    assert_eq!(o["subscriptionCreatedAt"], "2026-01-02T00:00:00Z");
}

#[test]
fn oauth_account_from_profile_none_without_account() {
    let p = serde_json::json!({ "organization": { "uuid": "x" } });
    assert!(oauth_account_from_profile(&p).is_none());
}

#[test]
fn oauth_account_from_profile_missing_org_fields_are_null() {
    let p = serde_json::json!({ "account": { "uuid": "a", "email": "e@x.com" } });
    let o = oauth_account_from_profile(&p).unwrap();
    assert_eq!(o["accountUuid"], "a");
    assert!(o["organizationUuid"].is_null());
}

#[test]
fn usage_deserializes_windows() {
    let json = serde_json::json!({
        "five_hour": { "utilization": 9.0, "resets_at": "2026-09-05T08:00:00Z" },
        "seven_day": { "utilization": 61.5, "resets_at": "2026-09-09T04:00:00Z" },
        "seven_day_opus": null
    })
    .to_string();
    let u: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(u.five_hour.as_ref().unwrap().utilization, Some(9.0));
    assert_eq!(
        u.five_hour.as_ref().unwrap().resets_at.as_deref(),
        Some("2026-09-05T08:00:00Z")
    );
    assert_eq!(u.seven_day.as_ref().unwrap().utilization, Some(61.5));
    assert!(u.seven_day_opus.is_none());
}

#[test]
fn usage_window_defaults_when_fields_absent() {
    let json = serde_json::json!({
        "five_hour": {},
        "seven_day": {},
        "seven_day_opus": null
    })
    .to_string();
    let u: Usage = serde_json::from_str(&json).unwrap();
    assert!(u.five_hour.as_ref().unwrap().utilization.is_none());
    assert!(u.five_hour.as_ref().unwrap().resets_at.is_none());
}

#[test]
fn subscription_active_for_paid_org() {
    assert_eq!(
        subscription_active_from_profile(&sample_profile()),
        Some(true)
    );
}

#[test]
fn subscription_lapsed_when_org_dropped_to_free() {
    // The shape a Max account's profile took after its plan expired.
    let p = serde_json::json!({
        "account": { "email": "dev@example.com", "has_claude_max": false, "has_claude_pro": false },
        "organization": {
            "organization_type": "claude_free",
            "billing_type": "none",
            "subscription_status": "canceled"
        }
    });
    assert_eq!(subscription_active_from_profile(&p), Some(false));
}

#[test]
fn subscription_lapsed_on_terminal_status_alone() {
    let mut p = sample_profile();
    p["organization"]["subscription_status"] = "unpaid".into();
    assert_eq!(subscription_active_from_profile(&p), Some(false));
}

#[test]
fn subscription_active_while_status_active() {
    let mut p = sample_profile();
    p["organization"]["subscription_status"] = "active".into();
    assert_eq!(subscription_active_from_profile(&p), Some(true));
}

#[test]
fn subscription_unknown_without_organization() {
    let p = serde_json::json!({ "account": { "email": "e@x.com" } });
    assert_eq!(subscription_active_from_profile(&p), None);
}

#[test]
fn plan_label_maps_org_type_and_tier() {
    assert_eq!(
        plan_label(Some("claude_max"), Some("default_claude_max_20x")).as_deref(),
        Some("Max 20x")
    );
    assert_eq!(
        plan_label(Some("claude_max"), Some("default_claude_max_5x")).as_deref(),
        Some("Max 5x")
    );
    assert_eq!(plan_label(Some("claude_pro"), None).as_deref(), Some("Pro"));
    assert_eq!(
        plan_label(Some("claude_free"), None).as_deref(),
        Some("Free")
    );
    assert_eq!(plan_label(Some("something_new"), None), None);
    assert_eq!(plan_label(None, None), None);
}

#[test]
fn plan_label_maps_every_org_type_and_tier() {
    assert_eq!(
        plan_label(Some("claude_team"), None).as_deref(),
        Some("Team")
    );
    assert_eq!(
        plan_label(Some("claude_enterprise"), None).as_deref(),
        Some("Enterprise")
    );
    // A Max org with no / unrecognised tier is plain "Max", not a multiplier.
    assert_eq!(plan_label(Some("claude_max"), None).as_deref(), Some("Max"));
    assert_eq!(
        plan_label(Some("claude_max"), Some("default_claude_max")).as_deref(),
        Some("Max")
    );
}

#[test]
fn plan_status_from_profile_reports_label_and_activity() {
    let s = plan_status_from_profile(&sample_profile()).unwrap();
    assert!(s.active);
    assert_eq!(s.label.as_deref(), Some("Max 20x"));

    let lapsed = serde_json::json!({
        "organization": { "organization_type": "claude_free", "subscription_status": "canceled" }
    });
    let s = plan_status_from_profile(&lapsed).unwrap();
    assert!(!s.active);
    assert_eq!(s.label.as_deref(), Some("Free"));

    // No organization at all: unknown, not "lapsed".
    assert!(plan_status_from_profile(&serde_json::json!({})).is_none());
}

#[test]
fn fetch_error_display_texts() {
    assert_eq!(
        FetchError::RateLimited.to_string(),
        "rate limited (HTTP 429)"
    );
    assert_eq!(
        FetchError::Auth.to_string(),
        "unauthorized (token expired or revoked)"
    );
    assert_eq!(FetchError::Forbidden.to_string(), "forbidden (HTTP 403)");
    assert_eq!(
        FetchError::Transient("x".into()).to_string(),
        "transient error: x"
    );
    assert_eq!(FetchError::Other("y".into()).to_string(), "y");
}

// --- provider-reported extras (spend / extra_usage / breakdown / limits) ---

/// The live response shape (captured 2026-09-27) for an account with extra
/// usage off, nothing spent, all usage in Claude Code, and an unused Fable
/// weekly bucket.
fn live_usage_json() -> serde_json::Value {
    serde_json::json!({
        "five_hour": { "utilization": 74.0, "resets_at": "2026-09-27T20:00:00+00:00" },
        "seven_day": { "utilization": 73.0, "resets_at": "2026-09-29T23:00:00+00:00" },
        "seven_day_opus": null,
        "extra_usage": {
            "is_enabled": false, "monthly_limit": null, "used_credits": null,
            "utilization": null, "currency": null, "decimal_places": null,
            "disabled_reason": null, "user_disabled": false,
            "spend_limit_reached": false, "credits_ever_enabled": false,
            "daily": null, "weekly": null
        },
        "spend": {
            "used": { "amount_minor": 0, "currency": "USD", "exponent": 2 },
            "limit": null, "percent": 0, "severity": "normal", "enabled": false,
            "disabled_reason": null, "cap": null, "balance": null, "auto_reload": null,
            "disclaimer": "…", "can_purchase_credits": false, "can_toggle": false
        },
        "seven_day_breakdown": {
            "as_of": "2026-09-27T18:00:00+00:00",
            "window_started_at": "2026-09-22T23:00:00+00:00",
            "rows": [
                { "key": "claude_code", "display_name": "Claude Code", "percent": 100 },
                { "key": "chat", "display_name": "Chats", "percent": 0 },
                { "key": "cowork", "display_name": "Cowork", "percent": 0 },
                { "key": "other", "display_name": "Other", "percent": 0 }
            ]
        },
        "limits": [
            { "kind": "session", "group": "session", "percent": 74, "severity": "normal",
              "resets_at": "2026-09-27T20:00:00+00:00", "scope": null, "is_active": true },
            { "kind": "weekly_all", "group": "weekly", "percent": 73, "severity": "normal",
              "resets_at": "2026-09-29T23:00:00+00:00", "scope": null, "is_active": true },
            { "kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
              "resets_at": "2026-09-29T23:00:00+00:00",
              "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null },
              "is_active": false }
        ]
    })
}

fn parse(v: serde_json::Value) -> Usage {
    serde_json::from_value(v).unwrap()
}

#[test]
fn live_shape_parses_with_no_credits_and_raw_extras() {
    let u = parse(live_usage_json());
    assert_eq!(u.seven_day.as_ref().unwrap().utilization, Some(73.0));
    let r = reported_usage(&u);
    // Extra usage off and $0.00 spent → nothing worth showing.
    assert!(r.credits.is_none());
    // The breakdown is carried as reported (the menu decides it's noise).
    assert_eq!(r.breakdown.len(), 4);
    assert_eq!(r.breakdown[0].label, "Claude Code");
    assert_eq!(r.breakdown[0].key, "claude_code");
    assert_eq!(r.breakdown[0].percent, 100.0);
    // The Fable bucket is reported (at 0%); session/weekly_all are not scoped.
    assert_eq!(r.scoped_limits.len(), 1);
    assert_eq!(r.scoped_limits[0].label, "Fable 7d");
    assert_eq!(r.scoped_limits[0].id, "scoped:fable");
    assert_eq!(r.scoped_limits[0].utilization, Some(0.0));
    assert_eq!(
        r.scoped_limits[0].resets_at.unwrap().to_rfc3339(),
        "2026-09-29T23:00:00+00:00"
    );
}

#[test]
fn missing_extras_report_nothing() {
    let u =
        parse(serde_json::json!({ "five_hour": null, "seven_day": null, "seven_day_opus": null }));
    assert!(reported_usage(&u).is_empty());
}

#[test]
fn all_null_extras_report_nothing() {
    let u = parse(serde_json::json!({
        "five_hour": null, "seven_day": null, "seven_day_opus": null,
        "extra_usage": null, "spend": null, "seven_day_breakdown": null, "limits": null
    }));
    assert!(reported_usage(&u).is_empty());
    let mut v = live_usage_json();
    v["spend"]["used"] = serde_json::Value::Null;
    v["seven_day_breakdown"]["rows"] = serde_json::json!([]);
    v["limits"] = serde_json::json!([]);
    assert!(reported_usage(&parse(v)).is_empty());
}

#[test]
fn unexpected_extra_shapes_never_fail_the_usage_parse() {
    // A renamed/retyped field must not blank the windows.
    let u = parse(serde_json::json!({
        "five_hour": { "utilization": 9.0 },
        "seven_day": null, "seven_day_opus": null,
        "extra_usage": "on", "spend": [1, 2],
        "seven_day_breakdown": { "rows": { "not": "a list" } },
        "limits": [ 3, { "kind": "weekly_scoped", "percent": "high", "scope": {} } ]
    }));
    assert_eq!(u.five_hour.as_ref().unwrap().utilization, Some(9.0));
    assert!(reported_usage(&u).is_empty());
}

#[test]
fn populated_spend_maps_exact_money() {
    let mut v = live_usage_json();
    v["spend"] = serde_json::json!({
        "used": { "amount_minor": 1234, "currency": "USD", "exponent": 2 },
        "limit": { "amount_minor": 5000, "currency": "USD", "exponent": 2 },
        "balance": null, "cap": null, "enabled": true
    });
    let c = reported_usage(&parse(v)).credits.unwrap();
    assert!(c.enabled);
    assert!(!c.limit_reached);
    assert_eq!(c.used.as_ref().unwrap().display(), "$12.34");
    assert_eq!(c.limit.as_ref().unwrap().display(), "$50.00");
    assert!(c.balance.is_none());
}

#[test]
fn spend_limit_reached_is_reported_even_without_money() {
    let mut v = live_usage_json();
    v["extra_usage"]["is_enabled"] = true.into();
    v["extra_usage"]["spend_limit_reached"] = true.into();
    v["spend"] = serde_json::Value::Null;
    let c = reported_usage(&parse(v)).credits.unwrap();
    assert!(c.enabled);
    assert!(c.limit_reached);
    assert!(c.used.is_none());
}

#[test]
fn extra_usage_used_credits_fall_back_when_spend_is_absent() {
    let mut v = live_usage_json();
    v["spend"] = serde_json::Value::Null;
    v["extra_usage"]["is_enabled"] = true.into();
    v["extra_usage"]["used_credits"] = 310.into();
    v["extra_usage"]["decimal_places"] = 2.into();
    v["extra_usage"]["currency"] = "USD".into();
    let c = reported_usage(&parse(v)).credits.unwrap();
    assert_eq!(c.used.unwrap().display(), "$3.10");
}

#[test]
fn scoped_opus_limit_defers_to_the_fixed_opus_window() {
    let mut v = live_usage_json();
    v["seven_day_opus"] = serde_json::json!({ "utilization": 12.0 });
    v["limits"] = serde_json::json!([
        { "kind": "weekly_scoped", "group": "weekly", "percent": 12,
          "scope": { "model": { "display_name": "Opus" } } },
        { "kind": "weekly_scoped", "group": "weekly", "percent": 42,
          "scope": { "model": { "display_name": "Fable" } } },
        { "kind": "weekly_scoped", "group": "weekly", "percent": null,
          "scope": { "model": { "display_name": "Other" } } }
    ]);
    let r = reported_usage(&parse(v));
    assert_eq!(r.scoped_limits.len(), 1, "{:?}", r.scoped_limits);
    assert_eq!(r.scoped_limits[0].label, "Fable 7d");
    assert_eq!(r.scoped_limits[0].utilization, Some(42.0));
    assert!(r.scoped_limits[0].resets_at.is_none());
}
