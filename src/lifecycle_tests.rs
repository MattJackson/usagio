use super::*;
use crate::cli::capture::capture_current_generic;
use crate::providers::claude::oauth;
use crate::providers::trait_def::ClientHandoff;
use crate::providers::Provider;
use crate::store::{Account, State};
use crate::test_support::{
    codex_account, codex_auth_json, codex_id_token, hit_count, mock_claude_blob, read_log,
    spawn_mock_token_server, spawn_mock_usage_server, MockActiveSlotProvider,
};
use crate::{providers, store};
use chrono::Utc;

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

fn with_inactive_codex_account<R>(exp: i64, f: impl FnOnce(&std::path::Path, &str) -> R) -> R {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_str().unwrap().to_string();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(&home), || {
        let blob = codex_auth_json("user@example.com", exp, "at", "rt");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();
        capture_current_generic("codex").unwrap();
        // State lags the vendor login: the account reads as inactive.
        let mut st = State::load().unwrap();
        st.provider_accounts_mut("codex").active = None;
        st.save().unwrap();
        f(dir.path(), &blob)
    })
}

#[test]
fn vendor_slot_read_error_skips_our_refresh_but_returns_the_saved_credential() {
    for expired in [false, true] {
        let exp = if expired {
            Utc::now().timestamp() + 10
        } else {
            4_000_000_000
        };
        with_inactive_codex_account(exp, |dir, _| {
            // Garbled vendor file: a persistent read/parse error.
            std::fs::write(dir.join("auth.json"), "{not json").unwrap();
            let (base_url, hits) = spawn_mock_token_server();
            #[allow(clippy::disallowed_methods)]
            std::env::set_var(
                "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
                format!("{base_url}/v1/oauth/token"),
            );
            let res = crate::lifecycle::ensure_provider_fresh(
                crate::providers::get("codex").unwrap(),
                "user@example.com",
            );
            #[allow(clippy::disallowed_methods)]
            std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
            let acct = res.expect("the saved credential is returned, not an error");
            assert_eq!(acct.access_token, "at");
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }
}

#[test]
fn api_key_only_vendor_slot_does_not_block_our_refresh_of_a_saved_account() {
    with_inactive_codex_account(Utc::now().timestamp() + 10, |dir, _| {
        std::fs::write(dir.join("auth.json"), r#"{"OPENAI_API_KEY":"sk-x"}"#).unwrap();
        let (base_url, hits) = spawn_mock_token_server();
        #[allow(clippy::disallowed_methods)]
        std::env::set_var(
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            format!("{base_url}/v1/oauth/token"),
        );
        let res = crate::lifecycle::ensure_provider_fresh(
            crate::providers::get("codex").unwrap(),
            "user@example.com",
        );
        #[allow(clippy::disallowed_methods)]
        std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the vendor holds no OAuth login, so the saved one is ours to refresh"
        );
    });
}

#[test]
fn adopting_a_vendor_blob_without_a_refresh_token_keeps_the_saved_one() {
    with_inactive_codex_account(4_000_000_000, |dir, _| {
        let blob = serde_json::json!({
            "tokens": {
                "id_token": codex_id_token("user@example.com", 4_000_000_000),
                "access_token": "vendor-at",
            }
        })
        .to_string();
        std::fs::write(dir.join("auth.json"), &blob).unwrap();
        let _ = crate::lifecycle::ensure_provider_fresh(
            crate::providers::get("codex").unwrap(),
            "user@example.com",
        );
        let st = State::load().unwrap();
        let acct = st
            .find_provider_account("codex", "user@example.com")
            .unwrap();
        assert_eq!(acct.access_token, "vendor-at", "adopted");
        assert_eq!(acct.refresh_token, "rt", "saved refresh token kept");
    });
}

// --- mutation-hardening: remaining switch / adopt paths ---

#[test]
fn active_refresh_reports_whether_the_vendor_cli_rotated_since_the_last_cycle() {
    let _g = crate::store::ScopedConfigDir::new();
    let same = mock_claude_blob("same-at", "same-rt", 1);
    let mut provider = MockActiveSlotProvider::new(&same);
    provider.email = "log@example.com".into();
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
fn claude_active_adoption_rejects_other_accounts_credentials() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let original = mock_claude_blob("a-at", "a-rt", 1);
    let other = mock_claude_blob("b-at", "b-rt", 2);
    let provider = MockActiveSlotProvider::new(&other);
    let mut account = Account::from_keychain_blob(&original).unwrap();
    account.email = Some("other@example.com".into());
    account.needs_relogin = true;
    assert!(matches!(
        active_refresh_cas(&provider, &mut account),
        ActiveRefreshOutcome::RefreshFailed
    ));
    assert_eq!(account.keychain_blob, original);
    assert_eq!(account.access_token, "a-at");
    assert_eq!(account.refresh_token, "a-rt");
    assert!(account.needs_relogin);
}

#[test]
fn graceful_handoff_drains_once_and_preserves_credentials_until_exit() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing-secret");
    let mut state = State::default();
    state.provider_accounts_mut(provider.provider_id()).active = Some("a@example.com".into());
    state.save().unwrap();
    let mut status = providers::ClientStatus {
        running: true,
        busy: true,
        account_key: Some("a@example.com".into()),
        account_id: None,
        handoff_token: Some("instance-1".into()),
    };
    assert!(!prepare_handoff(&provider, &status, &mut state, "b@example.com", true).unwrap());
    // Reload simulates another usagio process or a restart while draining.
    let mut reloaded = State::load().unwrap();
    status.busy = false;
    assert!(!prepare_handoff(&provider, &status, &mut reloaded, "b@example.com", true).unwrap());
    assert_eq!(*provider.drains.lock().unwrap(), vec!["instance-1"]);
    assert_eq!(
        provider.slot.lock().unwrap().as_deref(),
        Some("outgoing-secret")
    );
    assert_eq!(
        reloaded
            .provider_accounts(provider.provider_id())
            .unwrap()
            .active
            .as_deref(),
        Some("a@example.com")
    );
    // Idle is insufficient; the daemon must actually stop before credential IO.
    status.running = false;
    provider
        .process_alive
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(prepare_handoff(&provider, &status, &mut reloaded, "b@example.com", true).is_err());
    assert_eq!(
        provider.slot.lock().unwrap().as_deref(),
        Some("outgoing-secret")
    );
    assert_eq!(
        State::load()
            .unwrap()
            .provider_accounts(provider.provider_id())
            .unwrap()
            .pending_switch
            .as_ref()
            .unwrap()
            .drain_token
            .as_deref(),
        Some("instance-1")
    );
    provider
        .process_alive
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(prepare_handoff(&provider, &status, &mut reloaded, "b@example.com", true).unwrap());
    assert_eq!(provider.drains.lock().unwrap().len(), 1);
}

#[test]
fn changing_a_pending_target_does_not_force_the_draining_daemon() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing-secret");
    let mut state = State::default();
    state.save().unwrap();
    let status = providers::ClientStatus {
        running: true,
        busy: true,
        account_key: None,
        account_id: None,
        handoff_token: Some("same-instance".into()),
    };
    prepare_handoff(&provider, &status, &mut state, "b@example.com", false).unwrap();
    prepare_handoff(&provider, &status, &mut state, "c@example.com", true).unwrap();
    assert_eq!(provider.drains.lock().unwrap().len(), 1);
    let stored = State::load().unwrap();
    let pending = stored
        .provider_accounts(provider.provider_id())
        .unwrap()
        .pending_switch
        .as_ref()
        .unwrap();
    assert_eq!(pending.key, "c@example.com");
    assert!(pending.manual);
}

