//! `usagio install` / `uninstall`: autostart registration and the legacy-label migrations.

use anyhow::Result;

use crate::{platform, store};

/// Reverse-DNS label for the OS's autostart registration (runs the menu-bar
/// app at login). Reused across install / uninstall on every platform: it's
/// the launchd Label on macOS, the .desktop filename on Linux, and the
/// registry value name on Windows.
pub(crate) const AUTOSTART_LABEL: &str = "com.mattjackson.usagio.menubar";
/// Previous launchd label (pre-v0.4.0). Referenced only by the one-shot
/// migration in `cmd_install` — unload + remove the old plist before
/// registering the new label so upgraded users don't run two agents.
pub(crate) const LEGACY_AUTOSTART_LABEL: &str = "com.claude-usage.menubar";

// ---------------------------------------------------------------------------
// Autostart install / uninstall — delegate to the platform Autostart backend
// ---------------------------------------------------------------------------

/// Path to invoke for the login item and for a post-upgrade relaunch, chosen to
/// survive `brew upgrade`. `current_exe()` resolves symlinks to the versioned
/// Homebrew Cellar path, which an upgrade deletes; map that back to the stable
/// `<prefix>/bin/usagio` symlink brew keeps repointing. For a from-source
/// install the resolved path is already stable.
pub(crate) fn stable_exe_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let s = exe.to_string_lossy();
    if let Some(idx) = s.find("/Cellar/usagio/") {
        let stable = std::path::PathBuf::from(format!("{}/bin/usagio", &s[..idx]));
        if stable.exists() {
            return stable;
        }
    }
    exe
}

/// Path to hand `launchctl` for the LaunchAgent's `ProgramArguments`, chosen
/// so Login Items shows the usagio app icon (see `packaging/macos/`) instead
/// of the generic "exec" glyph a bare binary gets. Resolution order:
///
/// 1. If we're already executing from inside a bundle — `current_exe()`
///    contains `.app/Contents/MacOS/` — use that path as-is. This is the
///    case once `usagio install` itself is invoked via the bundled
///    executable (e.g. a future launcher), and it's already the
///    Cellar-versioned bundle path brew keeps stable per version.
/// 2. Otherwise, probe for a sibling `usagio.app` next to the resolved
///    stable binary: a Homebrew install lays out
///    `<Cellar>/usagio/<version>/bin/usagio` alongside
///    `<Cellar>/usagio/<version>/usagio.app/Contents/MacOS/usagio`, so from
///    the binary's `bin/` directory, `usagio.app` is one level up.
/// 3. Fall back to the bare [`stable_exe_path`] if no bundle is found (a
///    from-source install, or a brew formula version predating the bundle).
pub(crate) fn launch_agent_exe_path() -> std::path::PathBuf {
    resolve_launch_agent_exe(std::env::current_exe().unwrap_or_default(), stable_exe_path)
}

/// [`launch_agent_exe_path`] over an injected `current_exe` and stable-path
/// resolver, so the bundle-preference rules are testable.
pub(crate) fn resolve_launch_agent_exe(
    current: std::path::PathBuf,
    stable: impl FnOnce() -> std::path::PathBuf,
) -> std::path::PathBuf {
    if current.to_string_lossy().contains(".app/Contents/MacOS/") {
        return current;
    }
    let stable = stable();
    sibling_app_bundle_exe(&stable).unwrap_or(stable)
}

/// Given a path to `.../bin/usagio` — typically the stable Homebrew symlink
/// `<prefix>/bin/usagio` — resolve a **version-stable** path to the sibling
/// `usagio.app/Contents/MacOS/usagio` that survives `brew upgrade`.
///
/// Homebrew keeps two stable roots per formula: `<prefix>/bin/<formula>`
/// (symlink to the current Cellar bin), and `<prefix>/opt/<formula>/`
/// (symlink to the current Cellar root — the whole version dir). Both point
/// at the just-installed version and are updated atomically on `brew upgrade`.
/// We MUST use one of those, not the canonicalized `<prefix>/Cellar/usagio/<version>/...`
/// path, because Homebrew deletes the old version's Cellar directory on
/// upgrade — pinning the LaunchAgent to the versioned path would silently
/// break autostart on every user's next `brew upgrade`.
///
/// Resolution order:
///   1. Given `<prefix>/bin/usagio`, derive `<prefix>` (bin's parent) and
///      probe `<prefix>/opt/usagio/usagio.app/Contents/MacOS/usagio`. Homebrew
///      creates that symlink whenever a formula ships a bundle. This is the
///      preferred path — brew keeps it valid across upgrades.
///   2. Given a non-brew layout (from-source install, custom deployment),
///      look for `usagio.app` as a sibling of the binary's version dir
///      (`../usagio.app/Contents/MacOS/usagio` from `bin/`). Not upgrade-
///      stable for brew but doesn't apply to brew installs.
///   3. Otherwise, `None` — callers fall back to the bare `stable_exe_path()`.
pub(crate) fn sibling_app_bundle_exe(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    // (1) Homebrew stable opt-path — the correct answer for a brew install.
    if let Some(bin_dir) = exe.parent() {
        if let Some(prefix) = bin_dir.parent() {
            let opt_bundle = prefix
                .join("opt")
                .join("usagio")
                .join("usagio.app")
                .join("Contents")
                .join("MacOS")
                .join("usagio");
            if opt_bundle.exists() {
                return Some(opt_bundle);
            }
        }
    }
    // (2) Non-brew: sibling in the version dir. Match on canonicalized form
    // too so a symlinked bin/usagio still finds a real sibling app.
    let mut candidates = vec![exe.to_path_buf()];
    if let Ok(resolved) = std::fs::canonicalize(exe) {
        candidates.push(resolved);
    }
    for path in candidates {
        let bin_dir = path.parent()?; // .../<version>/bin
        let version_dir = bin_dir.parent()?; // .../<version>
        let bundle_exe = version_dir
            .join("usagio.app")
            .join("Contents")
            .join("MacOS")
            .join("usagio");
        if bundle_exe.exists() {
            return Some(bundle_exe);
        }
    }
    None
}

