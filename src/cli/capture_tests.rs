use super::*;
use crate::lifecycle::{refresh_provider_active_account, ActiveRefreshOutcome};
use crate::providers::trait_def::LoginFlow;
use crate::providers::trait_def::TokenGrant;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::rows::claude_plan_label;
use crate::store::{Account, CachedUsage, State};
use crate::test_support::locked_active_refresh_cas;
use crate::test_support::{
    acct_with_cache, claude_account, codex_account, codex_auth_json, codex_login_response,
    hit_count, mock_claude_blob, prime_active_slot, spawn_mock_token_server,
    spawn_mock_usage_server, with_claude_home, MockActiveSlotProvider,
};
use crate::{lifecycle, providers, store};

#[test]
fn merged_cached_usage_prefers_existing_snapshot() {
    let existing = acct_with_cache(Some(CachedUsage {
        session_pct: Some(42.0),
        weekly_pct: Some(61.0),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: 123,
        ..Default::default()
    }));
    let merged = merged_cached_usage(Some(&existing));
    assert_eq!(merged.unwrap().session_pct, Some(42.0));
}

#[test]
fn merged_cached_usage_none_for_new_account() {
    assert!(merged_cached_usage(None).is_none());
}

#[test]
fn capture_current_generic_persists_a_codex_account_into_state_v2() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("codex-user@example.com", 4_000_000_000, "at1", "rt1");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();

        let (key, existed) = capture_current_generic("codex").expect("capture must succeed");
        assert_eq!(key, "codex-user@example.com");
        assert!(
            !existed,
            "first capture of this account must report existed=false"
        );

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "codex-user@example.com")
            .expect("captured account must be persisted");
        assert_eq!(acct.access_token, "at1");
        assert_eq!(acct.refresh_token, "rt1");
        assert_eq!(acct.secret_blob, blob);
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("codex-user@example.com"),
            "capture must also become the active account for its provider"
        );

        // Re-capturing the same account (same auth.json) reports existed=true
        // and doesn't lose the identity.
        let (key2, existed2) = capture_current_generic("codex").unwrap();
        assert_eq!(key2, "codex-user@example.com");
        assert!(existed2);
    });
}

#[test]
fn refresh_provider_active_account_codex_fresh_token_is_a_noop() {
    // The stored blob still matches the slot on disk (the Codex CLI hasn't
    // rotated it), so the active-account follower reports `Fresh` — no network
    // call is made, and the account must be byte-for-byte unchanged.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("fresh@example.com", 4_000_000_000, "fresh-at", "fresh-rt");
        std::fs::write(dir.path().join("auth.json"), &blob).unwrap();
        capture_current_generic("codex").unwrap();

        refresh_provider_active_account("codex");

        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "fresh@example.com")
            .unwrap();
        assert_eq!(acct.access_token, "fresh-at");
        assert_eq!(acct.secret_blob, blob);
    });
}