#[test]
fn idempotent_drain_retries_failed_delivery_without_losing_intent() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut provider = MockActiveSlotProvider::new("outgoing");
    provider.retry_drain = true;
    provider
        .fail_drain
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut state = State::default();
    let status = providers::ClientStatus {
        running: true,
        busy: true,
        account_key: None,
        account_id: None,
        handoff_token: Some("instance".into()),
    };
    assert!(prepare_handoff(&provider, &status, &mut state, "target", false).is_err());
    let mut reloaded = State::load().unwrap();
    assert!(!prepare_handoff(&provider, &status, &mut reloaded, "target", false).unwrap());
    assert_eq!(provider.drains.lock().unwrap().len(), 2);
    assert_eq!(provider.slot.lock().unwrap().as_deref(), Some("outgoing"));
}

#[test]
fn failed_refresh_lock_never_sends_a_single_use_refresh_grant() {
    let _cfg = crate::store::ScopedConfigDir::new();
    std::fs::create_dir_all(store::config_dir().unwrap().join("refresh.lock")).unwrap();
    let (url, hits) = spawn_mock_usage_server(500, "{}".into());
    oauth::set_token_url_override(Some(&url));
    let mut account =
        Account::from_keychain_blob(&mock_claude_blob("expired", "single-use", 1)).unwrap();
    let result = ensure_claude_fresh(&mut account, 60);
    oauth::set_token_url_override(None);
    assert!(matches!(result, Err(oauth::RefreshError::Transient(_))));
    assert_eq!(hit_count(&hits), 0);
    assert_eq!(account.refresh_token, "single-use");
}

