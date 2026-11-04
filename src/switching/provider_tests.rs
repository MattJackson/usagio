use super::*;
use crate::cli::capture::{capture_current_generic, persist_login};
use crate::lifecycle::refresh_provider_active_account;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::selectors::resolve_provider_selector;
use crate::store::State;
use crate::test_support::{
    codex_account, codex_auth_json, codex_id_token, codex_login_response, usage_at,
    MockActiveSlotProvider,
};
use crate::{autoswap, lifecycle, login, providers, store, usage_log};
use chrono::Utc;

// --- choose_swap_target (auto-swap guard) ---

#[test]
fn provider_supports_swap_reflects_registered_claude() {
    // Claude is registered, has both usage and switching → swappable.
    assert!(provider_supports_swap(CLAUDE_SLUG));
    // Unknown slugs are treated as non-candidates (safest default).
    assert!(!provider_supports_swap("nope"));
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
        assert_eq!(State::load().unwrap().manual_pick("codex"), None);
        // A manual switch under the trigger (B has no usage yet) doesn't hold,
        // but is recorded as a pick.
        switch_to_provider_account("codex", "a@example.com", true).unwrap();
        switch_to_provider_account("codex", "b@example.com", true).unwrap();
        let state = State::load().unwrap();
        assert_eq!(state.manual_hold("codex"), None);
        assert_eq!(state.manual_pick("codex"), Some("b@example.com"));
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

#[test]
fn provider_adoption_preserves_both_accounts_when_disk_identity_changes() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let a = codex_auth_json("a@example.com", 4_000_000_000, "at-a", "rt-a");
        let b = codex_auth_json("b@example.com", 4_000_000_000, "at-b", "rt-b");
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        capture_current_generic("codex").unwrap();
        std::fs::write(dir.path().join("auth.json"), &b).unwrap();
        capture_current_generic("codex").unwrap();
        let mut st = State::load().unwrap();
        st.provider_accounts_mut("codex").active = Some("a@example.com".into());
        st.save().unwrap();
        refresh_provider_active_account("codex");
        let st = State::load().unwrap();
        assert_eq!(
            st.find_provider_account("codex", "a@example.com")
                .unwrap()
                .secret_blob,
            a
        );
        assert_eq!(
            st.find_provider_account("codex", "b@example.com")
                .unwrap()
                .secret_blob,
            b
        );
        switch_to_provider_account("codex", "a@example.com", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            a
        );
    });
}

#[test]
fn corrupted_provider_account_cannot_be_switched_into_vendor_slot() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let a = codex_auth_json("a@example.com", 4_000_000_000, "at-a", "rt-a");
        let b = codex_auth_json("b@example.com", 4_000_000_000, "at-b", "rt-b");
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        capture_current_generic("codex").unwrap();
        let mut st = State::load().unwrap();
        st.find_provider_account_mut("codex", "a@example.com")
            .unwrap()
            .secret_blob = b;
        st.save().unwrap();
        // Keep the active file empty so outgoing adoption cannot repair the fixture.
        std::fs::remove_file(dir.path().join("auth.json")).unwrap();
        assert!(switch_to_provider_account("codex", "a@example.com", true).is_err());
        assert!(!dir.path().join("auth.json").exists());
    });
}

#[test]
fn handoff_verification_retry_does_not_drain_replacement_or_overwrite_rotation() {
    use std::sync::atomic::Ordering;
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let key = "active@example.com";
    let mut account = codex_account(key, None);
    account.identity_email = Some(key.into());
    account.secret_blob = "target".into();
    let mut state = State::default();
    state.provider_accounts_mut(slug).accounts.push(account);
    state.provider_accounts_mut(slug).active = Some("old@example.com".into());
    state.provider_accounts_mut(slug).pending_switch = Some(store::PendingSwitch {
        renewal: false,
        key: key.into(),
        manual: true,
        from: Some("old@example.com".into()),
        drain_token: Some("old-instance".into()),
        credentials_written: false,
        renewed_account: None,
        leave_latch: None,
    });
    state.save().unwrap();
    provider.fail_handoff.store(true, Ordering::SeqCst);
    assert!(commit_provider_switch(&provider, slug, key, true, true).is_err());
    let mut state = State::load().unwrap();
    assert!(
        state
            .provider_accounts(slug)
            .unwrap()
            .pending_switch
            .as_ref()
            .unwrap()
            .credentials_written
    );
    provider.rotate_externally("newly-rotated-target");
    let status = providers::ClientStatus {
        running: true,
        busy: false,
        account_key: Some(key.into()),
        account_id: None,
        handoff_token: Some("replacement-instance".into()),
    };
    assert!(lifecycle::prepare_handoff(&provider, &status, &mut state, key, true).unwrap());
    commit_provider_switch(&provider, slug, key, true, true).unwrap();
    assert_eq!(provider.writes.load(Ordering::SeqCst), 1);
    assert!(provider.drains.lock().unwrap().is_empty());
    assert_eq!(
        provider.slot.lock().unwrap().as_deref(),
        Some("newly-rotated-target")
    );
    let state = State::load().unwrap();
    assert!(state
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .is_none());
    assert_eq!(
        state.provider_accounts(slug).unwrap().active.as_deref(),
        Some(key)
    );
}