#[test]
fn refresh_provider_active_account_codex_adopts_cli_rotation_without_posting() {
    // End-to-end for the NEVER-POST active-account contract: usagio captured
    // one blob, then the Codex CLI rotated `auth.json` on its own cadence.
    // `refresh_provider_active_account` must ADOPT the CLI's new blob into
    // `state.providers["codex"]` WITHOUT ever POSTing `/token` — a usagio POST
    // for the active account would consume the single-use refresh token and
    // force a `codex login`. We prove zero POSTs by pointing the token-URL
    // override at a mock server and asserting its hit counter stays 0.
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().to_str().unwrap().to_string();

    // `scoped_env_var` isn't reentrant, so both env vars are set inside one
    // call (mirrors `providers::codex::oauth_tests::with_codex_env`, which
    // this file's edit scope doesn't extend to import from).
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(&codex_home), || {
        // usagio's last-observed blob (captured into state).
        let captured_blob = codex_auth_json("user@example.com", 1, "old-at", "old-rt");
        std::fs::write(dir.path().join("auth.json"), &captured_blob).unwrap();
        capture_current_generic("codex").unwrap();

        // The Codex CLI now rotates auth.json to a fresh grant behind our back.
        let cli_rotated = codex_auth_json(
            "user@example.com",
            4_000_000_000,
            "cli-rotated-at",
            "cli-rotated-rt",
        );
        std::fs::write(dir.path().join("auth.json"), &cli_rotated).unwrap();

        let (base_url, hits) = spawn_mock_token_server();
        #[allow(clippy::disallowed_methods)]
        let prev_url = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)]
        std::env::set_var(
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            format!("{base_url}/v1/oauth/token"),
        );

        refresh_provider_active_account("codex");

        #[allow(clippy::disallowed_methods)]
        match prev_url {
            Some(v) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", v),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }

        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "active-account refresh must NEVER POST /token"
        );

        // State adopted the CLI's rotated grant verbatim.
        let state = State::load().unwrap();
        let acct = state
            .find_provider_account("codex", "user@example.com")
            .expect("account survives the adopt");
        assert_eq!(acct.access_token, "cli-rotated-at");
        assert_eq!(acct.secret_blob, cli_rotated);

        // usagio never wrote auth.json — the CLI's blob stands untouched.
        let on_disk = std::fs::read_to_string(dir.path().join("auth.json")).unwrap();
        assert_eq!(on_disk, cli_rotated);
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn capturing_the_claude_login_stores_it_as_the_active_account() {
    with_claude_home("cap@e.com", |_| {
        let blob = claude_account("cap@e.com", 3_600_000).keychain_blob;
        prime_active_slot(&blob);
        let (email, existed) = capture_current().unwrap();
        assert_eq!(email, "cap@e.com");
        assert!(!existed);
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("cap@e.com"));
        let a = st.find("cap@e.com").unwrap();
        assert_eq!(a.access_token, "at-cap@e.com");
        assert!(a.oauth_account.is_some(), "the identity travels with it");
        assert_eq!(a.user_id.as_deref(), Some("user-1"));
        // Capturing again refreshes rather than duplicating.
        let (_, existed) = capture_current().unwrap();
        assert!(existed);
        assert_eq!(State::load().unwrap().accounts.len(), 1);
        // The CLI entry point does the same for Claude with no argument.
        cmd_capture(&[]).unwrap();
        assert_eq!(State::load().unwrap().accounts.len(), 1);
        assert!(!State::load().unwrap().providers.contains_key("claude"));
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn capture_refuses_a_token_whose_profile_email_differs_from_claude_json() {
    with_claude_home("json@e.com", |_| {
        let blob = claude_account("json@e.com", 3_600_000).keychain_blob;
        prime_active_slot(&blob);
        let (url, _) = crate::test_support::spawn_mock_usage_server(
            200,
            r#"{"account":{"email":"someone-else@e.com"}}"#.into(),
        );
        crate::endpoints::set_test_override(
            crate::endpoints::Endpoint::ClaudeProfile,
            Some(&format!("{url}/api/oauth/profile")),
        );
        let res = capture_current();
        crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeProfile, None);
        let err = res.expect_err("mismatched identity must not be captured");
        assert!(format!("{err:#}").contains("someone-else@e.com"), "{err:#}");
        assert!(State::load().unwrap().accounts.is_empty());
    });
}

#[test]
fn the_capture_command_routes_other_providers_to_their_own_slot() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let blob = codex_auth_json("cx@example.com", 4_000_000_000, "at", "rt");
        std::fs::write(dir.path().join("auth.json"), blob).unwrap();
        cmd_capture(&["codex".to_string()]).unwrap();
        let st = State::load().unwrap();
        let acct = st.find_provider_account("codex", "cx@example.com").unwrap();
        // Expiry is the id token's own (4e9), not some other sum.
        assert!(
            (acct.expires_at - 4_000_000_000).abs() < 30,
            "{}",
            acct.expires_at
        );
        assert!(st.accounts.is_empty());
        assert!(cmd_capture(&["nope".to_string()]).is_err());
    });
}

