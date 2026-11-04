use super::*;
#[cfg(unix)]
use crate::test_support::read_log;

// ---------------------------------------------------------------------------
// M12 — .app bundle direct-launch (Finder double-click) must default to the
// menu bar, not the one-shot `list` a bare CLI invocation defaults to.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_menubar_when_invoked_from_app_bundle() {
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    assert_eq!(effective_first_arg(&[], &bundle_exe), Some("menubar"));
}

#[cfg(unix)]
#[test]
fn run_dispatch_defaults_to_list_for_bare_binary() {
    let bare_exe = std::path::PathBuf::from("/opt/homebrew/bin/usagio");
    assert_eq!(effective_first_arg(&[], &bare_exe), None);
}

#[cfg(unix)]
#[test]
fn run_dispatch_prefers_an_explicit_arg_over_bundle_detection() {
    // Even when launched from inside a bundle, an explicit argument (e.g.
    // `usagio list` run via Terminal against the bundled binary) wins.
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    let args = vec!["list".to_string()];
    assert_eq!(effective_first_arg(&args, &bundle_exe), Some("list"));
}

#[cfg(unix)]
#[test]
fn raising_the_open_file_limit_reaches_the_hard_cap_or_logs_why_not() {
    fn limits() -> libc::rlimit {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid resource id and a writable rlimit.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) }, 0);
        rl
    }
    let _g = crate::store::ScopedConfigDir::new();
    let before = limits();
    let target = before.rlim_max.min(65536);
    // Start from a low soft cap so there is something to raise.
    let low = target.min(512);
    if low >= target || before.rlim_cur != low {
        let lowered = libc::rlimit {
            rlim_cur: low,
            rlim_max: before.rlim_max,
        };
        // SAFETY: valid resource id and rlimit struct.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) } != 0 {
            return;
        }
    }
    let lowered_to = limits().rlim_cur;
    raise_nofile_limit();
    let after = limits().rlim_cur;
    // Leave the process as we found it for the tests running beside this one.
    let restore = libc::rlimit {
        rlim_cur: before.rlim_cur.max(after),
        rlim_max: before.rlim_max,
    };
    // SAFETY: valid resource id and rlimit struct.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &restore) };
    let log = read_log();
    assert!(!log.contains("getrlimit(NOFILE) failed"), "{log}");
    if lowered_to >= target {
        return; // nothing to raise on this host
    }
    if after > lowered_to {
        assert!(
            !log.contains("setrlimit(NOFILE"),
            "raised, yet logged a failure: {log}"
        );
        assert_eq!(after, target);
    } else {
        // The OS refused (macOS caps well below 65536); the attempt is logged.
        assert!(
            log.contains("setrlimit(NOFILE"),
            "no raise and no attempt: {log}"
        );
    }
}

#[test]
fn bundle_launch_with_empty_args_does_not_panic_on_tail() {
    let bundle_exe = std::path::PathBuf::from("/Applications/usagio.app/Contents/MacOS/usagio");
    let args: Vec<String> = Vec::new();
    // Mirrors the "menubar" dispatch arm for an argv-less bundle launch.
    if cfg!(unix) {
        assert_eq!(effective_first_arg(&args, &bundle_exe), Some("menubar"));
    }
    assert!(tail_args(&args).is_empty());
    let one = vec!["menubar".to_string()];
    assert!(tail_args(&one).is_empty());
    let two = vec!["menubar".to_string(), "--dark".to_string()];
    assert_eq!(tail_args(&two), &["--dark".to_string()]);
}