#[test]
fn queued_auto_handoff_preserves_no_bounce_latch_across_restart() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let mut state = State::default();
    state.provider_accounts_mut(slug).active = Some("old@example.com".into());
    state
        .provider_accounts_mut(slug)
        .accounts
        .push(codex_account("target", None));
    let latch = autoswap::memory::Latch {
        window: autoswap::WindowId::Session,
        resets_at: Some(Utc::now().timestamp() + 3600),
        pct: 95.0,
        at: Utc::now().timestamp(),
    };
    {
        let _armed = autoswap::shell::arm_leave_latch(
            slug,
            "old@example.com",
            Some((autoswap::memory::LatchKind::Trigger, latch.clone())),
        );
        let status = providers::ClientStatus {
            running: true,
            busy: true,
            account_key: None,
            account_id: None,
            handoff_token: Some("instance".into()),
        };
        assert!(
            !lifecycle::prepare_handoff(&provider, &status, &mut state, "target", false).unwrap()
        );
    }
    let mut restarted = State::load().unwrap();
    assert!(lifecycle::prepare_handoff(
        &provider,
        &providers::ClientStatus::default(),
        &mut restarted,
        "target",
        false
    )
    .unwrap());
    commit_provider_switch(&provider, slug, "target", false, false).unwrap();
    let saved = State::load().unwrap();
    assert_eq!(
        saved.swap_memory[slug].trigger_left.get("old@example.com"),
        Some(&latch)
    );
}

#[test]
fn active_login_renewal_waits_for_drain_and_survives_old_token_rotation() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut provider = MockActiveSlotProvider::new("old-live-login");
    provider.client_running = true;
    let slug = provider.provider_id();
    let key = "active@example.com";
    let mut old = codex_account(key, None);
    old.identity_email = Some(key.into());
    old.secret_blob = "old-live-login".into();
    let mut state = State::default();
    state.upsert_provider_account(slug, old);
    state.provider_accounts_mut(slug).active = Some(key.into());
    state.save().unwrap();
    let captured = providers::codex::CodexProvider
        .complete_login(&codex_login_response(key, "account-a"))
        .unwrap();
    let renewed_blob = captured.secret_blob.clone();
    let result = persist_login(&provider, captured, None).unwrap();
    assert!(!result.made_live);
    assert_eq!(provider.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        provider.slot.lock().unwrap().as_deref(),
        Some("old-live-login")
    );
    let mut reloaded = State::load().unwrap();
    assert_eq!(
        reloaded
            .find_provider_account(slug, key)
            .unwrap()
            .secret_blob,
        "old-live-login"
    );
    assert_eq!(
        reloaded
            .provider_accounts(slug)
            .unwrap()
            .pending_switch
            .as_ref()
            .unwrap()
            .renewed_account
            .as_ref()
            .unwrap()
            .secret_blob,
        renewed_blob
    );
    // The running client still owns the old grant. Its rotation may be adopted
    // while draining, but it cannot replace the staged browser login.
    reloaded
        .find_provider_account_mut(slug, key)
        .unwrap()
        .secret_blob = "old-grant-rotated".into();
    reloaded.save().unwrap();
    assert!(lifecycle::prepare_handoff(
        &provider,
        &providers::ClientStatus::default(),
        &mut reloaded,
        key,
        true
    )
    .unwrap());
    commit_provider_switch(&provider, slug, key, true, true).unwrap();
    assert_eq!(
        provider.slot.lock().unwrap().as_deref(),
        Some(renewed_blob.as_str())
    );
    let saved = State::load().unwrap();
    assert_eq!(
        saved.find_provider_account(slug, key).unwrap().secret_blob,
        renewed_blob
    );
    assert!(saved
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .is_none());
}