#[test]
fn adopting_a_codex_rotation_records_the_new_expiry() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let stale = codex_auth_json("exp@example.com", 1, "old-at", "old-rt");
        std::fs::write(dir.path().join("auth.json"), &stale).unwrap();
        capture_current_generic("codex").unwrap();
        let rotated = codex_auth_json("exp@example.com", 4_000_000_000, "new-at", "new-rt");
        std::fs::write(dir.path().join("auth.json"), &rotated).unwrap();
        refresh_provider_active_account("codex");
        let st = State::load().unwrap();
        let a = st
            .find_provider_account("codex", "exp@example.com")
            .unwrap();
        assert_eq!(a.access_token, "new-at");
        assert!(
            (a.expires_at - 4_000_000_000).abs() < 30,
            "{}",
            a.expires_at
        );
    });
}

fn renewed_capture(
    email: &str,
    at: &str,
    rt_expires_at: i64,
) -> providers::trait_def::CapturedAccount {
    providers::trait_def::CapturedAccount {
        identity: providers::trait_def::IdentitySnapshot {
            email: Some(email.to_string()),
            uuid: Some("from-login".into()),
            display_name: None,
            native_blob: serde_json::json!({
                "oauthAccount": { "emailAddress": email, "accountUuid": "from-login" },
                "userID": null,
            }),
        },
        secret_blob: serde_json::json!({ "claudeAiOauth": {
            "accessToken": at, "refreshToken": "new-rt", "expiresAt": 9,
            "refreshTokenExpiresAt": rt_expires_at,
        }})
        .to_string(),
        tokens: TokenGrant {
            access: at.to_string(),
            refresh: Some("new-rt".into()),
            expires_in_secs: 60,
        },
    }
}

#[test]
fn persist_login_renews_an_existing_claude_account_without_switching() {
    use crate::store::ScopedConfigDir;
    let _g = ScopedConfigDir::new();
    let mut old = Account::from_keychain_blob(
        &serde_json::json!({ "claudeAiOauth": {
            "accessToken": "old-at", "refreshToken": "old-rt", "expiresAt": 1, "clientId": "kept",
        }})
        .to_string(),
    )
    .unwrap();
    old.email = Some("dev@example.com".into());
    old.needs_relogin = true;
    old.oauth_account =
        Some(serde_json::json!({ "emailAddress": "dev@example.com", "accountUuid": "original" }));
    let mut other = old.clone();
    other.email = Some("other@example.com".into());
    let st = State {
        accounts: vec![old, other],
        active: Some("other@example.com".into()),
        ..Default::default()
    };
    st.save().unwrap();

    let provider = providers::claude::new();
    let mut captured = renewed_capture("dev@example.com", "new-at", 77);
    captured.identity.native_blob["oauthAccount"]["planDisplayName"] = "Ultra".into();
    let r = persist_login(provider.as_ref(), captured, None).unwrap();

    assert_eq!(r.key, "dev@example.com");
    assert!(
        !r.made_live,
        "an inactive account isn't written to the live slot"
    );
    assert_eq!(r.login_expires_at, Some(77));
    let st = State::load().unwrap();
    assert_eq!(
        st.active.as_deref(),
        Some("other@example.com"),
        "renew never switches"
    );
    let a = st.find("dev@example.com").unwrap();
    assert_eq!(a.access_token, "new-at");
    assert!(!a.needs_relogin);
    assert_eq!(a.refresh_token_expires_at(), Some(77));
    // Renewed identity metadata replaces the old snapshot along with the login.
    assert_eq!(
        a.oauth_account.as_ref().unwrap()["accountUuid"],
        "from-login"
    );
    assert_eq!(
        a.oauth_account.as_ref().unwrap()["planDisplayName"],
        "Ultra"
    );
    assert_eq!(claude_plan_label(a).as_deref(), Some("Ultra"));
    assert!(a.keychain_blob.contains("\"clientId\":\"kept\""));
}

