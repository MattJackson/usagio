use super::*;
use crate::cli::capture::cmd_capture;
use crate::store::State;
use crate::test_support::codex_account;

#[test]
fn renew_selector_qualifier_picks_the_provider() {
    let mut slug = "claude".to_string();
    let sel = strip_renew_qualifier(&mut slug, Some("codex:dev@example.com".into()));
    assert_eq!(
        (slug.as_str(), sel.as_deref()),
        ("codex", Some("dev@example.com"))
    );
    let mut slug = "claude".to_string();
    let sel = strip_renew_qualifier(&mut slug, Some("dev@example.com".into()));
    assert_eq!(
        (slug.as_str(), sel.as_deref()),
        ("claude", Some("dev@example.com"))
    );
}

#[test]
fn renew_resolves_unique_provider_prefix_and_rejects_misses() {
    let mut state = State::default();
    state.upsert_provider_account("codex", codex_account("dev@example.com", None));
    assert_eq!(
        resolve_provider_renew_key(&state, "codex", "dev").unwrap(),
        "dev@example.com"
    );
    assert!(resolve_provider_renew_key(&state, "codex", "nobody").is_err());
}

#[test]
fn provider_slugs_are_case_insensitive_on_capture_and_renew() {
    let _cfg = crate::store::ScopedConfigDir::new();
    State::default().save().unwrap();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let err = format!("{:#}", cmd_capture(&args(&["Codex"])).unwrap_err());
        assert!(!err.contains("not registered"), "{err}");
        let err = format!(
            "{:#}",
            cmd_renew(&args(&["nobody@e.com", "--provider", "Codex"])).unwrap_err()
        );
        assert!(!err.contains("not registered"), "{err}");
    });
}