#[test]
fn removing_pending_target_restores_client_without_recreating_account() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("still-valid-vendor-login");
    let pending = store::PendingSwitch {
        renewal: false,
        key: "removed@example.com".into(),
        manual: true,
        from: Some("removed@example.com".into()),
        drain_token: Some("drained-instance".into()),
        credentials_written: false,
        renewed_account: None,
        leave_latch: None,
    };
    let mut state = State::default();
    state
        .provider_accounts_mut(provider.provider_id())
        .pending_switch = Some(pending.clone());
    state.save().unwrap();
    provider
        .process_alive
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(recover_obsolete_pending_switch(&provider, &pending).is_err());
    assert!(State::load()
        .unwrap()
        .provider_accounts(provider.provider_id())
        .unwrap()
        .pending_switch
        .is_some());
    provider
        .process_alive
        .store(false, std::sync::atomic::Ordering::SeqCst);
    recover_obsolete_pending_switch(&provider, &pending).unwrap();
    let state = State::load().unwrap();
    let accounts = state.provider_accounts(provider.provider_id()).unwrap();
    assert!(accounts.pending_switch.is_none());
    assert!(accounts.accounts.is_empty());
    assert_eq!(provider.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn changed_selection_preserves_staged_renewal_as_inactive_credentials() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("current");
    let mut renewed = codex_account("a", None);
    renewed.secret_blob = "renewed-a".into();
    let pending = store::PendingSwitch {
        renewal: false,
        key: "a".into(),
        manual: true,
        from: Some("a".into()),
        drain_token: Some("instance".into()),
        credentials_written: false,
        renewed_account: Some(renewed),
        leave_latch: None,
    };
    let mut state = State::default();
    state.upsert_provider_account(provider.provider_id(), codex_account("a", None));
    state.upsert_provider_account(provider.provider_id(), codex_account("b", None));
    let accounts = state.provider_accounts_mut(provider.provider_id());
    accounts.active = Some("b".into());
    accounts.pending_switch = Some(pending.clone());
    state.save().unwrap();
    recover_obsolete_pending_switch(&provider, &pending).unwrap();
    let state = State::load().unwrap();
    assert_eq!(
        state
            .find_provider_account(provider.provider_id(), "a")
            .unwrap()
            .secret_blob,
        "renewed-a"
    );
    let next = state
        .provider_accounts(provider.provider_id())
        .unwrap()
        .pending_switch
        .as_ref()
        .unwrap();
    assert_eq!(next.key, "b");
    assert!(next.renewed_account.is_none());
    assert_eq!(next.drain_token, pending.drain_token);
}

#[test]
fn auto_switch_hands_off_client_without_holding_state_lock() {
    use std::sync::atomic::Ordering;
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let mut account = codex_account("target@example.com", None);
    account.identity_email = Some("target@example.com".into());
    account.secret_blob = "target".into();
    let mut state = State::default();
    state.provider_accounts_mut(slug).accounts.push(account);
    state.provider_accounts_mut(slug).active = Some("old@example.com".into());
    state.save().unwrap();
    let status = provider.client_status().unwrap();
    let out = switch_provider_if_still_active(
        &provider,
        &status,
        "target@example.com",
        "old@example.com",
    )
    .unwrap();
    assert!(out.is_some());
    assert!(
        !provider.lock_held_at_handoff.load(Ordering::SeqCst),
        "state lock was held across client handoff"
    );
}

#[test]
fn pending_switch_check_reports_a_state_read_failure() {
    let cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    state.provider_accounts_mut("codex").pending_switch = Some(store::PendingSwitch {
        renewal: false,
        key: "a@example.com".into(),
        manual: true,
        from: None,
        drain_token: None,
        credentials_written: false,
        renewed_account: None,
        leave_latch: None,
    });
    state.save().unwrap();
    assert!(try_provider_switch_pending("codex", "a@example.com").unwrap());
    std::fs::write(store::config_dir().unwrap().join("state.json"), "{not json").unwrap();
    assert!(try_provider_switch_pending("codex", "a@example.com").is_err());
    drop(cfg);
}