#[test]
fn persist_login_adds_an_account_it_has_never_seen() {
    use crate::store::ScopedConfigDir;
    let _g = ScopedConfigDir::new();
    State::default().save().unwrap();
    let provider = providers::claude::new();
    persist_login(
        provider.as_ref(),
        renewed_capture("new@example.com", "at", 5),
        Some("new:abc"),
    )
    .unwrap();
    let st = State::load().unwrap();
    let a = st.find("new@example.com").expect("added");
    assert_eq!(
        a.oauth_account.as_ref().unwrap()["accountUuid"],
        "from-login"
    );
    assert_eq!(
        a.login_store.as_deref(),
        Some("new:abc"),
        "renewals reuse that store"
    );
    assert_eq!(st.active, None);
}

#[test]
fn codex_inactive_refresh_persists_complete_credential_without_touching_active_file() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let a = codex_auth_json("a@example.com", 4_000_000_000, "at-a", "rt-a");
    let old = codex_auth_json("b@example.com", 1, "old-at-b", "old-rt-b");
    let fresh = codex_auth_json("b@example.com", 4_000_000_001, "new-at-b", "new-rt-b");
    let fresh_value: serde_json::Value = serde_json::from_str(&fresh).unwrap();
    let (url, hits) = spawn_mock_usage_server(200, fresh_value["tokens"].to_string());
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        std::fs::write(dir.path().join("auth.json"), &old).unwrap();
        capture_current_generic("codex").unwrap();
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        capture_current_generic("codex").unwrap();
        // scoped_env_var takes the global environment lock; set the nested
        // endpoint while we already own that lock and restore it on exit.
        let previous = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)] // ENV_LOCK is held by the enclosing scope.
        std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", &url);
        let result =
            lifecycle::ensure_provider_fresh(&providers::codex::CodexProvider, "b@example.com");
        #[allow(clippy::disallowed_methods)] // Restore before inspecting the result.
        match previous {
            Some(value) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", value),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }
        let account = result.unwrap();
        let saved: serde_json::Value = serde_json::from_str(&account.secret_blob).unwrap();
        assert_eq!(saved["tokens"], fresh_value["tokens"]);
        assert_eq!(account.access_token, "new-at-b");
        assert_eq!(account.refresh_token, "new-rt-b");
        // Opaque access token: due 8 days after this refresh (the Codex
        // client's cadence), not at the id_token's exp.
        let due = chrono::Utc::now().timestamp() + 8 * 86_400;
        assert!(
            (due - 120..=due).contains(&account.expires_at),
            "{}",
            account.expires_at
        );
        assert_eq!(hit_count(&hits), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            a
        );
        assert_eq!(
            State::load()
                .unwrap()
                .find_provider_account("codex", "b@example.com")
                .unwrap()
                .secret_blob,
            account.secret_blob
        );
    });
}

#[test]
fn codex_vendor_owned_account_is_never_refreshed_when_saved_selection_lags() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    let (url, hits) = spawn_mock_usage_server(500, String::new());
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let a = codex_auth_json("a@example.com", 1, "vendor-at", "vendor-rt");
        let b = codex_auth_json("b@example.com", 4_000_000_000, "b-at", "b-rt");
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        capture_current_generic("codex").unwrap();
        std::fs::write(dir.path().join("auth.json"), &b).unwrap();
        capture_current_generic("codex").unwrap();
        // The client logged into A, but usagio's last selection is still B.
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        let previous = std::env::var_os("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
        #[allow(clippy::disallowed_methods)] // Enclosing scope owns ENV_LOCK.
        std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", &url);
        let result =
            lifecycle::ensure_provider_fresh(&providers::codex::CodexProvider, "a@example.com");
        #[allow(clippy::disallowed_methods)]
        match previous {
            Some(value) => std::env::set_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE", value),
            None => std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
        }
        let account = result.unwrap();
        assert_eq!(hit_count(&hits), 0, "the vendor owns A's refresh token");
        assert_eq!(account.secret_blob, a);
        assert_eq!(account.access_token, "vendor-at");
        let state = State::load().unwrap();
        assert_eq!(
            state
                .find_provider_account("codex", "b@example.com")
                .unwrap()
                .secret_blob,
            b
        );
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("b@example.com")
        );
    });
}

