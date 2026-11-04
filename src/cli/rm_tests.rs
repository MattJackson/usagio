use super::*;
use crate::store::State;
use crate::test_support::{claude_account, codex_account};

#[test]
fn rm_removes_a_captured_codex_account_by_qualified_or_bare_selector() {
    let _cfg = crate::store::ScopedConfigDir::new();
    for sel in ["codex:dev@example.com", "dev", "codex:dev"] {
        let mut state = State::default();
        state
            .provider_accounts_mut("codex")
            .accounts
            .push(codex_account("dev@example.com", None));
        state.save().unwrap();
        cmd_rm(Some(sel)).unwrap_or_else(|e| panic!("rm {sel}: {e:#}"));
        assert!(State::load()
            .unwrap()
            .provider_accounts("codex")
            .is_none_or(|p| p.accounts.is_empty()));
    }
}

#[test]
fn rm_with_ambiguous_claude_selector_never_deletes_a_provider_account() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    state.accounts.push(claude_account("dev1@a.com", 3_600_000));
    state.accounts.push(claude_account("dev2@a.com", 3_600_000));
    state
        .provider_accounts_mut("codex")
        .accounts
        .push(codex_account("dev@x.com", None));
    state.save().unwrap();
    let err = cmd_rm(Some("dev")).unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
    let state = State::load().unwrap();
    assert_eq!(state.accounts.len(), 2);
    assert_eq!(state.provider_accounts("codex").unwrap().accounts.len(), 1);
}

#[test]
fn rm_refuses_a_selector_matching_two_providers() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    state.accounts.push(claude_account("dev@a.com", 3_600_000));
    state
        .provider_accounts_mut("codex")
        .accounts
        .push(codex_account("dev@a.com", None));
    state.save().unwrap();
    assert!(cmd_rm(Some("dev@a.com")).is_err());
    assert_eq!(State::load().unwrap().accounts.len(), 1);
    cmd_rm(Some("codex:dev@a.com")).unwrap();
    // The qualified selector removed the Codex account and only that one.
    let after = State::load().unwrap();
    assert_eq!(after.accounts.len(), 1, "the Claude dev@a.com survives");
    assert_eq!(after.accounts[0].email.as_deref(), Some("dev@a.com"));
    assert!(after
        .provider_accounts("codex")
        .is_none_or(|p| p.accounts.is_empty()));
}

#[test]
fn rm_refuses_a_prefix_shared_by_two_codex_accounts() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut state = State::default();
    let codex = state.provider_accounts_mut("codex");
    codex.accounts.push(codex_account("dev@example.com", None));
    codex.accounts.push(codex_account("dev2@example.com", None));
    state.save().unwrap();
    let err = cmd_rm(Some("dev")).unwrap_err();
    assert!(format!("{err}").contains("more than one"), "{err}");
    let err = cmd_rm(Some("codex:dev")).unwrap_err();
    assert!(
        format!("{err}").contains("no unique codex account"),
        "{err}"
    );
    let after = State::load().unwrap();
    assert_eq!(
        after.provider_accounts("codex").map(|p| p.accounts.len()),
        Some(2),
        "an ambiguous selector deleted nothing"
    );
}
