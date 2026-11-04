use super::*;
use crate::claude_local::keychain_read;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::store::{Account, State};
use crate::switching::provider::switch_if_still_active;
use crate::test_support::{
    claude_account, claude_account_with_identity, hit_count, mock_claude_blob, prime_active_slot,
    seed_claude_state, spawn_mock_token_server, usage_at, with_claude_home, MockActiveSlotProvider,
};
use chrono::Utc;

// usagio must NEVER mint a token for the active account — Anthropic's refresh
// tokens are single-use, so a usagio POST would rotate the family server-side
// and invalidate the copy Claude Code holds (→ /login). The active-account
// path (`active_refresh_cas`) is therefore a pure adopt-from-slot, exercised in
// isolation above against `MockActiveSlotProvider` so no test here ever touches
// the real OS keychain that `refresh_usage_cache` would resolve
// `ClaudeProvider::read_active_slot()` to.

// --- apply_account keychain-write-failure rollback (v0.5.17 codeaudit) --------
//
// The core "switching ALWAYS works, never a half-applied switch" guard:
// apply_account writes ~/.claude.json FIRST, then the keychain LAST as the
// commit point, and rolls ~/.claude.json back (returning Err) if the keychain
// write fails. Before v0.5.17 the cfg(test) keychain mock always succeeded and
// no failure-injection seam existed, so this rollback branch had ZERO coverage
// — a regression dropping the rollback (leaving ~/.claude.json on the new
// account while the keychain still holds the old) would have passed the suite.
// Runs on every platform: the failure seam now lives in platform/mod.rs and is
// honored by all three SecretStore::set impls (macOS mock, Linux keyring, Windows
// Credential Manager), so the rollback guarantee is enforced in CI everywhere.
#[test]
fn apply_account_rolls_back_claude_json_when_keychain_write_fails() {
    use crate::store::{Account, ScopedConfigDir};

    // ScopedConfigDir isolates config_dir (logging::log resolves it); the HOME
    // override points ~/.claude.json (claude_json_path uses $HOME) at the same
    // tempdir. env_lock serializes the $HOME mutation against other tests.
    let scd = ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let claude_json = home.join(".claude.json");
        let prior = r#"{"oauthAccount":{"emailAddress":"old@example.com"},"other":"keep"}"#;
        std::fs::write(&claude_json, prior).unwrap();

        let acct = Account::from_keychain_blob(
            r#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"r","expiresAt":0}}"#,
        )
        .unwrap();
        let identity = serde_json::json!({
            "oauthAccount": { "emailAddress": "new@example.com" }
        });

        // Arm the mock keychain to fail the (single) set apply_account performs.
        crate::platform::arm_keychain_set_failure();

        let provider = crate::providers::get("claude").expect("claude provider registered");
        let res = apply_account(
            provider,
            &acct,
            &identity,
            Some("old@example.com"),
            "new@example.com",
        );

        assert!(
            res.is_err(),
            "a keychain write failure must fail the switch, never report success"
        );
        let after = std::fs::read_to_string(&claude_json).unwrap();
        // Rollback re-patches the prior identity (so Claude Code's own writes
        // survive), hence compare the JSON value rather than the exact bytes.
        let after: serde_json::Value = serde_json::from_str(&after).unwrap();
        let prior: serde_json::Value = serde_json::from_str(prior).unwrap();
        assert_eq!(
            after, prior,
            "~/.claude.json must be rolled back to its prior contents on keychain failure \
             (no half-applied switch)"
        );
    });
}

#[test]
fn flag_needs_relogin_marks_only_that_account() {
    let _g = crate::store::ScopedConfigDir::new();
    seed_claude_state(
        vec![
            claude_account("dead@e.com", 3_600_000),
            claude_account("fine@e.com", 3_600_000),
        ],
        None,
    );
    flag_needs_relogin("dead@e.com", "rt-dead@e.com");
    let st = State::load().unwrap();
    assert!(st.find("dead@e.com").unwrap().needs_relogin);
    assert!(!st.find("fine@e.com").unwrap().needs_relogin);
}

