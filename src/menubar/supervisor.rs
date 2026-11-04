//! Poll-loop panic supervision and self-relaunch (upgrade / launchd kickstart).

use super::poller::poll_loop;
use std::time::Duration;

/// Max panic-and-respawn cycles tolerated in `PANIC_LOOP_WINDOW_SECS` before
/// `poll_loop_supervisor` treats it as a crash-loop (asks launchd for a clean
/// restart, else respawns slowly) rather than a transient one-off panic.
pub(super) const MAX_PANICS_PER_WINDOW: usize = 10;
pub(super) const PANIC_LOOP_WINDOW_SECS: u64 = 60;
/// What the supervisor does once it sees a panic loop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PanicLoopResponse {
    Exit,
    KeepRespawning,
}

/// Both LaunchAgent plists ship `KeepAlive=false`, so a bare exit is never
/// restarted: only exit once a launchd restart was actually issued; otherwise
/// keep respawning the poller (slowly) so the tray stays alive.
pub(super) fn panic_loop_response(restart: &LaunchdRestart) -> PanicLoopResponse {
    match restart {
        LaunchdRestart::Issued => PanicLoopResponse::Exit,
        LaunchdRestart::Failed | LaunchdRestart::NotManaged => PanicLoopResponse::KeepRespawning,
    }
}

/// Minimum process uptime before a panic loop may ask launchd for a restart.
/// A process that was itself just restarted and loops again falls to the slow
/// respawn path, so a deterministic panic cannot kickstart forever.
pub(super) const MIN_UPTIME_FOR_KICKSTART_SECS: u64 = 600;

pub(super) fn may_kickstart(uptime: Duration) -> bool {
    uptime.as_secs() >= MIN_UPTIME_FOR_KICKSTART_SECS
}

/// When this process started (first call wins; the supervisor primes it).
pub(super) fn process_uptime() -> Duration {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed()
}

/// Backoff between respawns once a panic loop was detected and no restart
/// was possible.
pub(super) const PANIC_LOOP_BACKOFF_SECS: u64 = 300;

/// Backoff between a `poll_loop` panic and respawning it.
pub(super) const PANIC_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Best-effort human string for a `catch_unwind` payload (usually a `&str` or
/// `String` from a `panic!`/`unwrap`/`expect` message; anything else — a
/// custom payload type — falls back to a fixed string rather than failing to
/// log at all).
pub(super) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Supervises `poll_loop`: if ANY unwind (an unwrap/expect/index-panic
/// anywhere in `run_cycle`/`refresh_usage_cache`/`active_refresh_cas`/…)
/// escapes it, the bare `std::thread::spawn(poll_loop)` this replaces would
/// let the whole background thread die silently — permanently disabling
/// polling, auto-swap, notifications, and credential sync for the rest of
/// the app's lifetime with no user-visible
/// symptom beyond stale menu numbers. Instead: log the panic, back off, and
/// respawn — self-healing by default. Only a genuine panic-loop (more than
/// `MAX_PANICS_PER_WINDOW` in `PANIC_LOOP_WINDOW_SECS`) asks launchd for a
/// clean restart (the `watchdog_self_restart` path) and exits only once that
/// was issued; the plists ship `KeepAlive=false`, so otherwise it keeps
/// respawning slowly rather than exiting into a dead tray.
pub(super) fn poll_loop_supervisor() {
    let _ = process_uptime(); // start the uptime clock
    let mut panics: Vec<std::time::Instant> = Vec::new();
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(poll_loop));
        // `poll_loop` is an infinite `loop {}` — reaching here at all means it
        // either panicked (Err) or, in principle, returned (Ok), which it
        // never does today but respawning either way is strictly safer than
        // silently exiting the supervisor thread.
        let reason = match outcome {
            Err(payload) => panic_payload_message(&*payload),
            Ok(()) => "poll_loop returned without panicking (unexpected)".to_string(),
        };
        crate::logging::log(&format!(
            "event=poll_loop_panic reason={reason} ; respawning"
        ));

        let now = std::time::Instant::now();
        panics.retain(|t| now.duration_since(*t).as_secs() < PANIC_LOOP_WINDOW_SECS);
        panics.push(now);
        if panics.len() > MAX_PANICS_PER_WINDOW {
            crate::logging::log(&format!(
                "event=poll_loop_panic_loop reason=\"more than {MAX_PANICS_PER_WINDOW} panics in \
                 {PANIC_LOOP_WINDOW_SECS}s\" ; asking launchd for a clean restart"
            ));
            let restart = if may_kickstart(process_uptime()) {
                relaunch_via_launchd()
            } else {
                LaunchdRestart::Failed
            };
            if panic_loop_response(&restart) == PanicLoopResponse::Exit {
                // kickstart -k replaces this process; wait to be replaced.
                std::thread::sleep(Duration::from_secs(3));
                std::process::exit(0);
            }
            crate::logging::log(
                "event=poll_loop_panic_loop no launchd restart available (KeepAlive=false); \
                 staying alive and retrying slowly",
            );
            std::thread::sleep(Duration::from_secs(PANIC_LOOP_BACKOFF_SECS));
            continue;
        }
        std::thread::sleep(Duration::from_secs(PANIC_RESPAWN_BACKOFF_SECS));
    }
}

