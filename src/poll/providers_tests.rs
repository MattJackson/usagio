use super::*;
use crate::providers::trait_def::PlanStatus;
use crate::store::{CachedUsage, State};
use crate::test_support::codex_account;

#[test]
fn delayed_usage_cannot_replace_new_login_or_newer_observation() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut original = codex_account("account@example.com", None);
    original.identity_email = Some(original.key.clone());
    original.secret_blob = "original-generation".into();
    let stale_usage = CachedUsage {
        session_pct: Some(90.0),
        fetched_at: 10,
        ..Default::default()
    };
    let stale_plan = PlanStatus {
        label: Some("old-plan".into()),
        active: false,
    };
    for change in ["login", "cache", "plan"] {
        let mut newer = original.clone();
        match change {
            "login" => {
                newer.secret_blob = "renewed-generation".into();
                newer.access_token = "renewed-access".into();
            }
            "cache" => {
                newer.cached_usage = Some(CachedUsage {
                    session_pct: Some(5.0),
                    fetched_at: 20,
                    ..Default::default()
                })
            }
            "plan" => newer.plan = Some("new-plan".into()),
            _ => unreachable!(),
        }
        let mut state = State::load().unwrap_or_default();
        state.upsert_provider_account("codex", newer.clone());
        state.save().unwrap();
        assert!(
            !persist_provider_observation(
                "codex",
                &original,
                Some(&stale_usage),
                Some(&stale_plan)
            )
            .unwrap(),
            "stale {change} result was accepted"
        );
        let saved = State::load().unwrap();
        let current = saved.find_provider_account("codex", &original.key).unwrap();
        assert_eq!(current.secret_blob, newer.secret_blob);
        assert_eq!(current.cached_usage, newer.cached_usage);
        assert_eq!(current.plan, newer.plan);
        assert!(!current.no_subscription);
    }
    // The result can still update its own unchanged account while another
    // account is selected; switching accounts must not throw away useful usage.
    let mut state = State::load().unwrap();
    state.upsert_provider_account("codex", original.clone());
    state.provider_accounts_mut("codex").active = Some("other@example.com".into());
    state.save().unwrap();
    assert!(persist_provider_observation("codex", &original, Some(&stale_usage), None).unwrap());
    assert_eq!(
        State::load()
            .unwrap()
            .find_provider_account("codex", &original.key)
            .unwrap()
            .cached_usage
            .as_ref(),
        Some(&stale_usage)
    );
}
