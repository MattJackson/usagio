use super::*;
use crate::claude_local::write_claude_identity;
use crate::cli::capture::{capture_current_generic, persist_login};
use crate::lifecycle::refresh_provider_active_account;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::rows::row_from_account;
use crate::store::State;
use crate::switching::identity::sync_active_claude_identity;
use crate::switching::provider::switch_if_still_active;
use crate::test_support::{
    claude_account_with_identity, codex_account, codex_auth_json, seed_claude_state,
    with_claude_home, MockActiveSlotProvider,
};
use crate::{autoswap, login, providers};

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

#[test]
fn compare_and_set_switch_aborts_when_a_pin_lands_after_the_cycle_started() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        for (who, at, rt) in [("a", "at-a", "rt-a"), ("b", "at-b", "rt-b")] {
            let blob = codex_auth_json(&format!("{who}@example.com"), 4_000_000_000, at, rt);
            std::fs::write(dir.path().join("auth.json"), blob).unwrap();
            capture_current_generic("codex").unwrap();
        }
        // The cycle began with no pin on b (active)...
        let cycle_start = State::load().unwrap();
        let _armed = autoswap::shell::arm_expected_pick("codex", &cycle_start);
        // ...then the user pinned b while the switch was preparing.
        let mut st = State::load().unwrap();
        st.set_manual_pick("codex", Some("b@example.com"));
        st.save().unwrap();
        let moved = switch_if_still_active("codex", "a@example.com", "b@example.com").unwrap();
        assert_eq!(
            moved, None,
            "a pin set mid-cycle must win over the auto move"
        );
        let st = State::load().unwrap();
        assert_eq!(
            provider_active(&st, "codex").as_deref(),
            Some("b@example.com")
        );
        assert_eq!(st.manual_pick("codex"), Some("b@example.com"));
    });
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
fn claude_identity_refresh_persists_ultra_without_a_usage_fetch() {
    with_claude_home("a@e.com", |_| {
        let mut account = claude_account_with_identity("a@e.com", 3_600_000);
        account.plan = Some("Max 20x".into());
        account.oauth_account.as_mut().unwrap()["organizationType"] = "claude_max".into();
        account.oauth_account.as_mut().unwrap()["organizationRateLimitTier"] =
            "default_claude_max_20x".into();
        seed_claude_state(vec![account.clone()], Some("a@e.com"));
        let mut identity = account.oauth_account.clone().unwrap();
        identity["planDisplayName"] = "Ultra".into();
        write_claude_identity(&identity, None).unwrap();
        with_state_lock(|| {
            let mut state = State::load()?;
            sync_active_claude_identity(&mut state);
            state.save()
        })
        .unwrap();
        let state = State::load().unwrap();
        let saved = state.find("a@e.com").unwrap();
        assert_eq!(saved.plan.as_deref(), Some("Ultra"));
        assert_eq!(
            saved.oauth_account.as_ref().unwrap()["planDisplayName"],
            "Ultra"
        );
        assert_eq!(saved.keychain_blob, account.keychain_blob);
        assert_eq!(row_from_account(saved).plan.as_deref(), Some("Ultra"));

        // A stale active marker must not copy a different login's plan.
        write_claude_identity(
            &serde_json::json!({
                "emailAddress": "stranger@e.com", "accountUuid": "uuid-stranger@e.com",
                "planDisplayName": "Pro",
            }),
            None,
        )
        .unwrap();
        let mut state = state;
        sync_active_claude_identity(&mut state);
        assert_eq!(
            state.find("a@e.com").unwrap().plan.as_deref(),
            Some("Ultra")
        );
    });
}

#[test]
fn poll_merge_skips_save_when_nothing_changed() {
    use crate::store::{ScopedConfigDir, State};
    let _g = ScopedConfigDir::new();
    let mut st = State {
        active: Some("a@x.com".into()),
        ..State::default()
    };
    let before = serde_json::to_vec(&st).unwrap();
    assert!(
        !save_if_changed(&st, &before).unwrap(),
        "unchanged state must not be saved"
    );
    st.active = Some("b@x.com".into());
    assert!(
        save_if_changed(&st, &before).unwrap(),
        "changed state must be saved"
    );
}

#[test]
fn a_provider_account_added_by_sign_in_keeps_its_store_through_a_sweep() {
    // "Sign in to a new Codex account…" used a fresh store; its renewals must
    // reopen that session, so the store sweep must keep it.
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let k = "added@example.com";
    let captured = providers::trait_def::CapturedAccount {
        identity: providers::IdentitySnapshot {
            email: Some(k.into()),
            uuid: None,
            display_name: None,
            native_blob: serde_json::Value::Null,
        },
        secret_blob: "added".into(),
        tokens: providers::trait_def::TokenGrant {
            access: "added-at".into(),
            refresh: Some("added-rt".into()),
            expires_in_secs: 3600,
        },
    };
    persist_login(&provider, captured, Some("new:fresh")).unwrap();
    let state = State::load().unwrap();
    assert_eq!(
        crate::menubar::signin::renew_login_store(&state, slug, k),
        "new:fresh",
        "a renew must reopen the store the account signed in with"
    );
    let keep = crate::menubar::signin::sign_in_stores_to_keep(&state);
    assert!(
        keep.contains(&login::store_id(slug, "new:fresh")),
        "the sweep would delete the account's working sign-in session"
    );
    // Removing the account forgets the store, so the sweep reclaims it.
    remove_provider_account_generic(slug, k).unwrap();
    let state = State::load().unwrap();
    assert!(state
        .provider_accounts(slug)
        .unwrap()
        .login_stores
        .is_empty());
    assert!(!crate::menubar::signin::sign_in_stores_to_keep(&state)
        .contains(&login::store_id(slug, "new:fresh")));
}
