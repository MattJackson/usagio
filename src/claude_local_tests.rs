use super::*;
use crate::lifecycle::ActiveRefreshOutcome;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::store::{Account, State};
use crate::switching::claude::switch_to_guarded;
use crate::switching::identity::sync_active_from_keychain;
use crate::test_support::locked_active_refresh_cas;
use crate::test_support::{
    claude_account_with_identity, mock_claude_blob, prime_active_slot, seed_claude_state,
    with_claude_home,
};

#[test]
fn main_write_claude_identity_redoes_patch_when_file_changes_before_commit() {
    with_claude_home("a@e.com", |home| {
        let file = home.join(".claude.json");
        let mut first = true;
        write_claude_identity_with(&serde_json::json!({"emailAddress":"b@e.com"}), None, || {
            if std::mem::take(&mut first) {
                std::fs::write(&file, r#"{"cli_new_key":1}"#).unwrap();
            }
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(v["cli_new_key"], 1, "concurrent CLI write was dropped");
        assert_eq!(v["oauthAccount"]["emailAddress"], "b@e.com");
    });
}

#[test]
fn main_identity_write_retries_a_torn_claude_json_read() {
    with_claude_home("who@e.com", |home| {
        let file = home.join(".claude.json");
        std::fs::write(&file, "{\"a\":").unwrap(); // torn mid-write
        let f2 = file.clone();
        let healer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            std::fs::write(&f2, "{}").unwrap();
        });
        let r = write_claude_identity(&serde_json::json!({"emailAddress": "t@e.com"}), None);
        healer.join().unwrap();
        r.expect("a torn read must be retried, not abort the switch");
        let (oauth, _) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("t@e.com")
        );
    });
}

#[test]
fn claude_config_dir_redirects_the_main_switch_path() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join(".claude.json"), "{}").unwrap();
    crate::env_lock::scoped_env_var("CLAUDE_CONFIG_DIR", Some(cfg.to_str().unwrap()), || {
        assert_eq!(claude_json_path().unwrap(), cfg.join(".claude.json"));
        write_claude_identity(&serde_json::json!({"emailAddress": "c@e.com"}), None).unwrap();
        let (oauth, _) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("c@e.com")
        );
    });
}

#[test]
fn claude_json_helpers_read_write_and_report_the_file() {
    with_claude_home("who@e.com", |home| {
        assert_eq!(claude_json_path().unwrap(), home.join(".claude.json"));
        let (oauth, uid) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("who@e.com")
        );
        assert_eq!(uid.as_deref(), Some("user-1"));

        write_claude_identity(&serde_json::json!({"emailAddress": "new@e.com"}), None).unwrap();
        let (oauth, uid) = read_claude_identity();
        assert_eq!(
            oauth.unwrap().get("emailAddress").and_then(|v| v.as_str()),
            Some("new@e.com")
        );
        assert_eq!(uid, None, "an unknown userID must not be left behind");

        let (bytes, _mode) = read_claude_json_raw().unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("new@e.com"));

        std::fs::remove_file(home.join(".claude.json")).unwrap();
        assert_eq!(read_claude_identity(), (None, None));
        assert!(read_claude_json_raw().is_none());
    });
}

