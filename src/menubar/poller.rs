//! The background poll loop: cadence, wake-ups, the single-engine lock.

use super::model::{build_snapshot, now_utc};
use crate::poll::trackers::set_current_loop_interval_secs;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use crate::watch::cadence::{Cadence, TRIGGER_PCT, WATCH_INTERVAL_SECS};
use crate::watch::cycle::{watch_cycle, CycleOutcome, SwapGuards};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Poller thread
// ---------------------------------------------------------------------------

/// The `SwapGuard` shared by every in-process caller of `run_cycle` — the
/// long-lived poller (`poll_loop`) and the one-off "Refresh usage now" click
/// (`handle_refresh_now`) — so anti-thrash cooldown/no-return windows apply
/// to BOTH, not just poll-triggered swaps.
/// Before this, `handle_refresh_now` spawned its own throwaway
/// `SwapGuard::default()`: a manual click could swap right past a cooldown
/// the poller had just started, or bounce straight back into an account the
/// poller had just left inside its no-return window. `Arc<Mutex<_>>` (rather
/// than splitting the struct's fields into separate atomics) keeps
/// `SwapGuard`'s existing `&mut self` API — `left_at`'s `HashMap` and
/// `stuck_notified`'s bookkeeping aren't lock-free-friendly, and the guard is
/// only ever held for the duration of one `run_cycle` call, so a `Mutex`
/// contends at most twice a poll interval.
pub(super) fn shared_swap_guard() -> std::sync::Arc<std::sync::Mutex<SwapGuards>> {
    static GUARD: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<SwapGuards>>> =
        std::sync::OnceLock::new();
    GUARD
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(SwapGuards::default())))
        .clone()
}

/// Settle margin (seconds) after a window's reset instant before the poller
/// wakes to refresh it — the vendor's usage counter needs a beat to roll over.
const RESET_SETTLE_SECS: u64 = 5;

/// Seconds until the soonest upcoming session/weekly reset across all accounts,
/// plus [`RESET_SETTLE_SECS`]. `None` when no account has a known future reset.
/// Lets `poll_loop` wake at the reset boundary (refresh + swap immediately)
/// rather than waiting out the cadence — the reason a just-reset account showed
/// a stale near-100% instead of its fresh 0%.
fn next_reset_wake_secs() -> Option<u64> {
    let now = now_utc();
    build_snapshot()
        .sections
        .iter()
        .flat_map(|s| s.accounts.iter())
        .flat_map(|a| [a.session_reset_at, a.weekly_reset_at])
        .flatten()
        .filter(|dt| *dt > now)
        .map(|dt| (dt - now).num_seconds().max(0) as u64 + RESET_SETTLE_SECS)
        .min()
}

/// Longest single uninterrupted `thread::sleep`. A cadence interval longer
/// than this is slept in chunks no bigger than this so the loop re-evaluates
/// promptly after a system suspend/resume rather than riding out a stale
/// reading for the rest of a long interval. 30s = the backstop cadence.
const SLEEP_CHUNK_SECS: u64 = 30;

/// If the wall clock advances by more than the chunk we asked to sleep plus
/// this slack, the machine was suspended across the chunk (system sleep, lid
/// close, battery-death). We compare wall-clock deltas, not the monotonic
/// sleep return, because that gap is the whole signal.
const SUSPEND_GAP_SLACK_SECS: u64 = 8;

/// Whether a chunk's *wall-clock* elapsed indicates the machine was suspended
/// mid-sleep (so the poller should refresh immediately on resume instead of
/// finishing a now-stale interval). Pure + testable; the sleep mechanics call
/// it with real deltas.
fn is_suspend_gap(intended_chunk_secs: u64, actual_elapsed_secs: u64) -> bool {
    actual_elapsed_secs > intended_chunk_secs.saturating_add(SUSPEND_GAP_SLACK_SECS)
}

/// Sleep `total` seconds in chunks of at most [`SLEEP_CHUNK_SECS`], returning
/// early (`true`) the moment a forward wall-clock jump reveals the machine was
/// suspended across a chunk — so the next `run_cycle` refreshes right after
/// resume instead of up to a full interval later (the battery-death / lid-close
/// staleness). Returns `false` on an ordinary, uninterrupted sleep. Uses
/// `SystemTime` wall-clock deltas since the monotonic sleep timer can itself
/// pause across a suspend.
fn sleep_bounded_detecting_suspend(total: u64) -> bool {
    let mut remaining = total;
    while remaining > 0 {
        let chunk = remaining.min(SLEEP_CHUNK_SECS);
        let before = std::time::SystemTime::now();
        if wait_for_poll_request(Duration::from_secs(chunk)) {
            return true;
        }
        let elapsed = before.elapsed().map(|d| d.as_secs()).unwrap_or(chunk);
        if is_suspend_gap(chunk, elapsed) {
            let to = chrono::Utc::now().timestamp();
            crate::autoswap::shell::note_sleep(to - elapsed as i64 + chunk as i64, to);
            crate::logging::log(&format!(
                "poll: wall-clock jumped {elapsed}s over a {chunk}s sleep — system resumed from \
                 suspend; refreshing now (event=poll_wake_from_suspend gap_secs={elapsed})"
            ));
            return true;
        }
        remaining = remaining.saturating_sub(chunk);
    }
    false
}