#[test]
fn codex_renew_repairs_only_the_target_account_and_preserves_active_login() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let provider = providers::codex::CodexProvider;
        let a = codex_auth_json("a@example.com", 4_000_000_000, "a-at", "a-rt");
        let b = codex_auth_json("b@example.com", 4_000_000_000, "b-at", "b-rt");
        std::fs::write(dir.path().join("auth.json"), b).unwrap();
        capture_current_generic("codex").unwrap();
        std::fs::write(dir.path().join("auth.json"), &a).unwrap();
        capture_current_generic("codex").unwrap();
        let mut state = State::load().unwrap();
        let broken = state
            .find_provider_account_mut("codex", "b@example.com")
            .unwrap();
        broken.secret_blob = a.clone();
        broken.needs_relogin = true;
        state.save().unwrap();
        let captured = provider
            .complete_login(&codex_login_response("b@example.com", "account-b"))
            .unwrap();
        let result = persist_login(&provider, captured, None).unwrap();
        assert!(!result.made_live);
        let state = State::load().unwrap();
        // The email-only entry (no workspace on record) is upgraded by the
        // renewal to its workspace key.
        assert!(state
            .find_provider_account("codex", "b@example.com")
            .is_none());
        let repaired = state
            .find_provider_account("codex", "b@example.com#account-b")
            .unwrap();
        assert!(!repaired.needs_relogin);
        lifecycle::validate_stored_identity(&provider, repaired).unwrap();
        assert_eq!(repaired.access_token, "renewed-at");
        assert_eq!(
            state
                .find_provider_account("codex", "a@example.com")
                .unwrap()
                .secret_blob,
            a
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            a
        );
        assert_eq!(
            state.provider_accounts("codex").unwrap().active.as_deref(),
            Some("a@example.com")
        );
    });
}

#[test]
fn renewing_active_account_keeps_a_queued_switch_to_another_account() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("outgoing");
    let slug = provider.provider_id();
    let k = "active@example.com";
    let mut state = State::default();
    state
        .provider_accounts_mut(slug)
        .accounts
        .push(codex_account(k, None));
    state
        .provider_accounts_mut(slug)
        .accounts
        .push(codex_account("other@example.com", None));
    state.provider_accounts_mut(slug).active = Some(k.into());
    state.provider_accounts_mut(slug).pending_switch = Some(store::PendingSwitch {
        renewal: false,
        key: "other@example.com".into(),
        manual: true,
        from: Some(k.into()),
        drain_token: Some("drain".into()),
        credentials_written: false,
        renewed_account: None,
        leave_latch: None,
    });
    state.save().unwrap();
    let captured = providers::trait_def::CapturedAccount {
        identity: providers::IdentitySnapshot {
            email: Some(k.into()),
            uuid: None,
            display_name: None,
            native_blob: serde_json::Value::Null,
        },
        secret_blob: "renewed".into(),
        tokens: providers::trait_def::TokenGrant {
            access: "renewed-at".into(),
            refresh: Some("renewed-rt".into()),
            expires_in_secs: 3600,
        },
    };
    let r = persist_login(&provider, captured, None).unwrap();
    assert!(!r.made_live);
    let state = State::load().unwrap();
    let pa = state.provider_accounts(slug).unwrap();
    let pending = pa.pending_switch.as_ref().expect("queued switch dropped");
    assert_eq!(pending.key, "other@example.com");
    assert_eq!(
        state.find_provider_account(slug, k).unwrap().access_token,
        "renewed-at"
    );
}

#[test]
fn active_login_renewal_does_not_become_a_manual_pick() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("old-live-login");
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
    assert!(result.made_live);
    let after = State::load().unwrap();
    assert_eq!(after.manual_pick(slug), None, "renewal recorded a pick");
    assert_eq!(after.manual_hold(slug), None, "renewal recorded a hold");
    assert!(after
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .is_none());
}

