use super::*;
use crate::lifecycle::ActiveRefreshOutcome;
use crate::store::{Account, State};
use crate::test_support::{
    claude_account, claude_expired, hit_count, mock_claude_blob, prime_active_account,
    seed_claude_state, spawn_mock_token_server, with_claude_home, MockActiveSlotProvider,
    POLL_ACTIVE_LOCK,
};

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
        prime_active_account(&slot);
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

#[test]
fn token_selector_strips_claude_and_rejects_other_qualifiers() {
    assert_eq!(token_selector("claude:a@x.com").unwrap(), "a@x.com");
    assert_eq!(token_selector("a@x.com").unwrap(), "a@x.com");
    let err = token_selector("codex:a@x.com").unwrap_err().to_string();
    assert!(err.contains("Claude-only"), "{err}");
}

#[test]
fn token_for_a_no_longer_active_account_never_rewrites_the_slot() {
    let _cfg = crate::store::ScopedConfigDir::new();
    // The slot still holds A's old grant; A carries a failed-publish marker.
    let provider = MockActiveSlotProvider::new(&mock_claude_blob("old-at", "old-rt", 5));
    let mut a = Account::from_keychain_blob(&mock_claude_blob("new-at", "new-rt", 6)).unwrap();
    a.email = Some(provider.email.clone());
    a.live_write_pending = Some(crate::logging::token_fingerprint(&a.refresh_token));
    let mut b = claude_account("b@e.com", 3_600_000);
    b.email = Some("b@e.com".into());
    // `usagio token A` saw A active; a switch to B committed since.
    seed_claude_state(vec![a, b], Some("b@e.com"));
    let _ = token_active_cas(&provider, &provider.email.clone());
    assert_eq!(
        provider.writes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "reverted the committed switch in the slot"
    );
}

#[test]
fn an_in_sync_active_cas_does_not_rewrite_state() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let blob = mock_claude_blob("live-at", "live-rt", 9_999_999_999_999);
    let provider = MockActiveSlotProvider::new(&blob);
    let mut a = Account::from_keychain_blob(&blob).unwrap();
    a.email = Some(provider.email.clone());
    seed_claude_state(vec![a], Some(&provider.email.clone()));
    let backups = || {
        std::fs::read_dir(crate::store::config_dir().unwrap().join("backups"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    // First pass may normalise; an idle repeat must not rotate a backup.
    let _ = poll_active_cas(&provider, &provider.email.clone()).unwrap();
    let before = backups();
    let (outcome, _) = poll_active_cas(&provider, &provider.email.clone()).unwrap();
    assert!(matches!(outcome, Some(ActiveRefreshOutcome::Adopted)));
    assert_eq!(backups(), before, "an idle CAS rewrote state.json");
}