/// Delay before refreshing a newly active account after an auto-swap — just
/// long enough for Claude Code to pick up the new credentials.
const POST_SWITCH_REFRESH_SECS: u64 = 5;

/// Set by [`request_poll_now`]; the poller's sleep waits on the condvar so a
/// request cuts the current sleep short.
static POLL_REQUEST: (std::sync::Mutex<bool>, std::sync::Condvar) =
    (std::sync::Mutex::new(false), std::sync::Condvar::new());

/// Ask the poller to run its next cycle now. Called after a switch so the new
/// active account is refreshed immediately — it may otherwise sit behind a
/// 1200s backoff earned by the account we just left.
pub(crate) fn request_poll_now() {
    let (lock, cv) = &POLL_REQUEST;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
}

/// True while this process is display-only because another usagio process
/// holds `engine.lock` (design §3.5).
pub(super) static ENGINE_DISPLAY_ONLY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Wait up to `timeout` for a [`request_poll_now`]; returns (and clears) the
/// request flag. Behaves like `thread::sleep(timeout)` when nothing is asked.
fn wait_for_poll_request(timeout: Duration) -> bool {
    let (lock, cv) = &POLL_REQUEST;
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let (mut guard, _) = cv
        .wait_timeout_while(guard, timeout, |requested| !*requested)
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *guard)
}

/// Options for the shared poll loop. The menu-bar app uses the defaults (the
/// trigger and auto-swap setting from Settings); `usagio watch` passes its
/// command-line overrides. One loop for both, so they can't drift.
pub(crate) struct LoopOpts {
    /// Overrides the configured trigger (`usagio watch --trigger`).
    pub(crate) trigger: Option<f64>,
    /// Lowers the swap-target session ceiling (`usagio watch --ceiling`).
    pub(crate) ceiling: Option<f64>,
    /// Base cadence (`usagio watch --interval`).
    pub(crate) base: u64,
}

impl Default for LoopOpts {
    fn default() -> Self {
        LoopOpts {
            trigger: None,
            ceiling: None,
            base: WATCH_INTERVAL_SECS,
        }
    }
}

pub(super) fn poll_loop() {
    poll_loop_with(&LoopOpts::default());
}

/// The poll + auto-swap loop shared by the menu-bar app and `usagio watch`.
pub(crate) fn poll_loop_with(opts: &LoopOpts) {
    // Single engine owner (§3.5): only the process holding engine.lock polls,
    // decides and switches; a second one (the app plus `usagio watch`) is
    // display-only and waits for the lock, so request budgets never double.
    let _engine_lock = acquire_engine_lock();
    let guard = shared_swap_guard();
    let mut cadence = Cadence::new(opts.base);
    let mut wd = crate::watchdog::Watchdog::default();
    let mut wd_effects = crate::watchdog::RealEffects;
    // Force a full refresh of EVERY account on the first cycle after launch —
    // including "locked" ones the per-cycle guard would normally skip — so the
    // menu never opens on stale data carried over from before the app was last
    // closed (e.g. an account that reset/boosted while usagio wasn't running).
    let mut first_cycle = true;
    loop {
        // v0.8.0: publish current loop cadence so the fetch-floor projection
        // uses the correct horizon under 429 backoff (see main.rs).
        set_current_loop_interval_secs(cadence.current_for(CLAUDE_SLUG));
        for p in providers::all() {
            crate::poll::trackers::set_provider_interval_secs(
                p.provider_id(),
                cadence.current_for(p.provider_id()),
            );
        }
        // Self-healing health check (fd-count + keychain-write), throttled to
        // ~once a minute. The menubar poller runs under launchd, so a critical
        // fd leak that a watcher respawn can't clear escalates to a launchd
        // kickstart into a clean process (see `crate::watchdog`).
        wd.maybe_run(&mut wd_effects);
        // Fetch usage + auto-swap; this writes cached usage to state.json, which
        // the main-thread timer reads back to render. This is the ONLY thing that
        // hits the network, so ordinary use can never rate-limit.
        let (outcome, trigger) = {
            let mut g = guard.lock().unwrap_or_else(|e| e.into_inner());
            run_cycle(&mut g, first_cycle, opts)
        };
        first_cycle = false;
        let current = cadence.advance(&outcome, trigger);
        let swapped = outcome.providers.iter().any(|p| p.swapped.is_some());
        // Wake right after the soonest upcoming window reset (+ a settle
        // margin) if that's sooner than the normal cadence — so a session/
        // weekly reset refreshes usage and fires any now-unblocked swap
        // immediately, instead of showing stale "99%" until the next poll. A
        // single targeted boundary wake, so it may dip below the cadence floor.
        let sleep_secs = match next_reset_wake_secs() {
            // An auto-swap this cycle: refresh the new active account right
            // away instead of flying blind on its (possibly old) cache.
            _ if swapped => POST_SWITCH_REFRESH_SECS,
            Some(w) if w < current => w.max(RESET_SETTLE_SECS),
            _ => current,
        };
        // Bounded, suspend-aware sleep: chunks the wait so a system resume
        // (sleep / lid-close / battery-death) is caught within a chunk and the
        // next iteration refreshes immediately, rather than showing a stale
        // pre-sleep reading until the full interval elapses. The return value
        // is intentionally unused here — the loop head already re-refreshes;
        // early return just gets us there sooner.
        let _resumed = sleep_bounded_detecting_suspend(sleep_secs);
    }
}