/// Relaunch into the on-disk binary if it changed since we started (i.e. a
/// `brew upgrade` repointed the stable symlink), so the menu bar hot-swaps to
/// the new version without waiting for the next login.
///
/// When we're the launchd-managed agent we must NOT just spawn a child and
/// exit: the child is in the job's process group, and launchd SIGKILLs that
/// whole group when the main process exits (AbandonProcessGroup defaults to
/// false), so the replacement dies with us and — with KeepAlive=false — is never
/// restarted. Instead we ask launchd to restart the job (`launchctl kickstart
/// -k`), which relaunches it in a fresh job context. A bare/from-source run has
/// no such job, so there the orphaned self-spawn survives our exit as usual.
pub(super) fn maybe_relaunch_after_upgrade(start: &std::path::Path) {
    // Called from the macOS main-thread NSTimer tick (~0.75s). `canonicalize`
    // is a real syscall that hits disk; skip it 90%+ of the time via a
    // per-process last-check clock so the tray thread doesn't do blocking I/O
    // on the hot path (R2-PERF audit finding). A stale check window of 10s
    // is plenty — the brew upgrade + relaunch is best-effort and doesn't need
    // sub-second detection.
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
    static FAILED_TARGET: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
    {
        let mut g = LAST_CHECK.lock().unwrap();
        let should_check = !matches!(*g, Some(t) if t.elapsed() < Duration::from_secs(10));
        if !should_check {
            return;
        }
        *g = Some(Instant::now());
    }

    let stable = crate::cli::install::stable_exe_path();
    let Ok(now) = std::fs::canonicalize(&stable) else {
        return;
    };
    let mut failed = FAILED_TARGET.lock().unwrap_or_else(|e| e.into_inner());
    if !relaunch_due(start, &now, failed.as_deref()) {
        return;
    }
    crate::logging::log("binary changed on disk (brew upgrade); relaunching");
    match relaunch_via_launchd() {
        LaunchdRestart::Issued => {
            // kickstart -k will SIGKILL and restart this job; wait to be replaced.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        LaunchdRestart::Failed => {
            // Under launchd but the kickstart didn't take. Staying on the current
            // (old) binary — alive — is far better than exiting into a dead,
            // never-restarted state (KeepAlive=false won't bring us back).
            crate::logging::log(
                "launchctl kickstart failed; staying on current version until the binary changes again",
            );
            *failed = Some(now);
        }
        LaunchdRestart::NotManaged => {
            // Bare run (no launchd job): an orphaned child re-parents to
            // launchd/init and outlives our exit — but only exit if the spawn
            // actually succeeded, else we'd vanish with no replacement.
            match std::process::Command::new(&stable).arg("menubar").spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    crate::logging::log(&format!(
                        "relaunch spawn failed: {e}; staying on current version until the binary changes again"
                    ));
                    *failed = Some(now);
                }
            }
        }
    }
}

/// Whether the binary at `now` should be relaunched into: it differs from the
/// one this process started from, and a relaunch into it hasn't already failed
/// (a failure isn't retried every 10s; a newer binary is tried again).
pub(super) fn relaunch_due(
    start: &std::path::Path,
    now: &std::path::Path,
    failed: Option<&std::path::Path>,
) -> bool {
    now != start && failed != Some(now)
}

/// Outcome of attempting a launchd-driven restart.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) enum LaunchdRestart {
    /// kickstart succeeded — the caller should wait to be replaced.
    Issued,
    /// We're launchd-managed but kickstart failed — caller must NOT exit.
    Failed,
    /// Not launchd-managed — caller should self-spawn a replacement.
    NotManaged,
}

