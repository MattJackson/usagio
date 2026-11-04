use super::*;
use crate::providers::CLAUDE_SLUG;

// --- env-override guard (CLAUDE_CODE_OAUTH_TOKEN) ---

#[test]
fn env_override_active_reads_shared_slug_map() {
    // The hook drives both the menu (via menubar::model::env_override_for) and the
    // swap filter — one source of truth for the whole app. The un-hooked
    // answer falls through to the real env var, so pin it unset first.
    crate::env_lock::scoped_env_var(CLAUDE_ENV_OVERRIDE_VAR, None, || {
        assert!(!env_override_active(CLAUDE_SLUG));
        with_env_override_hook(&[CLAUDE_SLUG], || {
            assert!(env_override_active(CLAUDE_SLUG));
            // Unknown / non-Claude slugs are unaffected by the Claude env var.
            assert!(!env_override_active("codex"));
        });
        // Restored on exit.
        assert!(!env_override_active(CLAUDE_SLUG));
    });
}

#[test]
fn env_override_names_the_variable_that_bypasses_the_login() {
    assert_eq!(
        env_override_var_for(CLAUDE_SLUG),
        Some("CLAUDE_CODE_OAUTH_TOKEN")
    );
    assert_eq!(CLAUDE_ENV_OVERRIDE_VAR, "CLAUDE_CODE_OAUTH_TOKEN");
    assert_eq!(env_override_var_for("codex"), None);
    assert_eq!(env_override_var_for("nope"), None);
}

/// Child half of `env_override_is_active_only_for_a_non_empty_variable`: a
/// no-op unless the parent launched it with an expectation.
#[test]
fn env_override_child_probe() {
    let Ok(expect) = std::env::var("USAGIO_PROBE_EXPECT_OVERRIDE") else {
        return;
    };
    assert_eq!(env_override_active(CLAUDE_SLUG), expect == "1");
    assert!(!env_override_active("codex"));
}

/// Real-environment check, run in a child process so flipping the variable
/// can never race other tests reading it.
#[test]
fn env_override_is_active_only_for_a_non_empty_variable() {
    let exe = std::env::current_exe().unwrap();
    let probe = |value: Option<&str>, expect: &str| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--exact",
            "env_override::tests::env_override_child_probe",
            "--nocapture",
        ])
        .env("USAGIO_PROBE_EXPECT_OVERRIDE", expect)
        .env_remove(CLAUDE_ENV_OVERRIDE_VAR);
        if let Some(v) = value {
            cmd.env(CLAUDE_ENV_OVERRIDE_VAR, v);
        }
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success(), "probe({value:?}) failed:\n{text}");
        assert!(text.contains("1 passed"), "probe did not run:\n{text}");
    };
    probe(Some("tok"), "1");
    probe(Some(""), "0");
    probe(None, "0");
}
