use super::*;
use crate::menubar::test_support::restore_test_acct;

// -----------------------------------------------------------------
// Backups Save…/Restore… ▸ FileDialog trait routing
//
// These assert the click handlers call `FileDialog` with the expected
// arguments WITHOUT ever popping a real native panel — the mock's
// `save_file`/`pick_file` return `None` by default, so each handler
// returns right after recording the call (the "user cancelled" path).
// -----------------------------------------------------------------

#[test]
fn backup_save_click_calls_file_dialog_with_default_name_and_downloads_dir() {
    let dialog = crate::platform::MockFileDialog::default();
    handle_backup_save_with(&dialog);

    let calls = dialog.save_file_calls.borrow();
    assert_eq!(calls.len(), 1, "expected exactly one save_file() call");
    let (default_name, default_dir) = &calls[0];
    assert!(
        default_name.starts_with("usagio-state-") && default_name.ends_with(".json"),
        "unexpected default file name: {default_name}"
    );
    assert_eq!(default_dir.as_deref(), dirs::download_dir().as_deref());
}

#[test]
fn restore_rejects_redacted_dump_and_keeps_live_tokens() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut live = crate::store::State::default();
    live.accounts.push(restore_test_acct("a@e.com"));
    live.save().unwrap();
    let redacted = crate::store::redact_state_for_dump(&crate::store::State::load().unwrap());
    let path = crate::store::config_dir()
        .unwrap()
        .join("redacted-dump.json");
    std::fs::write(&path, serde_json::to_vec(&redacted).unwrap()).unwrap();

    let dialog = crate::platform::MockFileDialog::default();
    *dialog.pick_file_returns.borrow_mut() = Some(path);
    handle_backup_restore_dialog_with(&dialog);

    let after = crate::store::State::load().unwrap();
    assert_eq!(
        after.accounts[0].access_token, "at",
        "restoring a redacted dump must not overwrite live tokens"
    );
}

#[test]
fn backup_restore_click_calls_file_dialog_pick_file_once() {
    let _g = crate::store::ScopedConfigDir::new();
    let dialog = crate::platform::MockFileDialog::default();
    handle_backup_restore_dialog_with(&dialog);

    let calls = dialog.pick_file_calls.borrow();
    assert_eq!(calls.len(), 1, "expected exactly one pick_file() call");
}

// -----------------------------------------------------------------------
// Restore… drop-confirmation wording + store-level
// wiring. `handle_backup_restore_dialog` itself opens a native file panel
// and a native `rfd::MessageDialog` confirm prompt, neither of which is
// unit-testable; these tests cover the pure logic it's built from.
// -----------------------------------------------------------------------

#[test]
fn restore_drop_confirmation_names_dropped_emails_not_just_a_count() {
    let msg = super::restore_drop_confirmation(&[
        "dev@example.com".to_string(),
        "personal@example.org".to_string(),
    ]);
    assert_eq!(
        msg,
        "Restoring will drop dev@example.com, personal@example.org. Continue?"
    );
}

#[test]
fn restore_that_drops_an_account_reports_exactly_that_email() {
    use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
    let _g = ScopedConfigDir::new();
    let mut current = State::default();
    for email in ["dev@example.com", "personal@example.org"] {
        current.accounts.push(restore_test_acct(email));
    }
    current.save().unwrap();

    let mut restore_target = State::default();
    restore_target
        .accounts
        .push(restore_test_acct("dev@example.com"));

    let dropped = accounts_dropped_by(&restore_target).unwrap();
    assert_eq!(dropped, vec!["personal@example.org".to_string()]);
    assert_eq!(
        restore_drop_confirmation(&dropped),
        "Restoring will drop personal@example.org. Continue?"
    );
}

#[test]
fn restore_without_drops_applies_the_chosen_backup() {
    use crate::store::{ScopedConfigDir, State};
    let _g = ScopedConfigDir::new();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("backup.json");
    let mut backup = State::default();
    backup.accounts.push(restore_test_acct("restored@e.com"));
    std::fs::write(&file, serde_json::to_vec(&backup).unwrap()).unwrap();

    let dialog = crate::platform::MockFileDialog::default();
    *dialog.pick_file_returns.borrow_mut() = Some(file);
    handle_backup_restore_dialog_with(&dialog);

    let keys: Vec<String> = State::load()
        .unwrap()
        .accounts
        .iter()
        .map(|a| a.key().to_string())
        .collect();
    assert_eq!(
        keys,
        ["restored@e.com"],
        "no accounts dropped → no prompt, restore applied"
    );
}
