use super::*;
use crate::poll::trackers::{
    get_usage_fetch_tracker, record_inactive_fetch_failure, reset_usage_fetch_tracker,
};
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use crate::test_support::{
    claude_account, codex_account, escalation_test_email, plan, read_log, seed_claude_state,
};

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

    fn login_capture(&self) -> Option<&dyn crate::providers::LoginCapture> {
        Some(self)
    }

    fn plan_check(&self) -> Option<&dyn crate::providers::PlanCheck> {
        Some(self)
    }
}

impl crate::providers::LoginCapture for PlanProvider {
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
}

impl crate::providers::PlanCheck for PlanProvider {
    fn check_plan(&self, _access_token: &str) -> Option<crate::providers::trait_def::PlanStatus> {
        self.0.clone()
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
fn failed_fetch_keeps_a_plan_restored_earlier_in_the_cycle() {
    let restored = plan("Pro", true);
    let kept = plan_after_failed_fetch(Some(restored.clone()), None);
    assert_eq!(kept.map(|p| p.active), Some(true));
    let lapsed = plan_after_failed_fetch(Some(restored), Some(plan("Free", false)));
    assert_eq!(lapsed.map(|p| p.active), Some(false));
    assert!(plan_after_failed_fetch(None, None).is_none());
}

#[test]
fn an_unchanged_plan_does_not_rewrite_state() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    st.upsert_provider_account("codex", codex_account("c@e.com", None));
    st.save().unwrap();
    persist_plan("codex", "c@e.com", &plan("Plus", true), None);
    let backups = || {
        std::fs::read_dir(crate::store::config_dir().unwrap().join("backups"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let before = backups();
    persist_plan("codex", "c@e.com", &plan("Plus", true), None);
    assert_eq!(backups(), before, "an unchanged plan rotated a backup");
}