#[test]
fn flag_needs_relogin_spares_an_account_whose_grant_was_replaced() {
    // The rejected grant is not the one saved now (a renewal or rotation
    // landed meanwhile): the saved login is healthy and must stay unflagged.
    let _g = crate::store::ScopedConfigDir::new();
    seed_claude_state(vec![claude_account("renewed@e.com", 3_600_000)], None);
    flag_needs_relogin("renewed@e.com", "an-older-rt");
    let st = State::load().unwrap();
    assert!(!st.find("renewed@e.com").unwrap().needs_relogin);
}

/// The vendor-visible active token. On macOS that is the keychain; elsewhere
/// it is `$HOME/.claude/.credentials.json`, read DIRECTLY from disk (not via
/// `active_slot_read`, which would be tautological with the writer).
fn keychain_access_token() -> Option<String> {
    let blob = if crate::platform::current().os_display_name() == "macOS" {
        keychain_read()
    } else {
        let home = std::env::var_os("HOME").expect("HOME is scoped by with_claude_home");
        std::fs::read_to_string(
            std::path::PathBuf::from(home)
                .join(".claude")
                .join(".credentials.json"),
        )
        .ok()
    };
    blob.map(|b| Account::from_keychain_blob(&b).unwrap().access_token)
}

#[test]
fn apply_account_writes_the_slot_claude_code_reads() {
    with_claude_home("old@e.com", |_| {
        let acct = claude_account_with_identity("new@e.com", 1_000);
        let identity = acct.oauth_account.clone().unwrap();
        apply_account(
            providers::get(CLAUDE_SLUG).unwrap(),
            &acct,
            &identity,
            Some("old@e.com"),
            "new@e.com",
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-new@e.com"));
    });
}

#[test]
fn resolve_identity_prefers_the_stored_identity_without_the_network() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let acct = claude_account_with_identity("id@e.com", 3_600_000);
    let (identity, backfilled) = resolve_identity(provider, &acct).unwrap();
    assert_eq!(
        identity.get("emailAddress").and_then(|v| v.as_str()),
        Some("id@e.com")
    );
    assert!(!backfilled);
}

#[test]
fn ensure_fresh_with_fallback_reports_whether_it_refreshed() {
    let _g = crate::store::ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_token_server();
    crate::endpoints::set_test_override(
        crate::endpoints::Endpoint::ClaudeToken,
        Some(&format!("{base_url}/v1/oauth/token")),
    );
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let mut fresh = claude_account("f@e.com", 3_600_000);
    let r1 = ensure_fresh_with_fallback(provider, "f@e.com", &mut fresh);
    let after_fresh = hit_count(&hits);
    let mut expired = claude_account("x@e.com", -60_000);
    let r2 = ensure_fresh_with_fallback(provider, "x@e.com", &mut expired);
    crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeToken, None);
    assert!(matches!(r1, Ok(false)));
    assert_eq!(after_fresh, 0);
    assert!(matches!(r2, Ok(true)));
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(expired.access_token, "mock-refreshed-access-token");
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn switching_refreshes_an_inactive_account_but_adopts_for_the_active_one() {
    with_claude_home("a@e.com", |_home| {
        let (base_url, hits) = spawn_mock_token_server();
        crate::endpoints::set_test_override(
            crate::endpoints::Endpoint::ClaudeToken,
            Some(&format!("{base_url}/v1/oauth/token")),
        );
        let a = claude_account_with_identity("a@e.com", -60_000);
        let b = claude_account_with_identity("b@e.com", -60_000);
        prime_active_slot(&a.keychain_blob);
        seed_claude_state(vec![a, b], Some("a@e.com"));

        // b is inactive and expired: usagio owns its token, so it refreshes.
        let label = switch_to("b@e.com", false).unwrap();
        assert_eq!(label, "b@e.com");
        assert_eq!(hit_count(&hits), 1);
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("b@e.com"));
        assert_eq!(
            st.find("b@e.com").unwrap().access_token,
            "mock-refreshed-access-token"
        );

        // Expire b again. It is now ACTIVE: its token belongs to the vendor
        // CLI and must never be POSTed.
        let mut st = State::load().unwrap();
        st.find_mut("b@e.com").unwrap().expires_at = Utc::now().timestamp_millis() - 60_000;
        st.save().unwrap();
        switch_to("b@e.com", false).unwrap();
        crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeToken, None);
        assert_eq!(hit_count(&hits), 1, "the active account is never refreshed");
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_commits_only_while_the_expected_account_is_active() {
    with_claude_home("a@e.com", |_home| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();

        // The decision was made while "z" was active; it is not any more.
        let skipped = switch_to_guarded(
            provider,
            "b@e.com",
            &acct,
            &identity,
            false,
            Some("z@e.com"),
            false,
        )
        .unwrap();
        assert!(skipped.is_none());
        assert_eq!(State::load().unwrap().active.as_deref(), Some("a@e.com"));

        let done = switch_to_guarded(
            provider,
            "b@e.com",
            &acct,
            &identity,
            false,
            Some("a@e.com"),
            false,
        )
        .unwrap();
        assert_eq!(done.as_deref(), Some("b@e.com"));
        assert_eq!(State::load().unwrap().active.as_deref(), Some("b@e.com"));
        assert_eq!(keychain_access_token().as_deref(), Some("at-b@e.com"));
    });
}