/// The engine role held by `poll_loop_with`. Dropping it (the loop unwinding
/// on a panic, before the supervisor's respawn backoff) releases the flock, so
/// it also marks this process display-only: another process may take the lock
/// in that window, and a manual refresh here must not run a second engine.
struct EngineLock {
    pub(super) _file: Option<std::fs::File>,
}

impl Drop for EngineLock {
    fn drop(&mut self) {
        ENGINE_DISPLAY_ONLY.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Take the engine lock (`~/.config/usagio/engine.lock`, a non-blocking
/// flock), waiting while another process holds it. The returned guard holds the
/// open file (the lock lives as long as it does); it holds no file if the lock
/// file can't be used at all — then this process runs the engine anyway rather
/// than never polling.
fn acquire_engine_lock() -> EngineLock {
    use fs2::FileExt;
    let mut logged = false;
    loop {
        let opened = crate::store::config_dir()
            .map_err(|e| format!("{e:#}"))
            .and_then(|d| {
                let _ = std::fs::create_dir_all(&d);
                let path = d.join("engine.lock");
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))
            });
        let file = match opened {
            Ok(f) => f,
            Err(why) => {
                crate::logging::log(&format!(
                    "event=engine_lock_unavailable error={why} ; running the engine unlocked"
                ));
                ENGINE_DISPLAY_ONLY.store(false, std::sync::atomic::Ordering::SeqCst);
                return EngineLock { _file: None };
            }
        };
        if file.try_lock_exclusive().is_ok() {
            ENGINE_DISPLAY_ONLY.store(false, std::sync::atomic::Ordering::SeqCst);
            if logged {
                crate::logging::log("event=engine_lock_acquired");
            }
            return EngineLock { _file: Some(file) };
        }
        ENGINE_DISPLAY_ONLY.store(true, std::sync::atomic::Ordering::SeqCst);
        if !logged {
            crate::logging::log(
                "event=engine_lock_busy ; another usagio process runs auto-swap — display-only",
            );
            eprintln!(
                "usagio: another usagio process (the menu-bar app?) is running auto-swap; \
                 waiting for it to exit"
            );
            logged = true;
        }
        let _ = wait_for_poll_request(Duration::from_secs(60));
    }
}

/// Like [`run_cycle`], keeping the failure (logged either way) so an explicit
/// Refresh click can say it failed.
pub(super) fn run_cycle_checked(
    guards: &mut SwapGuards,
    force: bool,
    opts: &LoopOpts,
) -> (std::result::Result<CycleOutcome, String>, f64) {
    let st = State::load().unwrap_or_default();
    let autoswap = !st.autoswap_disabled;
    let threshold = opts
        .trigger
        .unwrap_or_else(|| st.trigger_pct.unwrap_or(TRIGGER_PCT));
    // With auto-swap off, use an unreachable trigger so we only observe.
    let trigger = if autoswap { threshold } else { 101.0 };
    // The target ceiling follows the trigger (see `Row::eligible_target`).
    let ceiling = opts.ceiling.unwrap_or(trigger).min(trigger);
    match watch_cycle(trigger, ceiling, guards, force) {
        Ok(o) => (Ok(o), trigger),
        Err(e) => {
            crate::logging::log(&format!("poll failed: {e}"));
            (Err(format!("{e}")), trigger)
        }
    }
}

/// Run one poll+auto-swap cycle with the configured trigger (Settings ▸
/// Auto-swap, or `opts.trigger`), returning its outcome and the trigger used
/// so the caller's cadence math matches what `watch_cycle` dispatched on.
pub(super) fn run_cycle(
    guards: &mut SwapGuards,
    force: bool,
    opts: &LoopOpts,
) -> (CycleOutcome, f64) {
    let (res, trigger) = run_cycle_checked(guards, force, opts);
    let outcome = res.unwrap_or(CycleOutcome {
        providers: Vec::new(),
    });
    (outcome, trigger)
}

#[cfg(test)]
#[path = "poller_tests.rs"]
mod tests;