#[test]
fn claude_json_mode_reports_the_file_permissions_with_a_private_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.json");
    assert_eq!(claude_json_mode(&missing), 0o600);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.path().join("f.json");
        std::fs::write(&p, "{}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(claude_json_mode(&p) & 0o777, 0o640);
    }
    #[cfg(not(unix))]
    {
        let p = dir.path().join("f.json");
        std::fs::write(&p, "{}").unwrap();
        assert_eq!(claude_json_mode(&p), 0o600);
    }
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn pending_live_write_marker_does_not_survive_a_switch_away_and_back() {
    with_claude_home("a@e.com", |_home| {
        let mut a = claude_account_with_identity("a@e.com", 3_600_000);
        // A renewal whose live write failed left this marker on A.
        a.live_write_pending = Some(crate::logging::token_fingerprint(&a.refresh_token));
        seed_claude_state(
            vec![a, claude_account_with_identity("b@e.com", 3_600_000)],
            Some("a@e.com"),
        );
        let provider = providers::get(CLAUDE_SLUG).unwrap();
        let go = |email: &str| {
            let acct = State::load().unwrap().find(email).cloned().unwrap();
            let identity = acct.oauth_account.clone().unwrap();
            switch_to_guarded(provider, email, &acct, &identity, false, None, false).unwrap();
        };
        go("b@e.com");
        go("a@e.com");
        // The vendor CLI rotates the slot; no watcher absorbed it first.
        prime_active_slot(&mock_claude_blob("v-at", "v-rt", 9_999_999_999_999));
        let mut a = State::load().unwrap().find("a@e.com").cloned().unwrap();
        let outcome = locked_active_refresh_cas(provider, &mut a);
        assert!(
            matches!(outcome, ActiveRefreshOutcome::Adopted),
            "{outcome:?}"
        );
        assert_eq!(a.refresh_token, "v-rt", "vendor rotation was overwritten");
        let slot = Account::from_keychain_blob(&keychain_read().unwrap()).unwrap();
        assert_eq!(slot.refresh_token, "v-rt");
    });
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn keychain_rotation_is_adopted_only_for_the_matching_identity() {
    let provider = providers::get(CLAUDE_SLUG).unwrap();
    let rotated = claude_account_with_identity("a@e.com", 9_000_000);
    let mut rotated_blob = rotated.clone();
    rotated_blob.set_tokens("at-rotated".into(), "rt-rotated".into(), 9_999);
    let make_state = || {
        let mut st = State::default();
        st.upsert(claude_account_with_identity("a@e.com", 3_600_000));
        st.active = Some("a@e.com".to_string());
        st
    };
    // The vendor CLI is logged in as a@: its rotation is ours.
    with_claude_home("a@e.com", |_| {
        prime_active_slot(&rotated_blob.keychain_blob);
        let mut st = make_state();
        let reads = CLAUDE_IDENTITY_READS.with(std::cell::Cell::get);
        sync_active_from_keychain(provider, &mut st);
        // One parse of ~/.claude.json serves both the metadata refresh and
        // the rotation identity check.
        assert_eq!(CLAUDE_IDENTITY_READS.with(std::cell::Cell::get) - reads, 1);
        let a = st.find("a@e.com").unwrap();
        assert_eq!(a.access_token, "at-rotated");
        assert_eq!(a.expires_at, 9_999);
    });
    // Logged in as someone else: the keychain is not our account, hands off.
    with_claude_home("stranger@e.com", |_| {
        prime_active_slot(&rotated_blob.keychain_blob);
        let mut st = make_state();
        sync_active_from_keychain(provider, &mut st);
        assert_eq!(st.find("a@e.com").unwrap().access_token, "at-a@e.com");
    });
}

#[test]
fn keychain_account_is_the_login_name_with_a_fixed_fallback() {
    let exe = std::env::current_exe().unwrap();
    let probe = |user: Option<&str>| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--exact",
            "claude_local::tests::keychain_account_child_probe",
            "--nocapture",
        ])
        .env_remove("USER");
        if let Some(u) = user {
            cmd.env("USER", u);
        }
        let out = cmd.output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert!(probe(Some("alice-test")).contains("ACCOUNT=alice-test;"));
    assert!(probe(None).contains("ACCOUNT=claude;"));
}

#[test]
fn keychain_account_child_probe() {
    println!("ACCOUNT={};", keychain_account());
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn keychain_read_returns_what_keychain_write_stored() {
    with_claude_home("kc@e.com", |_| {
        keychain_write("blob-one").unwrap();
        assert_eq!(keychain_read().as_deref(), Some("blob-one"));
        keychain_write("blob-two").unwrap();
        assert_eq!(keychain_read().as_deref(), Some("blob-two"));
    });
}
