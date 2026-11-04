use crate::menubar::test_support::{codex_state_acct, restore_test_acct};
use crate::providers::Provider;

#[test]
fn codex_capabilities_advertise_switching_and_launch() {
    let caps = crate::providers::codex::CodexProvider.capabilities();
    assert!(caps.supports_switching);
    assert!(caps.supports_launch);
    assert!(caps.supports_remove);
}

#[test]
fn claude_capabilities_advertise_launch_support() {
    let caps = crate::providers::claude::ClaudeProvider.capabilities();
    assert!(caps.supports_switching);
    assert!(caps.supports_launch);
    assert!(caps.supports_remove);
}

#[test]
fn restore_dropped_list_is_stable_across_calls_with_several_provider_slugs() {
    use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
    let _g = ScopedConfigDir::new();
    let mut current = State::default();
    for slug in ["codex", "zed", "alpha", "mid"] {
        current.upsert_provider_account(slug, codex_state_acct("k@x.com"));
    }
    current.save().unwrap();
    let first = accounts_dropped_by(&State::default()).unwrap();
    assert_eq!(first.len(), 4, "{first:?}");
    for _ in 0..40 {
        assert_eq!(accounts_dropped_by(&State::default()).unwrap(), first);
    }
}

#[test]
fn restore_that_only_adds_an_account_reports_no_drops() {
    use crate::store::{accounts_dropped_by, ScopedConfigDir, State};
    let _g = ScopedConfigDir::new();
    let mut current = State::default();
    current.accounts.push(restore_test_acct("a@e.com"));
    current.save().unwrap();

    let mut restore_target = State::load().unwrap();
    restore_target.accounts.push(restore_test_acct("b@e.com"));

    let dropped = accounts_dropped_by(&restore_target).unwrap();
    assert!(
        dropped.is_empty(),
        "adding an account must not be reported as a drop"
    );
}

#[test]
fn pre_restore_stash_lands_under_config_backups_not_tmp() {
    use crate::store::{config_dir, stash_pre_restore, ScopedConfigDir, State};
    let g = ScopedConfigDir::new();
    let mut current = State::default();
    current.accounts.push(restore_test_acct("a@e.com"));
    current.save().unwrap();

    let dir = config_dir().unwrap();
    let stash = stash_pre_restore(&dir)
        .unwrap()
        .expect("live state existed");
    // Strong positive assertion: the stash MUST be inside the scoped tempdir's
    // config backups dir. On Linux CI the tempdir itself lives under /tmp/xyz/,
    // so a `!starts_with("/tmp")` guard would false-positive — the positive
    // form here catches the real regression (stash landing outside backups/).
    assert!(stash.starts_with(g.home().join(".config/usagio/backups")));
}
