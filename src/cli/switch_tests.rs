use super::*;
use crate::cli::capture::capture_current_generic;
use crate::providers::CLAUDE_SLUG;
use crate::selectors::resolve_provider_selector;
use crate::store::{Account, State};
use crate::test_support::{
    acct_with_cache, claude_account_with_identity, codex_account, codex_auth_json,
    mock_claude_blob, seed_claude_state, usage_at, with_claude_home,
};
use chrono::Utc;

#[test]
fn switch_reports_ambiguous_prefix_instead_of_no_match() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    for email in ["dev@e.com", "dev2@e.com"] {
        let mut a = acct_with_cache(None);
        a.email = Some(email.to_string());
        state.upsert(a);
    }
    state.save().unwrap();
    let err = cmd_switch(Some("dev"), None).unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(
        err.contains("dev@e.com") && err.contains("dev2@e.com"),
        "{err}"
    );
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn switch_command_activates_the_pick_and_holds_it_only_when_over_the_trigger() {
    with_claude_home("a@e.com", |_| {
        let mut over = claude_account_with_identity("over@e.com", 3_600_000);
        over.cached_usage = Some(usage_at(99.0));
        let mut calm = claude_account_with_identity("calm@e.com", 3_600_000);
        calm.cached_usage = Some(usage_at(10.0));
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                over,
                calm,
            ],
            Some("a@e.com"),
        );
        cmd_switch(Some("over"), None).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("over@e.com"));
        assert_eq!(st.manual_hold(CLAUDE_SLUG), Some("over@e.com"));
        cmd_switch(Some("calm"), None).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("calm@e.com"));
        assert_eq!(st.manual_hold(CLAUDE_SLUG), None);
        assert_eq!(st.manual_pick(CLAUDE_SLUG), Some("calm@e.com"));
        // With no selector the best account is auto-picked and not held.
        cmd_switch(None, None).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.manual_hold(CLAUDE_SLUG), None);
        assert_eq!(st.manual_pick(CLAUDE_SLUG), None);
    });
}

#[test]
fn qualified_codex_selector_does_not_switch_same_email_claude_account() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let codex = codex_auth_json("shared@example.com", 4_000_000_000, "codex-at", "codex-rt");
        std::fs::write(dir.path().join("auth.json"), &codex).unwrap();
        capture_current_generic("codex").unwrap();
        // A different Codex account is the one currently signed in.
        let other = codex_auth_json("other@example.com", 4_000_000_000, "other-at", "other-rt");
        std::fs::write(dir.path().join("auth.json"), &other).unwrap();
        capture_current_generic("codex").unwrap();
        let mut state = State::load().unwrap();
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("other@example.com")
        );
        let mut claude = Account::from_keychain_blob(&mock_claude_blob(
            "claude-at",
            "claude-rt",
            4_000_000_000_000,
        ))
        .unwrap();
        claude.email = Some("shared@example.com".into());
        state.accounts.push(claude);
        let mut claude_active = Account::from_keychain_blob(&mock_claude_blob(
            "claude-active-at",
            "claude-active-rt",
            4_000_000_000_000,
        ))
        .unwrap();
        claude_active.email = Some("claude-active@example.com".into());
        state.accounts.push(claude_active);
        state.active = Some("claude-active@example.com".into());
        state.save().unwrap();
        cmd_switch(Some("codex:shared"), None).unwrap();
        let saved = State::load().unwrap();
        // Claude is untouched: same active account, same tokens.
        assert_eq!(saved.active.as_deref(), Some("claude-active@example.com"));
        assert_eq!(
            saved.find("shared@example.com").unwrap().access_token,
            "claude-at"
        );
        // Codex moved to the qualified account, and its login file followed.
        assert_eq!(
            saved.provider_accounts("codex").unwrap().active.as_deref(),
            Some("shared@example.com")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            codex
        );
        assert_eq!(
            resolve_provider_selector(&saved, "CODEX:shared"),
            Some(("codex".into(), "shared@example.com".into()))
        );
        assert_eq!(resolve_provider_selector(&saved, "unknown:shared"), None);
    });
}

#[test]
fn switch_ambiguous_claude_prefix_does_not_fall_through_to_provider() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    for email in ["dev1@e.com", "dev2@e.com"] {
        let mut a = acct_with_cache(None);
        a.email = Some(email.to_string());
        state.upsert(a);
    }
    state.upsert_provider_account("codex", codex_account("dev3@e.com", None));
    state.save().unwrap();
    let err = cmd_switch(Some("dev"), None).unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
}

#[test]
fn switch_wait_requires_the_target_to_be_active() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    state.upsert_provider_account("codex", codex_account("a@e.com", None));
    state.upsert_provider_account("codex", codex_account("b@e.com", None));
    state.provider_accounts_mut("codex").active = Some("b@e.com".into());
    state.save().unwrap();
    assert!(ensure_switch_landed("codex", "a@e.com", "a@e.com").is_err());
    assert!(ensure_switch_landed("codex", "B@e.com", "b@e.com").is_ok());
}

#[test]
fn cli_wait_loop_does_not_replay_its_pick_over_a_newer_queued_one() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let (a, b) = ("a@e.com", "b@e.com");
        let fresh = |key: &str| {
            let mut acct = codex_account(key, None);
            acct.expires_at = Utc::now().timestamp() + 86_400;
            acct
        };
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
        let mut state = State::default();
        state.upsert_provider_account("codex", fresh(a));
        state.upsert_provider_account("codex", fresh(b));
        state.provider_accounts_mut("codex").pending_switch = Some(pending(a));
        state.save().unwrap();
        // While the CLI sleeps, the user picks B in the menu.
        let _ = wait_for_queued_provider_switch("codex", a, true, |_| {
            let mut st = State::load().unwrap();
            st.provider_accounts_mut("codex").pending_switch = Some(pending(b));
            st.save().unwrap();
        });
        let after = State::load().unwrap();
        let now = after.provider_accounts("codex").unwrap();
        assert_ne!(
            now.active.as_deref(),
            Some(a),
            "stale CLI pick replaced the newer one"
        );
        assert_eq!(now.pending_switch.as_ref().map(|p| p.key.as_str()), Some(b));
    });
}