#[test]
fn renewing_a_queued_switch_target_forces_the_credential_rewrite() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let provider = MockActiveSlotProvider::new("live");
    let slug = provider.provider_id();
    let (a, b) = ("a@example.com", "b@example.com");
    let mut state = State::default();
    state.upsert_provider_account(slug, codex_account(a, None));
    state.upsert_provider_account(slug, codex_account(b, None));
    state.provider_accounts_mut(slug).active = Some(a.into());
    state.provider_accounts_mut(slug).pending_switch = Some(crate::store::PendingSwitch {
        renewal: false,
        key: b.into(),
        manual: true,
        from: Some(a.into()),
        drain_token: None,
        credentials_written: true,
        leave_latch: None,
        renewed_account: None,
    });
    state.save().unwrap();
    let captured = providers::codex::CodexProvider
        .complete_login(&codex_login_response(b, "account-b"))
        .unwrap();
    persist_login(&provider, captured, None).unwrap();
    let pending = State::load()
        .unwrap()
        .provider_accounts(slug)
        .unwrap()
        .pending_switch
        .clone()
        .unwrap();
    assert!(
        !pending.credentials_written,
        "renewed grant would never be written"
    );
}

/// A Claude-slug provider whose live credential write always fails.
struct FailingClaudeSlot;

impl crate::providers::Provider for FailingClaudeSlot {
    fn provider_id(&self) -> &'static str {
        CLAUDE_SLUG
    }
    fn display_name(&self) -> &'static str {
        "Failing Claude slot"
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

    fn active_slot(&self) -> Option<&dyn crate::providers::ActiveSlot> {
        Some(self)
    }
}

impl crate::providers::LoginCapture for FailingClaudeSlot {
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
}

impl crate::providers::ActiveSlot for FailingClaudeSlot {
    fn write_active_account(
        &self,
        _blob: &str,
        _identity: &providers::IdentitySnapshot,
    ) -> providers::PResult<()> {
        Err(providers::ProviderError::Other(
            "keychain write refused".into(),
        ))
    }

    fn read_active_identity(
        &self,
    ) -> crate::providers::PResult<Option<crate::providers::IdentitySnapshot>> {
        Err(crate::providers::ProviderError::Unsupported)
    }

    fn read_active_slot(&self) -> crate::providers::PResult<Option<String>> {
        Err(crate::providers::ProviderError::Unsupported)
    }
}

#[test]
fn failed_live_write_of_a_claude_renewal_sets_the_marker_the_poll_republishes() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let key = "active@example.com";
    let old_blob = mock_claude_blob("old-at", "old-rt", 1_000);
    let mut old = Account::from_keychain_blob(&old_blob).unwrap();
    old.email = Some(key.into());
    let mut state = State::default();
    state.upsert(old);
    state.active = Some(key.into());
    state.save().unwrap();

    let new_blob = mock_claude_blob("new-at", "new-rt", 2_000);
    let captured = crate::providers::trait_def::CapturedAccount {
        identity: providers::IdentitySnapshot {
            email: Some(key.into()),
            uuid: None,
            display_name: None,
            native_blob: serde_json::Value::Null,
        },
        secret_blob: new_blob,
        tokens: crate::providers::trait_def::TokenGrant {
            access: "new-at".into(),
            refresh: Some("new-rt".into()),
            expires_in_secs: 3_600,
        },
    };
    let err = persist_login(&FailingClaudeSlot, captured, None).unwrap_err();
    assert!(format!("{err}").contains("making it live failed"), "{err}");

    let saved = State::load().unwrap();
    let a = saved.find(key).unwrap();
    assert_eq!(a.refresh_token, "new-rt", "the renewal was saved");
    assert_eq!(
        a.live_write_pending.as_deref(),
        Some(crate::logging::token_fingerprint("new-rt").as_str()),
        "the failed live write left no marker for the renewed grant"
    );

    // The next poll's CAS republishes the renewal over the old slot.
    let slot = MockActiveSlotProvider::new(&old_blob);
    let mut acct = a.clone();
    let outcome = locked_active_refresh_cas(&slot, &mut acct);
    assert!(
        matches!(outcome, ActiveRefreshOutcome::Adopted),
        "{outcome:?}"
    );
    assert_eq!(
        acct.refresh_token, "new-rt",
        "old slot adopted over renewal"
    );
    let live = Account::from_keychain_blob(&slot.slot.lock().unwrap().clone().unwrap()).unwrap();
    assert_eq!(live.refresh_token, "new-rt");
}