pub(crate) fn cmd_install() -> Result<()> {
    // One-shot launchd migration: unload + remove the pre-v0.4.0 plist so
    // an upgrading user doesn't end up with two agents fighting for the
    // menu bar. Best-effort — a failed unload (e.g. label not loaded)
    // is fine; only the file-remove failure is worth surfacing.
    migrate_launchd_if_needed();
    // Also purge any legacy System Events login item (contract-01, v0.5.2
    // codeaudit). `MacOsAutostart::uninstall` already does this, but a
    // `brew upgrade` user who installed under a pre-v0.4.3 build and never
    // ran `usagio uninstall` keeps that stale entry forever — `install`
    // itself must clean it up too, since it's the path every upgrade
    // actually runs through.
    purge_legacy_login_items();

    let exe = launch_agent_exe_path();
    platform()
        .autostart()
        .install(AUTOSTART_LABEL, &exe, &["menubar"])?;
    println!("Installed and started the usagio menu bar app — it now runs at every login.");
    println!(
        "Logs: {}",
        store::config_dir()?.join("usagio.log").display()
    );
    Ok(())
}

pub(crate) fn cmd_uninstall() -> Result<()> {
    platform().autostart().uninstall(AUTOSTART_LABEL)?;
    // Also clean up the pre-v0.4.0 plist if the user last installed under
    // the `claude-usage` name.
    migrate_launchd_if_needed();
    println!("Uninstalled — the menu-bar app will no longer start at login.");
    Ok(())
}

/// One-shot launchd label migration. If a pre-v0.4.0 plist named
/// `com.claude-usage.menubar.plist` still exists in `~/Library/LaunchAgents/`,
/// unload it (best-effort) and remove the file so it doesn't run in parallel
/// with the new `com.mattjackson.usagio.menubar` agent.
pub(crate) fn migrate_launchd_if_needed() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let old_plist = std::path::PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LEGACY_AUTOSTART_LABEL}.plist"));
    if !old_plist.exists() {
        return;
    }
    // Best-effort: the plist may already be unloaded (typical after a reboot
    // or `brew uninstall`). launchctl prints "Unload failed: 5: Input/output
    // error" to stderr in that case, which surfaces as a scary line during
    // `usagio install`. Swallow stdout+stderr — we only care whether the
    // file removal below succeeds.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &old_plist.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match std::fs::remove_file(&old_plist) {
        Ok(()) => {
            eprintln!(
                "usagio: removed legacy launchd plist {}",
                old_plist.display()
            );
        }
        Err(e) => {
            eprintln!(
                "usagio: could not remove legacy launchd plist {}: {}",
                old_plist.display(),
                e
            );
        }
    }
}

/// Purge any legacy System Events login item a pre-v0.4.3 usagio (or its
/// `claude-usage` predecessor) registered via the now-removed "Launch at
/// login" menu toggle (contract-01, v0.5.2 codeaudit). Left in place, macOS
/// re-launches the stale binary at every login even after the launchd plist
/// is gone/never installed. `MacOsAutostart::uninstall`
/// (`src/platform/macos.rs`) already runs this exact purge on `usagio
/// uninstall`; this is a `main.rs`-local copy so `cmd_install` can run it too
/// without editing `src/platform/**` (out of scope for this pass — a later
/// pass should hoist both call sites onto one shared helper there instead of
/// keeping this duplicate in sync by hand).
///
/// Best-effort everywhere it's called from (`cmd_install`, which today only
/// runs meaningfully on macOS — `platform().autostart()` is the thing that's
/// actually OS-gated, via the `Platform` trait in `src/platform/`). No
/// `#[cfg(target_os)]` here on purpose: `tests/strict_cfg.rs` restricts that
/// attribute to `src/platform/*`, and spawning a nonexistent
/// `osascript` binary on a non-macOS host is already a silent, swallowed
/// `Err` — `Command::spawn`'s ordinary "not found" failure mode — so the
/// unconditional call is a no-op there without needing an explicit gate.
/// Also a no-op if the login item is already absent on macOS — `osascript`
/// exiting non-zero in that case is the expected, swallowed outcome.
pub(crate) fn purge_legacy_login_items() {
    for name in ["usagio", "claude-usage"] {
        let mut cmd = std::process::Command::new("/usr/bin/osascript");
        cmd.args([
            "-e",
            &format!("tell application \"System Events\" to delete login item \"{name}\""),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        let _ = run_with_deadline(cmd, std::time::Duration::from_secs(3));
    }
}

/// Run `cmd`, killing it if it outlives `deadline` (no dependency on
/// `timeout(1)`, which stock macOS does not ship). `Ok(true)` on exit 0.
pub(crate) fn run_with_deadline(
    mut cmd: std::process::Command,
    deadline: std::time::Duration,
) -> std::io::Result<bool> {
    let mut child = cmd.spawn()?;
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