/// Seed A (active) and B, then run a real guarded switch A -> B and return
/// A's stored account afterwards. `before` runs after seeding, to plant a
/// rotation for the outgoing account A somewhere other than state.json.
fn switch_a_to_b_after(before: impl FnOnce(&std::path::Path)) -> Account {
    let mut result = None;
    with_claude_home("a@e.com", |home| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        before(home);
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();
        let done =
            switch_to_guarded(provider, "b@e.com", &acct, &identity, false, None, false).unwrap();
        assert_eq!(done.as_deref(), Some("b@e.com"));
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("b@e.com"));
        result = st.find("a@e.com").cloned();
    });
    result.unwrap()
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_keeps_a_rotation_absorbed_from_disk_for_the_outgoing_account() {
    // H1: absorb_before_switch rewrites state.json under the lock; the
    // switch must reload afterwards or its save clobbers the rotation.
    let a = switch_a_to_b_after(plant_disk_rotation);
    assert_eq!(a.access_token, "at-disk-rotated");
    assert_eq!(a.refresh_token, "rt-disk-rotated");
}

fn plant_disk_rotation(home: &std::path::Path) {
    {
        // Only the on-disk rotation may be in play: empty the (process-wide
        // in-memory) keychain slot other tests leave behind.
        let account = std::env::var("USER").unwrap_or_else(|_| "claude".to_string());
        crate::platform()
            .secrets()
            .delete("Claude Code-credentials", &account)
            .unwrap();
        let exp = Utc::now().timestamp_millis() + 7_200_000;
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join(".credentials.json"),
            serde_json::json!({"claudeAiOauth": {
                "accessToken": "at-disk-rotated",
                "refreshToken": "rt-disk-rotated",
                "expiresAt": exp,
                "oauthAccount": {"emailAddress": "a@e.com"},
            }})
            .to_string(),
        )
        .unwrap();
    }
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_saves_the_keychain_rotation_of_the_outgoing_account() {
    // H_R2_1: sync_active_from_keychain must run after the reload so its
    // in-memory adoption is what gets saved.
    let a = switch_a_to_b_after(|_| {
        let exp = Utc::now().timestamp_millis() + 7_200_000;
        prime_active_slot(&format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at-kc-rotated","refreshToken":"rt-kc-rotated","expiresAt":{exp}}}}}"#
        ));
    });
    assert_eq!(a.access_token, "at-kc-rotated");
    assert_eq!(a.refresh_token, "rt-kc-rotated");
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_writes_the_fresher_of_the_two_token_sets() {
    with_claude_home("a@e.com", |_home| {
        let mut state_side = claude_account_with_identity("b@e.com", 3_600_000);
        state_side.set_tokens("at-state".into(), "rt-state".into(), 5_000);
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                state_side,
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let identity = serde_json::json!({"emailAddress": "b@e.com"});
        let phase1 = |expires_at: i64| {
            let mut a = claude_account_with_identity("b@e.com", 3_600_000);
            a.set_tokens("at-phase1".into(), "rt-phase1".into(), expires_at);
            a
        };
        // A concurrent poll rotated b after our snapshot: the newer set wins.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(4_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-state"));
        // Same expiry: our own snapshot is kept.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(5_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-phase1"));
        // Ours is newer: ours.
        switch_to_guarded(
            provider,
            "b@e.com",
            &phase1(6_000),
            &identity,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(keychain_access_token().as_deref(), Some("at-phase1"));
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn only_an_explicit_hold_request_on_an_over_trigger_account_holds() {
    with_claude_home("a@e.com", |_home| {
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
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let go = |email: &str, hold: bool| {
            let acct = State::load().unwrap().find(email).cloned().unwrap();
            let identity = acct.oauth_account.clone().unwrap();
            switch_to_guarded(provider, email, &acct, &identity, false, None, hold).unwrap();
            State::load()
                .unwrap()
                .manual_hold(CLAUDE_SLUG)
                .map(str::to_string)
        };
        assert_eq!(go("over@e.com", true).as_deref(), Some("over@e.com"));
        assert_eq!(
            go("over@e.com", false),
            None,
            "an auto move clears the hold"
        );
        assert_eq!(
            go("calm@e.com", true),
            None,
            "under the trigger nothing to ack"
        );
    });
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn compare_and_set_switch_for_claude_commits_only_while_the_expected_account_is_active() {
    with_claude_home("a@e.com", |_| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
                claude_account_with_identity("c@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        // The decision was taken while c was active; it no longer is.
        let lost = switch_if_still_active(CLAUDE_SLUG, "b@e.com", "c@e.com").unwrap();
        assert_eq!(lost, None);
        assert_eq!(State::load().unwrap().active.as_deref(), Some("a@e.com"));
        let won = switch_if_still_active(CLAUDE_SLUG, "b@e.com", "a@e.com").unwrap();
        assert_eq!(won.as_deref(), Some("b@e.com"));
        assert_eq!(State::load().unwrap().active.as_deref(), Some("b@e.com"));
        let direct =
            switch_to_if_still_active(providers::get(CLAUDE_SLUG).unwrap(), "c@e.com", "b@e.com")
                .unwrap();
        assert_eq!(direct.as_deref(), Some("c@e.com"));
    });
}

#[test]
fn claim_for_the_active_account_falls_back_to_the_stored_token_on_state_error() {
    let _g = crate::store::ScopedConfigDir::new();
    let provider: &'static MockActiveSlotProvider = Box::leak(Box::new(
        MockActiveSlotProvider::new(&mock_claude_blob("cc-at", "cc-rt", 9_999_999_999_999)),
    ));
    let mut acct =
        Account::from_keychain_blob(&mock_claude_blob("stored-at", "stored-rt", 1_000)).unwrap();
    acct.email = Some("active@example.com".into());
    // An unreadable state.json makes the locked CAS fail to load.
    std::fs::write(
        crate::store::config_dir().unwrap().join("state.json"),
        "{ not json",
    )
    .unwrap();
    claim_refresh_token(provider, "active@example.com", &mut acct, true)
        .expect("a CAS that cannot run must degrade to the stored token");
    assert_eq!(acct.access_token, "stored-at");
}

#[test]
fn claim_for_the_active_account_does_not_save_state_when_nothing_changed() {
    let _g = crate::store::ScopedConfigDir::new();
    let provider: &'static MockActiveSlotProvider = Box::leak(Box::new(
        MockActiveSlotProvider::new(&mock_claude_blob("x", "y", 1)),
    ));
    *provider.slot.lock().unwrap() = None; // slot empty: the CAS fails
    let mut acct =
        Account::from_keychain_blob(&mock_claude_blob("stored-at", "stored-rt", 1_000)).unwrap();
    acct.email = Some("active@example.com".into());
    let mut st = State::default();
    st.upsert(acct.clone());
    st.active = Some("active@example.com".into());
    st.save().unwrap();
    st.save().unwrap(); // second save so a rolling backup exists
    let dir = crate::store::config_dir().unwrap().join("backups");
    let count = || std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    let before = count();
    // Backups are named to the second; wait so a save would add one.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    claim_refresh_token(provider, "active@example.com", &mut acct, true).unwrap();
    assert_eq!(count(), before, "an unchanged state was re-saved");
}

#[test]
fn claim_for_the_active_account_uses_the_slot_token_without_posting() {
    let _g = crate::store::ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_token_server();
    crate::endpoints::set_test_override(
        crate::endpoints::Endpoint::ClaudeToken,
        Some(&format!("{base_url}/v1/oauth/token")),
    );
    let cc_blob = mock_claude_blob("cc-at", "cc-rt", 9_999_999_999_999);
    let provider: &'static MockActiveSlotProvider =
        Box::leak(Box::new(MockActiveSlotProvider::new(&cc_blob)));
    let mut acct =
        Account::from_keychain_blob(&mock_claude_blob("stale-at", "stale-rt", 1_000)).unwrap();
    acct.email = Some("active@example.com".into());
    let mut st = State::default();
    st.upsert(acct.clone());
    st.active = Some("active@example.com".into());
    st.save().unwrap();
    claim_refresh_token(provider, "active@example.com", &mut acct, true).unwrap();
    crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeToken, None);
    assert_eq!(
        acct.access_token, "cc-at",
        "claim used the stale stored token"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn guarded_switch_refuses_a_target_removed_during_phase_one() {
    with_claude_home("a@e.com", |_home| {
        seed_claude_state(
            vec![
                claude_account_with_identity("a@e.com", 3_600_000),
                claude_account_with_identity("b@e.com", 3_600_000),
            ],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        prime_active_slot(&mock_claude_blob("at-a@e.com", "rt-a", 9_999_999_999_999));
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();
        // `usagio rm b` lands while prepare_switch is on the network.
        let mut st = State::load().unwrap();
        assert!(st.remove("b@e.com"));
        st.save().unwrap();
        let _ = switch_to_guarded(provider, "b@e.com", &acct, &identity, false, None, false);
        assert_eq!(State::load().unwrap().active.as_deref(), Some("a@e.com"));
        assert_ne!(
            keychain_access_token().as_deref(),
            Some(acct.access_token.as_str())
        );
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn switch_away_keeps_a_renewal_whose_live_write_failed() {
    with_claude_home("a@e.com", |_home| {
        // A was renewed, but publishing the fresh grant to the slot failed:
        // the slot still holds A's old login.
        let mut a = claude_account_with_identity("a@e.com", 3_600_000);
        a.live_write_pending = Some(crate::logging::token_fingerprint(&a.refresh_token));
        let renewed_rt = a.refresh_token.clone();
        seed_claude_state(
            vec![a, claude_account_with_identity("b@e.com", 3_600_000)],
            Some("a@e.com"),
        );
        prime_active_slot(&mock_claude_blob("old-at", "old-rt", 9_999_999_999_999));
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();
        switch_to_guarded(provider, "b@e.com", &acct, &identity, false, None, false).unwrap();
        let after = State::load().unwrap();
        assert_eq!(after.active.as_deref(), Some("b@e.com"));
        assert_eq!(
            after.find("a@e.com").unwrap().refresh_token,
            renewed_rt,
            "the renewed grant was overwritten by the old slot login"
        );
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn a_single_switch_clears_the_outgoing_and_the_incoming_live_write_markers() {
    // Each clear in switch_to_guarded is pinned on its own: one switch, with a
    // marker on the outgoing account AND on the incoming one.
    with_claude_home("a@e.com", |_home| {
        let mut a = claude_account_with_identity("a@e.com", 3_600_000);
        a.live_write_pending = Some(crate::logging::token_fingerprint(&a.refresh_token));
        let mut b = claude_account_with_identity("b@e.com", 3_600_000);
        b.live_write_pending = Some(crate::logging::token_fingerprint(&b.refresh_token));
        seed_claude_state(vec![a, b], Some("a@e.com"));
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let acct = State::load().unwrap().find("b@e.com").cloned().unwrap();
        let identity = acct.oauth_account.clone().unwrap();
        switch_to_guarded(provider, "b@e.com", &acct, &identity, false, None, false).unwrap();
        let st = State::load().unwrap();
        assert_eq!(st.active.as_deref(), Some("b@e.com"));
        assert_eq!(
            st.find("a@e.com").unwrap().live_write_pending,
            None,
            "the outgoing account kept its marker"
        );
        assert_eq!(
            st.find("b@e.com").unwrap().live_write_pending,
            None,
            "the incoming account kept its marker"
        );
    });
}
