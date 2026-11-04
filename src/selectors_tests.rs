use super::*;
use crate::store::State;
use crate::test_support::{claude_account, codex_account, usage_at};

#[test]
fn resolve_provider_selector_matches_exact_and_unique_prefix() {
    let mut state = State::default();
    state.upsert_provider_account(
        "codex",
        crate::store::ProviderAccount {
            key: "dev@example.com".into(),
            secret_blob: "{}".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            identity_email: Some("dev@example.com".into()),
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

    assert_eq!(
        resolve_provider_selector(&state, "dev@example.com"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(
        resolve_provider_selector(&state, "dev"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(resolve_provider_selector(&state, "nobody"), None);
}

#[test]
fn select_email_resolves_a_selector_or_auto_picks_without_one() {
    let mut st = State::default();
    let mut busy = claude_account("busy@e.com", 3_600_000);
    busy.cached_usage = Some(usage_at(80.0));
    let mut calm = claude_account("calm@e.com", 3_600_000);
    calm.cached_usage = Some(usage_at(10.0));
    st.upsert(busy);
    st.upsert(calm);
    assert_eq!(select_email(&st, None).unwrap(), "calm@e.com");
    assert_eq!(select_email(&st, Some("busy")).unwrap(), "busy@e.com");
    assert!(select_email(&st, Some("nobody")).is_err());
}

#[test]
fn unqualified_selector_matching_several_providers_must_be_qualified() {
    let mut state = State::default();
    state.upsert(claude_account("dev@a.com", 3_600_000));
    state.upsert_provider_account("codex", codex_account("dev@b.com", None));
    state.upsert_provider_account("codex", codex_account("dev@c.com", None));
    // `rm dev` matched three accounts in two providers.
    let err = resolve_account_selector(&state, "dev").unwrap_err();
    assert!(err.to_string().contains("qualify"), "{err}");
    // No Claude match, two Codex matches: ambiguous, not "no account".
    let mut codex_only = State::default();
    codex_only.upsert_provider_account("codex", codex_account("dev@b.com", None));
    codex_only.upsert_provider_account("codex", codex_account("dev@c.com", None));
    let err = resolve_account_selector(&codex_only, "dev").unwrap_err();
    assert!(err.to_string().contains("more than one"), "{err}");
}

#[test]
fn provider_selector_refuses_a_shared_prefix_and_a_key_in_two_providers() {
    // Two Codex accounts share the prefix "dev": a bare "dev" is ambiguous and
    // must not resolve to whichever comes first. The exact key still wins.
    let mut state = State::default();
    let codex = state.provider_accounts_mut("codex");
    codex.accounts.push(codex_account("dev@example.com", None));
    codex.accounts.push(codex_account("dev2@example.com", None));
    assert_eq!(resolve_provider_selector(&state, "dev"), None);
    assert_eq!(
        resolve_provider_selector(&state, "dev@example.com"),
        Some(("codex".to_string(), "dev@example.com".to_string()))
    );
    assert_eq!(
        resolve_provider_selector(&state, "dev2"),
        Some(("codex".to_string(), "dev2@example.com".to_string()))
    );

    // The same key under two providers is ambiguous too.
    let mut two = State::default();
    two.provider_accounts_mut("codex")
        .accounts
        .push(codex_account("same@example.com", None));
    two.provider_accounts_mut("opencode")
        .accounts
        .push(codex_account("same@example.com", None));
    assert_eq!(resolve_provider_selector(&two, "same@example.com"), None);
    assert_eq!(resolve_provider_selector(&two, "same"), None);
}
