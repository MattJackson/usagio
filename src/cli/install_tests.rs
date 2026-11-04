use super::*;

// The switch_to_guarded reload/sync ordering (H1, H_R2_1) is pinned by
// `guarded_switch_keeps_a_rotation_absorbed_from_disk_for_the_outgoing_account`
// and `guarded_switch_saves_the_keychain_rotation_of_the_outgoing_account`,
// which drive the real function.

// --- launch_agent_exe_path / sibling_app_bundle_exe ---

// Used only by macOS launch-agent / bundle tests below (all `#[cfg(target_os = "macos")]`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mock_cellar_bundle_layout(
    prefix: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let version_dir = dir.join("Cellar/usagio/0.9.9");
    let bin_dir = version_dir.join("bin");
    let bundle_macos_dir = version_dir.join("usagio.app/Contents/MacOS");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&bundle_macos_dir).unwrap();

    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();
    let bundle_exe = bundle_macos_dir.join("usagio");
    std::fs::write(&bundle_exe, b"bundled").unwrap();

    (dir, bare_exe, bundle_exe)
}

// macOS-only: exercises the .app bundle path-resolution helper (Homebrew
// Cellar layout) — no such concept on Linux/Windows. Uses cfg(unix)
// which the strict-cfg guard leaves alone.
#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_finds_bundle_next_to_bin_dir() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-bundle-test");

    let found = sibling_app_bundle_exe(&bare_exe).expect("bundle exe should be found");
    assert_eq!(found, bundle_exe);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The real brew shape: `<prefix>/bin/usagio` is a symlink into the versioned
/// Cellar dir, and `<prefix>/opt/usagio` is the upgrade-stable alias. The
/// LaunchAgent must get the opt path, never the versioned Cellar one.
#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_prefers_the_stable_brew_opt_path() {
    let (dir, cellar_bare, cellar_bundle) = mock_cellar_bundle_layout("usagio-opt-bundle-test");
    let prefix = dir.join("prefix");
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    let stable_bin = prefix.join("bin/usagio");
    std::os::unix::fs::symlink(&cellar_bare, &stable_bin).unwrap();
    let opt_macos = prefix.join("opt/usagio/usagio.app/Contents/MacOS");
    std::fs::create_dir_all(&opt_macos).unwrap();
    let opt_bundle = opt_macos.join("usagio");
    std::fs::write(&opt_bundle, b"opt-bundled").unwrap();

    let found = sibling_app_bundle_exe(&stable_bin).expect("bundle exe should be found");
    assert_eq!(found, opt_bundle);
    assert_ne!(found, cellar_bundle);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn sibling_app_bundle_exe_none_when_bundle_absent() {
    let dir = std::env::temp_dir().join(format!(
        "usagio-nobundle-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin_dir = dir.join("Cellar/usagio/0.9.9/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bare_exe = bin_dir.join("usagio");
    std::fs::write(&bare_exe, b"bare").unwrap();

    assert!(sibling_app_bundle_exe(&bare_exe).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Mocks a Homebrew-style `<Cellar>/usagio/<version>/{bin,usagio.app}` layout
/// and checks `resolve_launch_agent_exe` (the consumer `cmd_install` uses to
/// build the LaunchAgent's `ProgramArguments`) picks the bundle's
/// `Contents/MacOS/usagio`, not the bare binary. This is what makes Login
/// Items show the app icon instead of the generic "exec" glyph. The plist
/// itself is not written or inspected here.
#[cfg(unix)]
#[test]
fn usagio_install_prefers_app_bundle_path_when_available() {
    let (dir, bare_exe, bundle_exe) = mock_cellar_bundle_layout("usagio-install-bundle-test");

    // The real consumer (`launch_agent_exe_path`) over an injected pair:
    // running from the bare Cellar binary must resolve to the bundle exe.
    let resolved = resolve_launch_agent_exe(bare_exe.clone(), || bare_exe.clone());
    assert_eq!(resolved, bundle_exe);
    assert!(resolved
        .to_string_lossy()
        .contains("usagio.app/Contents/MacOS/usagio"));

    // Already running from inside a bundle: used as-is, stable never consulted.
    let in_bundle = resolve_launch_agent_exe(bundle_exe.clone(), || {
        panic!("stable path must not be resolved when already in a bundle")
    });
    assert_eq!(in_bundle, bundle_exe);

    // No sibling bundle: falls back to the bare stable path.
    let lone = dir.join("lone/bin/usagio");
    assert_eq!(
        resolve_launch_agent_exe(lone.clone(), || lone.clone()),
        lone
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn executable_paths_resolve_to_the_running_binary_outside_a_brew_or_bundle_layout() {
    let exe = std::env::current_exe().unwrap();
    assert!(!exe.as_os_str().is_empty());
    assert_eq!(stable_exe_path(), exe);
    assert_eq!(launch_agent_exe_path(), exe);
}

#[test]
fn legacy_launchd_plist_is_removed_when_present_and_ignored_otherwise() {
    let scd = crate::store::ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        let agents = home.join("Library").join("LaunchAgents");
        let plist = agents.join(format!("{LEGACY_AUTOSTART_LABEL}.plist"));
        // Absent: nothing happens (and nothing is created).
        migrate_launchd_if_needed();
        assert!(!agents.exists());
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(&plist, "not a real plist").unwrap();
        let other = agents.join("com.example.keep.plist");
        std::fs::write(&other, "keep").unwrap();
        migrate_launchd_if_needed();
        assert!(!plist.exists(), "the pre-0.4.0 agent plist must be removed");
        assert!(other.exists(), "unrelated agents are untouched");
    });
}

#[cfg(unix)]
#[test]
fn run_with_deadline_kills_an_overrunning_child_without_timeout_binary() {
    let started = std::time::Instant::now();
    let mut cmd = std::process::Command::new("/bin/sleep");
    cmd.arg("30");
    let ok = run_with_deadline(cmd, std::time::Duration::from_millis(150)).unwrap();
    assert!(!ok);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let cmd = std::process::Command::new("/usr/bin/true");
    assert!(run_with_deadline(cmd, std::time::Duration::from_secs(5)).unwrap());
}
