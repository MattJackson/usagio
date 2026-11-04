use super::*;
use std::time::Duration;

#[test]
fn panic_payload_message_handles_str_string_and_other() {
    // `poll_loop_supervisor` logs whatever
    // `catch_unwind` hands back — almost always a `&str` (`panic!("...")`)
    // or `String` (`format!(...)`-built panic message), but a custom
    // payload type must still produce SOME loggable string instead of
    // panicking the supervisor itself.
    let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
    assert_eq!(panic_payload_message(&*str_payload), "boom");

    let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
    assert_eq!(panic_payload_message(&*string_payload), "kaboom");

    let other_payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
    assert_eq!(
        panic_payload_message(&*other_payload),
        "non-string panic payload"
    );
}

#[test]
fn launchd_managed_detects_matching_service_name() {
    assert!(launchd_managed_from_env(Some(
        crate::cli::install::AUTOSTART_LABEL
    )));
    assert!(!launchd_managed_from_env(Some("com.other.service")));
    assert!(!launchd_managed_from_env(None));
}

// --- launchd / relaunch --------------------------------------------------

#[test]
fn launchd_managed_follows_the_xpc_service_name_env() {
    use crate::env_lock::scoped_env_var;
    scoped_env_var(
        "XPC_SERVICE_NAME",
        Some(crate::cli::install::AUTOSTART_LABEL),
        || {
            assert!(is_launchd_managed());
        },
    );
    scoped_env_var("XPC_SERVICE_NAME", Some("com.apple.Terminal"), || {
        assert!(!is_launchd_managed());
    });
    scoped_env_var("XPC_SERVICE_NAME", None, || {
        assert!(!is_launchd_managed());
    });
}

#[test]
fn relaunch_and_self_restart_do_nothing_when_not_launchd_managed() {
    let _g = crate::store::ScopedConfigDir::new(); // logging needs a config dir
    crate::env_lock::scoped_env_var("XPC_SERVICE_NAME", None, || {
        assert!(matches!(relaunch_via_launchd(), LaunchdRestart::NotManaged));
        assert!(
            !watchdog_self_restart(),
            "no restart issued outside launchd"
        );
    });
}

#[test]
fn upgrade_check_leaves_an_unchanged_binary_alone() {
    // The binary on disk is the one we started from → no relaunch. (A
    // wrongly-taken relaunch path logs first, which panics here without a
    // scoped config dir — deliberately not set up.)
    let start =
        std::fs::canonicalize(crate::cli::install::stable_exe_path()).expect("test exe path");
    maybe_relaunch_after_upgrade(&start);
}

#[test]
fn panic_loop_exits_only_when_a_restart_was_issued() {
    assert_eq!(
        panic_loop_response(&LaunchdRestart::Issued),
        PanicLoopResponse::Exit
    );
    for r in [LaunchdRestart::Failed, LaunchdRestart::NotManaged] {
        assert_eq!(
            panic_loop_response(&r),
            PanicLoopResponse::KeepRespawning,
            "KeepAlive=false means a bare exit is never restarted"
        );
    }
}

#[test]
fn a_failed_relaunch_is_not_retried_until_the_binary_changes_again() {
    use std::path::Path;
    let (a, b, c) = (
        Path::new("/v1/usagio"),
        Path::new("/v2/usagio"),
        Path::new("/v3/usagio"),
    );
    assert!(!relaunch_due(a, a, None), "unchanged binary");
    assert!(relaunch_due(a, b, None), "first sight of a new binary");
    assert!(
        !relaunch_due(a, b, Some(b)),
        "a relaunch into this binary already failed"
    );
    assert!(relaunch_due(a, c, Some(b)), "a newer binary retries");
}

#[test]
fn launchd_restart_is_allowed_once_per_incident() {
    assert!(!may_kickstart(Duration::from_secs(60)), "fresh restart");
    assert!(!may_kickstart(Duration::from_secs(599)));
    assert!(may_kickstart(Duration::from_secs(600)));
}
