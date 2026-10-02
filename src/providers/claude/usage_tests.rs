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
