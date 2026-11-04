use super::*;
use crate::claude_local::write_claude_identity;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::rows::claude_plan_label;
use crate::store::{Account, State};
use crate::test_support::{
    claude_account, claude_account_with_identity, prime_active_slot, seed_claude_state,
    with_claude_home,
};

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

#[test]
fn inactive_rotation_is_mirrored_only_while_the_vendor_cli_is_that_account() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let mut acct = claude_account("a@e.com", 3_600_000);
    acct.set_tokens("at-mirror".into(), "rt-mirror".into(), 7_777);
    with_claude_home("a@e.com", |_| {
        prime_active_slot(&claude_account("a@e.com", 1_000).keychain_blob);
        let got = mirror_inactive_rotation(provider, &acct);
        assert!(matches!(got, Some(Ok(()))), "{got:?}");
        let slot = provider
            .active_slot()
            .unwrap()
            .read_active_slot()
            .unwrap()
            .unwrap();
        assert_eq!(
            Account::from_keychain_blob(&slot).unwrap().access_token,
            "at-mirror"
        );
    });
    with_claude_home("stranger@e.com", |_| {
        let before = claude_account("stranger@e.com", 1_000).keychain_blob;
        prime_active_slot(&before);
        assert!(mirror_inactive_rotation(provider, &acct).is_none());
        assert_eq!(
            provider
                .active_slot()
                .unwrap()
                .read_active_slot()
                .unwrap()
                .unwrap(),
            before
        );
    });
}

#[test]
fn claude_identity_refresh_keeps_missing_fields_and_lapsed_status() {
    let mut account = claude_account_with_identity("a@e.com", 3_600_000);
    account.oauth_account.as_mut().unwrap()["planDisplayName"] = "Ultra".into();
    update_claude_identity(&mut account, &serde_json::json!({"planDisplayName": null}));
    assert_eq!(claude_plan_label(&account).as_deref(), Some("Ultra"));
    account.no_subscription = true;
    account.plan = Some("Free".into());
    update_claude_identity(
        &mut account,
        &serde_json::json!({"planDisplayName": "Ultra"}),
    );
    assert_eq!(claude_plan_label(&account).as_deref(), Some("Free"));
}

#[test]
fn older_cli_profile_does_not_undo_a_background_profile_refresh() {
    with_claude_home("a@e.com", |_| {
        let mut account = claude_account_with_identity("a@e.com", 3_600_000);
        account.plan = Some("Ultra".into());
        account.oauth_account.as_mut().unwrap()["planDisplayName"] = "Ultra".into();
        account.oauth_account.as_mut().unwrap()["profileFetchedAt"] = 2000.into();
        seed_claude_state(vec![account.clone()], Some("a@e.com"));
        let mut old = account.oauth_account.clone().unwrap();
        old["planDisplayName"] = "Max 20x".into();
        old["profileFetchedAt"] = 1000.into();
        write_claude_identity(&old, None).unwrap();
        let mut state = State::load().unwrap();
        sync_active_claude_identity(&mut state);
        assert_eq!(state.accounts[0].plan.as_deref(), Some("Ultra"));
        old["profileFetchedAt"] = 3000.into();
        old["planDisplayName"] = "New plan".into();
        write_claude_identity(&old, None).unwrap();
        sync_active_claude_identity(&mut state);
        assert_eq!(state.accounts[0].plan.as_deref(), Some("New plan"));
    });
}