#[test]
fn unreadable_state_never_sends_a_single_use_refresh_grant() {
    let _cfg = crate::store::ScopedConfigDir::new();
    std::fs::create_dir_all(store::config_dir().unwrap()).unwrap();
    std::fs::write(store::config_dir().unwrap().join("state.json"), "{corrupt").unwrap();
    let (url, hits) = spawn_mock_usage_server(500, "{}".into());
    oauth::set_token_url_override(Some(&url));
    let mut account =
        Account::from_keychain_blob(&mock_claude_blob("expired", "single-use", 1)).unwrap();
    let result = ensure_claude_fresh(&mut account, 60);
    oauth::set_token_url_override(None);
    assert!(matches!(result, Err(oauth::RefreshError::Transient(_))));
    assert_eq!(hit_count(&hits), 0);
}

#[test]
fn a_uuid_only_record_does_not_match_a_uuidless_identity() {
    // Expected: uuid, no email. Actual: no uuid, some other email.
    assert!(!identity_fields_match(
        Some("u-1"),
        None,
        None,
        Some("other@example.com")
    ));
    // No uuid on either side and no expected email: nothing to contradict.
    assert!(identity_fields_match(
        None,
        None,
        None,
        Some("a@example.com")
    ));
    assert!(identity_fields_match(
        Some("u-1"),
        Some("a@example.com"),
        None,
        Some("A@example.com")
    ));
}

#[test]
fn active_cas_adopts_a_vendor_rotation_even_with_a_lower_expiry() {
    let _g = crate::store::ScopedConfigDir::new();
    // Claude Code rotated; the slot's expiry is LOWER than ours (clock step or
    // shorter server lifetime). Without a pending marker it must be adopted.
    let cc_blob = mock_claude_blob("cc-at", "cc-rt", 1_000);
    let provider = MockActiveSlotProvider::new(&cc_blob);
    let mut acct =
        Account::from_keychain_blob(&mock_claude_blob("old-at", "old-rt", 9_999_999_999_999))
            .unwrap();
    acct.email = Some("active@example.com".into());
    let outcome = active_refresh_cas(&provider, &mut acct);
    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "{outcome:?}"
    );
    assert_eq!(
        acct.refresh_token, "cc-rt",
        "vendor rotation was overwritten"
    );
    assert_eq!(provider.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn active_cas_rewrites_the_slot_only_for_a_pending_live_write() {
    let _g = crate::store::ScopedConfigDir::new();
    let old_blob = mock_claude_blob("old-at", "old-rt", 1_000);
    let new_blob = mock_claude_blob("new-at", "new-rt", 2_000);
    let provider = MockActiveSlotProvider::new(&old_blob);
    let mut acct = Account::from_keychain_blob(&new_blob).unwrap();
    acct.email = Some("active@example.com".into());
    acct.live_write_pending = Some(crate::logging::token_fingerprint("new-rt"));
    let outcome = active_refresh_cas(&provider, &mut acct);
    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "{outcome:?}"
    );
    assert_eq!(
        acct.refresh_token, "new-rt",
        "renewal was reverted to the slot"
    );
    assert_eq!(acct.live_write_pending, None, "marker not cleared");
    assert_eq!(provider.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    let slot =
        Account::from_keychain_blob(&provider.slot.lock().unwrap().clone().unwrap()).unwrap();
    assert_eq!(slot.refresh_token, "new-rt");
    // A marker for some OTHER refresh token is stale: adopt the slot.
    let mut other = Account::from_keychain_blob(&new_blob).unwrap();
    other.email = Some("active@example.com".into());
    other.live_write_pending = Some(crate::logging::token_fingerprint("older-rt"));
    *provider.slot.lock().unwrap() = Some(mock_claude_blob("v-at", "v-rt", 1));
    active_refresh_cas(&provider, &mut other);
    assert_eq!(other.refresh_token, "v-rt");
}

#[test]
fn unchanged_drain_retry_does_not_resave_state() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut provider = MockActiveSlotProvider::new("live");
    provider.client_running = true;
    let slug = provider.provider_id();
    let mut state = State::default();
    state.upsert_provider_account(slug, codex_account("a@e.com", None));
    state.upsert_provider_account(slug, codex_account("b@e.com", None));
    state.provider_accounts_mut(slug).active = Some("a@e.com".into());
    state.save().unwrap();
    let status = provider.client_status().unwrap();
    assert!(!prepare_handoff(&provider, &status, &mut state, "b@e.com", true).unwrap());
    let backups = || {
        std::fs::read_dir(crate::store::config_dir().unwrap().join("backups"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let before = backups();
    let mut again = State::load().unwrap();
    assert!(!prepare_handoff(&provider, &status, &mut again, "b@e.com", true).unwrap());
    assert_eq!(backups(), before, "an unchanged retry rotated a backup");
}