/// If we're running as the launchd agent, ask launchd to kill+restart the job
/// (`launchctl kickstart -k`). We wait on the command's exit status — reporting
/// success only when the kickstart actually took, so a failed request can't lead
/// the caller to exit into a dead, unrestarted state.
pub(super) fn relaunch_via_launchd() -> LaunchdRestart {
    if !is_launchd_managed() {
        return LaunchdRestart::NotManaged;
    }
    relaunch_via_launchd_kickstart()
}

/// Watchdog self-restart: when the fd/keychain watchdog's remediation ladder
/// reaches `SelfRestart`, bring the daemon back in a clean process via a
/// launchd kickstart. We reuse `relaunch_via_launchd` (the same path the
/// brew-upgrade hot-swap uses), which is battle-tested and only acts when we're
/// actually launchd-managed.
///
/// Deliberately restart-loop-safe: a kickstart replaces the process with a
/// fresh fd table, so we `sleep` briefly then `exit(0)` and wait to be
/// replaced. If we are NOT launchd-managed (a bare foreground `usagio watch`)
/// or the kickstart fails, we DO NOT exit — the plist ships `KeepAlive=false`,
/// so a bare `exit()` would leave nothing to revive us. Returning `false` there
/// is correct: the watcher respawn + the watchdog's surfaced log lines are the
/// safety net for that case, with zero restart-loop risk. Returns `true` only
/// when a restart was issued (in which case this does not return).
pub(crate) fn watchdog_self_restart() -> bool {
    match relaunch_via_launchd() {
        LaunchdRestart::Issued => {
            crate::logging::log("event=fd_watchdog self_restart=issued via launchctl kickstart");
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        }
        LaunchdRestart::Failed => {
            crate::logging::log(
                "event=fd_watchdog self_restart=failed (launchctl kickstart did not take); \
                 staying alive",
            );
            false
        }
        LaunchdRestart::NotManaged => {
            crate::logging::log(
                "event=fd_watchdog self_restart=skipped (not launchd-managed); \
                 relying on watcher respawn",
            );
            false
        }
    }
}

// Split out from `relaunch_via_launchd` so the `libc::getuid()` call (`libc`
// is a `[target.'cfg(unix)'.dependencies]` crate — not available on Windows
// at all) doesn't have to gate on `target_os` (which `tests/strict_cfg.rs`
// restricts outside `src/platform/`). `cfg(unix)`/`cfg(not(unix))` aren't
// `target_os` checks, so this needs no allowlist entry.
#[cfg(unix)]
pub(super) fn relaunch_via_launchd_kickstart() -> LaunchdRestart {
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{}", crate::cli::install::AUTOSTART_LABEL);
    crate::logging::log(&format!("relaunching via launchctl kickstart -k {target}"));
    match std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
    {
        Ok(s) if s.success() => LaunchdRestart::Issued,
        Ok(s) => {
            crate::logging::log(&format!("launchctl kickstart exited with {s}"));
            LaunchdRestart::Failed
        }
        Err(e) => {
            crate::logging::log(&format!("launchctl kickstart could not run: {e}"));
            LaunchdRestart::Failed
        }
    }
}

/// launchd is macOS-only; `is_launchd_managed()` (an `XPC_SERVICE_NAME` env
/// check) can only be true there, so this is unreachable at runtime on
/// Windows — it exists purely so `relaunch_via_launchd` compiles without
/// `libc` (a unix-only dependency).
#[cfg(not(unix))]
pub(super) fn relaunch_via_launchd_kickstart() -> LaunchdRestart {
    LaunchdRestart::NotManaged
}

/// Whether we're the launchd-managed agent. `XPC_SERVICE_NAME` is set by launchd
/// to the job label for a LaunchAgent, so a manual `usagio menubar` run
/// (which self-spawns fine) isn't misrouted to the kickstart path.
pub(super) fn is_launchd_managed() -> bool {
    launchd_managed_from_env(std::env::var("XPC_SERVICE_NAME").ok().as_deref())
}

/// Pure predicate behind `is_launchd_managed`, split out for testing.
pub(super) fn launchd_managed_from_env(xpc_service_name: Option<&str>) -> bool {
    xpc_service_name == Some(crate::cli::install::AUTOSTART_LABEL)
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