#[test]
fn auto_commit_aborts_when_a_renewal_was_staged_after_the_cas() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let (k, j) = ("active@example.com", "target@example.com");
    let mut state = State::default();
    for key in [k, j] {
        let mut a = codex_account(key, None);
        a.identity_email = Some(key.into());
        state.provider_accounts_mut(slug).accounts.push(a);
    }
    state.provider_accounts_mut(slug).active = Some(k.into());
    // The renewal staged by persist_login between the CAS and the commit.
    state.provider_accounts_mut(slug).pending_switch = Some(store::PendingSwitch {
        renewal: false,
        key: k.into(),
        manual: true,
        from: Some(k.into()),
        drain_token: None,
        credentials_written: false,
        renewed_account: Some(codex_account(k, None)),
        leave_latch: None,
    });
    state.save().unwrap();
    let out = commit_provider_switch_guarded(&provider, slug, j, false, false, Some(k)).unwrap();
    assert!(out.is_none());
    assert_eq!(provider.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
    let state = State::load().unwrap();
    let pa = state.provider_accounts(slug).unwrap();
    assert_eq!(pa.pending_switch.as_ref().unwrap().key, k);
    assert!(pa
        .pending_switch
        .as_ref()
        .unwrap()
        .renewed_account
        .is_some());
    assert_eq!(pa.active.as_deref(), Some(k));
}

#[test]
fn stale_pending_retry_does_not_overwrite_a_newer_queued_switch() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let (a, b) = ("a@e.com", "b@e.com");
    let mut state = State::default();
    state.upsert_provider_account("codex", codex_account(a, None));
    state.upsert_provider_account("codex", codex_account(b, None));
    let pending = |key: &str| crate::store::PendingSwitch {
        renewal: false,
        key: key.into(),
        manual: true,
        from: None,
        drain_token: None,
        credentials_written: false,
        leave_latch: None,
        renewed_account: None,
    };
    let stale = pending(a);
    // The user queued B after the poller snapshotted A.
    state.provider_accounts_mut("codex").pending_switch = Some(pending(b));
    state.save().unwrap();
    switch_provider_expecting("codex", a, true, Some(&stale)).unwrap();
    let after = State::load().unwrap();
    let now = after.provider_accounts("codex").unwrap();
    assert_eq!(now.pending_switch.as_ref().unwrap().key, b);
    assert_eq!(now.active, None);
}

#[test]
fn running_client_renewal_stays_a_renewal_through_drain_and_retry() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut provider = MockActiveSlotProvider::new("old-live-login");
    provider.client_running = true;
    let slug = provider.provider_id();
    let key = "active@example.com";
    let mut old = codex_account(key, None);
    old.identity_email = Some(key.into());
    let mut state = State::default();
    state.upsert_provider_account(slug, old);
    state.provider_accounts_mut(slug).active = Some(key.into());
    state.save().unwrap();
    let captured = providers::codex::CodexProvider
        .complete_login(&codex_login_response(key, "account-a"))
        .unwrap();
    let result = persist_login(&provider, captured, None).unwrap();
    assert!(!result.made_live);
    let pending = State::load()
        .unwrap()
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .clone()
        .unwrap();
    assert!(
        !pending.manual && pending.renewal,
        "drain rewrote the renewal"
    );
    // The client drains; the retry commits with the pending's own flags.
    provider
        .process_alive
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut st = State::load().unwrap();
    let ready = lifecycle::prepare_handoff(
        &provider,
        &providers::ClientStatus::default(),
        &mut st,
        key,
        pending.manual,
    )
    .unwrap();
    assert!(ready);
    commit_provider_switch(&provider, slug, key, pending.manual, pending.manual).unwrap();
    let after = State::load().unwrap();
    assert_eq!(after.manual_pick(slug), None);
    assert_eq!(after.manual_hold(slug), None);
}

#[test]
fn auto_pick_commit_on_the_active_account_still_clears_pick_and_hold() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("live");
    let slug = provider.provider_id();
    let key = "a@example.com";
    let mut state = State::default();
    state.upsert_provider_account(slug, codex_account(key, None));
    state.provider_accounts_mut(slug).active = Some(key.into());
    state.set_manual_pick(slug, Some(key));
    state.set_manual_hold(slug, Some(key));
    state.save().unwrap();
    let mut st = State::load().unwrap();
    assert!(lifecycle::prepare_handoff(
        &provider,
        &providers::ClientStatus::default(),
        &mut st,
        key,
        false
    )
    .unwrap());
    commit_provider_switch(&provider, slug, key, false, false).unwrap();
    let after = State::load().unwrap();
    assert_eq!(after.manual_pick(slug), None, "pick survived an auto-pick");
    assert_eq!(after.manual_hold(slug), None, "hold survived an auto-pick");
}

#[test]
fn drained_renewal_keeps_the_users_pick_hold_and_swap_memory() {
    // A renewal of the active login must not touch the pick, hold or swap
    // memory, even when it waits for a running client to drain.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut provider = MockActiveSlotProvider::new("old-live-login");
    provider.client_running = true;
    let slug = provider.provider_id();
    let key = "active@example.com";
    let mut old = codex_account(key, None);
    old.identity_email = Some(key.into());
    let mut state = State::default();
    state.upsert_provider_account(slug, old);
    state.provider_accounts_mut(slug).active = Some(key.into());
    state.set_manual_pick(slug, Some(key));
    state.set_manual_hold(slug, Some(key));
    state
        .swap_memory
        .entry(slug.to_string())
        .or_default()
        .last_switch_at = Some(1_000);
    state.save().unwrap();
    let seeded = State::load().unwrap();
    let memory_before = seeded.swap_memory.get(slug).cloned();
    assert_eq!(seeded.manual_pick(slug), Some(key));
    assert_eq!(seeded.manual_hold(slug), Some(key));

    let captured = providers::codex::CodexProvider
        .complete_login(&codex_login_response(key, "account-a"))
        .unwrap();
    let result = persist_login(&provider, captured, None).unwrap();
    assert!(!result.made_live);
    provider
        .process_alive
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut st = State::load().unwrap();
    let staged = st.provider_accounts(slug).unwrap().pending_switch.clone();
    let manual = staged.as_ref().unwrap().manual;
    assert!(lifecycle::prepare_handoff(
        &provider,
        &providers::ClientStatus::default(),
        &mut st,
        key,
        manual,
    )
    .unwrap());
    // Commit with the flags production reads after the drain.
    let pending = State::load()
        .unwrap()
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .clone()
        .unwrap();
    assert!(!pending.manual && pending.renewal, "{pending:?}");
    commit_provider_switch(&provider, slug, key, pending.manual, pending.manual).unwrap();
    let after = State::load().unwrap();
    assert_eq!(
        after.manual_pick(slug),
        Some(key),
        "renewal cleared the pick"
    );
    assert_eq!(
        after.manual_hold(slug),
        Some(key),
        "renewal cleared the hold"
    );
    assert_eq!(
        after.swap_memory.get(slug).cloned(),
        memory_before,
        "renewal rewrote swap memory"
    );
}

/// A Codex `auth.json` for `email` in ChatGPT workspace `account_id`.
fn codex_workspace_auth_json(email: &str, account_id: &str, access: &str) -> String {
    serde_json::json!({
        "tokens": {
            "id_token": codex_id_token(email, 4_000_000_000),
            "access_token": access,
            "refresh_token": format!("rt-{account_id}"),
            "account_id": account_id,
        }
    })
    .to_string()
}

#[test]
fn codex_workspaces_of_one_email_are_separate_accounts() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let personal = codex_workspace_auth_json("a@example.com", "ws-personal", "at-p");
        let team = codex_workspace_auth_json("a@example.com", "ws-team", "at-t");
        std::fs::write(dir.path().join("auth.json"), &personal).unwrap();
        let (k1, _) = capture_current_generic("codex").unwrap();
        std::fs::write(dir.path().join("auth.json"), &team).unwrap();
        let (k2, existed) = capture_current_generic("codex").unwrap();
        assert!(!existed, "the team workspace must not overwrite personal");
        assert_eq!(k1, "a@example.com#ws-personal");
        assert_eq!(k2, "a@example.com#ws-team");
        let state = State::load().unwrap();
        assert_eq!(state.provider_accounts("codex").unwrap().accounts.len(), 2);
        assert_eq!(
            state
                .find_provider_account("codex", &k1)
                .unwrap()
                .access_token,
            "at-p"
        );
        // The email alone is ambiguous; the full key selects one.
        assert!(resolve_provider_selector(&state, "a@example.com").is_none());
        assert_eq!(
            resolve_provider_selector(&state, "a@example.com#ws-team"),
            Some(("codex".to_string(), k2.clone()))
        );
        // Labels show the workspace only because the email is shared.
        assert_eq!(
            provider_account_label("codex", &k2),
            "a@example.com (workspace ws-team)"
        );
        // The renewal hint is the email, not the workspace key.
        assert_eq!(providers::trait_def::key_email(&k2), "a@example.com");
    });
}

#[test]
fn email_keyed_codex_state_upgrades_on_load_and_keeps_its_history() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let blob = codex_workspace_auth_json("a@example.com", "ws-1", "at-1");
    let raw = serde_json::json!({
        "schema_version": 2,
        "accounts": [],
        "providers": {"codex": {
            "active": "a@example.com",
            "accounts": [{
                "key": "a@example.com",
                "secret_blob": blob,
                "access_token": "at-1",
                "identity_email": "a@example.com",
            }],
        }},
        "manual_picks": {"codex": "a@example.com"},
        "swap_memory": {"codex": {"left_at": {"a@example.com": 100}}},
    });
    let path = crate::store::config_dir().unwrap().join("state.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, raw.to_string()).unwrap();
    let old_row = usage_log::Snapshot {
        ts: Utc::now() - chrono::Duration::hours(1),
        provider: "codex".into(),
        account: "a@example.com".into(),
        weekly_pct: Some(42.0),
        ..Default::default()
    };
    usage_log::append(&old_row).unwrap();

    let state = State::load().unwrap();
    let key = "a@example.com#ws-1";
    let pa = state.provider_accounts("codex").unwrap();
    assert_eq!(pa.accounts[0].key, key, "workspace derived from the blob");
    assert_eq!(pa.active.as_deref(), Some(key));
    assert_eq!(
        state.manual_picks.get("codex").map(String::as_str),
        Some(key)
    );
    assert!(state.swap_memory["codex"].left_at.contains_key(key));
    // The save guard accepts the rename, and it sticks.
    state.save().unwrap();
    let again = State::load().unwrap();
    assert!(again.find_provider_account("codex", key).is_some());
    // Rows written under the old key are still this account's history.
    let last = usage_log::last_snapshot(&usage_log::AccountKey::new("codex", key)).unwrap();
    assert_eq!(last.weekly_pct, Some(42.0));
}

#[test]
fn an_upgraded_codex_key_keeps_the_sign_in_store_it_was_added_with() {
    // "Sign in to a new Codex account…" remembered its store under the
    // email-only key; the workspace re-key must carry it, or renewals open a
    // fresh session and the sweep deletes the working one.
    let _cfg = crate::store::ScopedConfigDir::new();
    let blob = codex_workspace_auth_json("a@example.com", "ws-1", "at-1");
    let raw = serde_json::json!({
        "schema_version": 2,
        "accounts": [],
        "providers": {"codex": {
            "accounts": [{
                "key": "a@example.com",
                "secret_blob": blob,
                "access_token": "at-1",
                "identity_email": "a@example.com",
            }],
            "login_stores": {"a@example.com": "new:fresh"},
        }},
    });
    let path = crate::store::config_dir().unwrap().join("state.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, raw.to_string()).unwrap();

    let state = State::load().unwrap();
    let key = "a@example.com#ws-1";
    assert_eq!(
        state.provider_accounts("codex").unwrap().accounts[0].key,
        key
    );
    assert_eq!(
        state.provider_accounts("codex").unwrap().login_store(key),
        Some("new:fresh")
    );
    assert_eq!(
        crate::menubar::renew_login_store(&state, "codex", key),
        "new:fresh",
        "a renew must reopen the store the account signed in with"
    );
    assert!(crate::menubar::sign_in_stores_to_keep(&state)
        .contains(&login::store_id("codex", "new:fresh")));
    // Saved under the new key, it survives the next load too.
    state.save().unwrap();
    let again = State::load().unwrap();
    let stores = &again.provider_accounts("codex").unwrap().login_stores;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores.get(key).map(String::as_str), Some("new:fresh"));
}
