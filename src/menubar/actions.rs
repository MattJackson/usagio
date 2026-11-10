//! Menu click handlers: switch, capture, remove, refresh, settings toggles, and the UI-task queue.

use super::backup::{handle_backup_restore_dialog, handle_backup_save};
use super::poller::{
    request_poll_now, run_cycle_checked, shared_swap_guard, LoopOpts, ENGINE_DISPLAY_ONLY,
};
use super::signin::{handle_renew, handle_sign_in, sweep_sign_in_stores_later};
use crate::cli::capture::{capture_current, capture_current_generic};
use crate::notifications::notify;
use crate::picking::optimize_now;
use crate::providers;
use crate::providers::require;
use crate::providers::trait_def::{ResetGrant, SESSION_LIMIT_KEY, WEEKLY_LIMIT_KEY};
use crate::providers::CLAUDE_SLUG;
use crate::state_ops::{remove_account, remove_provider_account_generic, with_state_lock};
use crate::store::State;
use crate::switching::claude::switch_to;
use crate::switching::provider::switch_to_provider_account;
use crate::watch::cycle::SwapGuards;

// ---------------------------------------------------------------------------
// Click handling
// ---------------------------------------------------------------------------

/// Parsed click id: `action[:slug[:key]]`. `slug`/`key` are `None` for global
/// actions (`quit`, `autoswap:toggle`, `autoswap:off`, `autoswap:95`,
/// `autoswap:now`, `notifications:threshold` / `:resetback` / `:pace`,
/// `backup:save`, `backup:restore`, `refresh:now`, `noop`, `capture` — the
/// plain-capture id used only by the submenu title itself, never a click).
struct ClickId<'a> {
    pub(super) action: &'a str,
    pub(super) slug: Option<&'a str>,
    pub(super) key: Option<&'a str>,
}

fn parse_click_id(id: &str) -> ClickId<'_> {
    let mut it = id.splitn(3, ':');
    let action = it.next().unwrap_or("");
    let slug = it.next();
    let key = it.next();
    ClickId { action, slug, key }
}

pub(super) fn handle_click(id: &str) {
    let c = parse_click_id(id);
    // Actions only mutate state.json; the main-thread timer re-renders from it
    // within ~1s without any network call.
    match (c.action, c.slug, c.key) {
        ("quit", _, _) => std::process::exit(0),
        ("noop", _, _) => {}
        ("autoswap", Some("off"), None) => set_autoswap(false),
        ("autoswap", Some("credits"), None) => toggle_use_credits(),
        // Off the UI thread (a switch can refresh a token over the network)
        // and ordered against queued Switch clicks by the generation counter.
        ("autoswap", Some("now"), None) => {
            spawn_latest_switch(CLAUDE_SLUG, |guard| match optimize_now() {
                Ok(Some(email)) => {
                    guard.for_provider(CLAUDE_SLUG).last_swap = Some(std::time::Instant::now());
                    notify(&format!("Switched to {email}"));
                    request_poll_now();
                }
                Ok(None) => notify("Already on the best account"),
                Err(e) => notify(&format!("Optimize failed: {e}")),
            })
        }
        ("autoswap", Some(t), None) => {
            if let Ok(v) = t.parse::<f64>() {
                set_autoswap_threshold(v);
            }
        }
        ("notifications", Some(trigger @ ("threshold" | "resetback" | "pace")), None) => {
            toggle_notification_trigger(trigger)
        }
        ("trayicon", Some(mode), None) => set_tray_icon_mode(mode),
        ("capture", Some(slug), None) => handle_capture(slug),
        ("apikey", Some(slug), None) => handle_apikey_capture(slug),
        ("switch", Some(slug), Some(key)) => handle_switch(slug, key),
        ("remove", Some(slug), Some(key)) => handle_remove(slug, key),
        ("renew", Some(slug), Some(key)) => handle_renew(slug, key),
        ("claim", Some(slug), Some(key)) => handle_claim(slug, key),
        ("signin", Some(slug), None) => handle_sign_in(slug),
        ("backup", Some("save"), None) => handle_backup_save(),
        ("backup", Some("restore"), None) => handle_backup_restore_dialog(),
        ("refresh", Some("now"), None) => handle_refresh_now(),
        // Ignore unrecognized ids (e.g. the top-level "capture"/"capture:apikey"
        // submenu titles or a future action added by a later phase we don't
        // yet handle).
        _ => {}
    }
}

/// Guards against piling up blocked threads when "Refresh usage now" is
/// clicked repeatedly: a stalled network call (see `run_cycle`) means each
/// new click would otherwise stack up its own throwaway `SwapGuard` and
/// thread. `false` = no refresh in flight; CAS to `true` before spawning,
/// reset to `false` when the spawned thread's cycle completes.
static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// "Refresh usage now" click (Settings ▸ Advanced ▸). Runs one poll +
/// auto-swap cycle on a background thread — same `run_cycle` the poll loop
/// calls every `WATCH_INTERVAL_SECS` — so the menu never blocks on network
/// I/O. The main-thread timer picks up the refreshed cache on its next tick.
///
/// De-duplicated via `REFRESH_IN_FLIGHT`: a second click while a refresh is
/// still running notifies instead of spawning another thread. See
/// `try_start_refresh` for the testable core.
/// Emails of every account currently flagged `needs_relogin`, across the
/// Claude slot and all provider slots, sorted for a stable notification body.
/// Reads state directly (no network) so it reflects whatever the just-finished
/// refresh cycle persisted.
fn accounts_needing_relogin() -> Vec<String> {
    let st = State::load().unwrap_or_default();
    let mut out: Vec<String> = st
        .accounts
        .iter()
        // A Free account renders as its plan, not "re-login" — don't nag about it.
        .filter(|a| a.needs_relogin && !a.no_subscription)
        .map(|a| a.key().to_string())
        .chain(
            st.providers
                .values()
                .flat_map(|p| p.accounts.iter())
                .filter(|a| a.needs_relogin && !a.no_subscription)
                .map(|a| a.key.clone()),
        )
        .collect();
    out.sort();
    out.dedup();
    out
}

fn handle_refresh_now() {
    // Single engine owner (design §3.5): a display-only process runs no poll,
    // decide or switch, so it must not run a cycle (a second engine with
    // independent hysteresis). The owner refreshes on its own schedule.
    if ENGINE_DISPLAY_ONLY.load(std::sync::atomic::Ordering::SeqCst) {
        notify(
            "Another usagio process owns refreshing; this menu updates from it on its own schedule",
        );
        return;
    }
    // Shares `poll_loop`'s `SwapGuard`
    // instead of a throwaway `SwapGuard::default()` — see
    // `shared_swap_guard`'s doc for why a manual click and the background
    // poller must never reason about anti-thrash cooldown independently.
    let guard = shared_swap_guard();
    try_start_refresh(move || {
        std::thread::spawn(move || {
            // Reset the in-flight flag on EVERY exit path — including a panic
            // inside run_cycle. Without this drop guard a panic would leave
            // REFRESH_IN_FLIGHT stuck `true`, permanently wedging the manual
            // "Refresh usage now" action (every later click hits the
            // already-running branch) until the app restarts. The background
            // poll_loop has a catch_unwind supervisor; this spawned thread
            // does not, so the guard is how the flag stays panic-safe.
            struct InFlightGuard;
            impl Drop for InFlightGuard {
                fn drop(&mut self) {
                    REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _reset = InFlightGuard;
            let (result, _trigger) = {
                let mut g = guard.lock().unwrap_or_else(|e| e.into_inner());
                run_cycle_checked(&mut g, true, &LoopOpts::default())
            };
            let outcome = match result {
                Ok(o) => o,
                Err(e) => {
                    notify(&format!("Refresh failed: {e}"));
                    return;
                }
            };
            let rate_limited = outcome.providers.iter().any(|p| p.rate_limited);
            if rate_limited {
                notify("Refresh: rate limited, backing off");
            } else {
                // Surface accounts that can't refresh at all — a dead login is
                // the reason the numbers won't move, so a bare "Usage refreshed"
                // would be a quiet lie for those rows.
                let fetched = outcome.providers.iter().map(|p| p.fetched).sum();
                let failed = outcome.providers.iter().map(|p| p.failed).sum();
                notify(&refresh_summary(
                    fetched,
                    failed,
                    &accounts_needing_relogin(),
                ));
            }
        });
    })
}

/// The "Refresh usage now" result toast. Per-account failures are logged and
/// skipped by the cycle, so they must be counted here, not read off `Ok`.
fn refresh_summary(fetched: usize, failed: usize, stuck: &[String]) -> String {
    let head = match (fetched, failed) {
        (0, f) if f > 0 => "Refresh failed — no account could be refreshed".to_string(),
        (_, 0) => "Usage refreshed".to_string(),
        (_, 1) => "Usage refreshed — 1 account could not be refreshed".to_string(),
        (_, f) => format!("Usage refreshed — {f} accounts could not be refreshed"),
    };
    match stuck {
        [] => head,
        [one] => format!("{head} — {one} needs re-login"),
        many => format!(
            "{head} — {} accounts need re-login: {}",
            many.len(),
            many.join(", ")
        ),
    }
}

/// CAS `REFRESH_IN_FLIGHT` from `false` to `true`; if it was already `true`
/// (a refresh is still running), notify and return without calling `spawn`.
/// Factored out from `handle_refresh_now` so a test can inject a counting
/// closure in place of a real `std::thread::spawn` and assert the second of
/// two rapid calls never invokes it.
fn try_start_refresh(spawn: impl FnOnce()) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        notify("Refresh already running");
        return;
    }
    spawn();
}

/// Capture the current login for `slug`. For Claude, use the full v1 flow
/// (persists into state.json + preserves cached usage). For every other
/// provider, dispatch through the trait (`capture_current_generic`), which
/// persists the account into `State::providers[slug]`.
fn handle_capture(slug: &str) {
    if slug != CLAUDE_SLUG && providers::get(slug).is_none() {
        notify(&format!(
            "Capture failed: provider '{slug}' is not registered"
        ));
        return;
    }
    // Capture makes the login active and may hit the network, so it runs like
    // a switch: off the UI thread, ordered against queued switch clicks.
    // A capture is never dropped by a later click: it is queued, bumps the
    // generation so OLDER switches yield, and whichever worker gets the swap
    // guard next (this one or a later switch) runs it first.
    PENDING_CAPTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .entry(slug.to_string())
        .and_modify(|n| *n += 1)
        .or_insert(1);
    let owned = slug.to_string();
    spawn_latest_switch(slug, move |_| run_pending_captures(&owned));
}

/// Captures clicked but not yet run, per provider.
static PENDING_CAPTURES: std::sync::Mutex<Option<std::collections::HashMap<String, usize>>> =
    std::sync::Mutex::new(None);

/// Run every queued capture for `slug`. Called with the swap guard held, by
/// the capture's own worker and by every switch worker before its gen check,
/// so a Switch clicked after a Capture always sees the captured account.
fn run_pending_captures(slug: &str) {
    let n = PENDING_CAPTURES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.remove(slug))
        .unwrap_or(0);
    for _ in 0..n {
        capture_serialized(slug);
    }
}

fn capture_serialized(slug: &str) {
    if slug == CLAUDE_SLUG {
        match capture_current() {
            Ok((email, existed)) => {
                notify(&format!(
                    "{} {email}",
                    if existed { "Refreshed" } else { "Captured" }
                ));
                // Capture makes this login the active account.
                request_poll_now();
            }
            Err(e) => notify(&format!("Capture failed: {e}")),
        }
        return;
    }
    let Some(provider) = providers::get(slug) else {
        return;
    };
    // State v2 (codex-switch-e2e): every non-Claude provider now has a real
    // multi-account slot (`State::providers[slug]`) to persist into, closing
    // the gap this used to leave as "persistence lands in a later phase".
    match capture_current_generic(slug) {
        Ok((key, existed)) => {
            notify(&format!(
                "{} {} {key}",
                if existed { "Refreshed" } else { "Captured" },
                provider.display_name()
            ));
            // Capture makes this login the provider's active account.
            request_poll_now();
        }
        Err(e) => notify(&format!("Capture failed: {e}")),
    }
}

/// Clicking a ready reset asks for confirmation before spending it. Claude
/// only; a click id for any other slug is ignored. Runs off the UI thread — the claim does a token
/// refresh, a profile fetch and a POST — and refreshes usage on success so the
/// cleared windows show up right away. This SPENDS a one-shot reset, so it only
/// ever runs from this explicit click, never automatically.
fn handle_claim(slug: &str, key: &str) {
    let grants = State::load()
        .ok()
        .and_then(|st| st.find(key).cloned())
        .and_then(|a| a.cached_usage)
        .map(|c| c.reported.reset_grants)
        .unwrap_or_default();
    handle_claim_with(
        slug,
        key,
        &grants,
        |question| confirm_with_button(question, "Use reset"),
        |key| {
            spawn_claim(key);
        },
    );
}

fn handle_claim_with(
    slug: &str,
    key: &str,
    grants: &[ResetGrant],
    ask: impl FnOnce(&str) -> bool,
    claim: impl FnOnce(String),
) {
    if slug == CLAUDE_SLUG && ask(&claim_question(key, grants)) {
        claim(key.to_string());
    }
}

/// Plain name for a limit key a reset clears.
fn limit_name(key: &str) -> String {
    match key {
        SESSION_LIMIT_KEY => "5-hour session limit".to_string(),
        WEEKLY_LIMIT_KEY => "weekly limit".to_string(),
        "seven_day_overage_included" => "extra-usage allowance".to_string(),
        other => other.replace('_', " "),
    }
}

/// The confirmation text: what the reset clears (with current usage), what it
/// leaves alone, and how many resets the account has left afterwards.
pub(super) fn claim_question(key: &str, grants: &[ResetGrant]) -> String {
    let Some(g) = grants.iter().find(|g| g.is_ready()) else {
        return format!("Use a reset on {key}?");
    };
    let kind = match (g.clears_session(), g.clears_weekly()) {
        (true, true) => "a session + weekly reset",
        (true, false) => "a session reset",
        (false, true) => "a weekly reset",
        (false, false) => "a reset",
    };
    let mut lines = vec![format!("Use {kind} on {key}?"), String::new()];
    // Skip empty side buckets (0%): clearing them changes nothing.
    for c in g.clears.iter().filter(|c| {
        c.key == SESSION_LIMIT_KEY || c.key == WEEKLY_LIMIT_KEY || c.pct.is_some_and(|p| p > 0.0)
    }) {
        let now = c
            .pct
            .map(|p| format!(" (now {}%)", p.round() as i64))
            .unwrap_or_default();
        lines.push(format!("Clears the {}{now}.", limit_name(&c.key)));
    }
    if !g.clears.is_empty() && !g.clears_weekly() {
        lines.push("Your weekly limit is not reset.".to_string());
    }
    let left: u32 = grants
        .iter()
        .filter(|g| g.is_available())
        .map(|g| g.resets_left)
        .sum();
    lines.push(String::new());
    lines.push(match left.saturating_sub(1) {
        0 => "This is the last reset on this account.".to_string(),
        1 => "1 reset will be left on this account.".to_string(),
        n => format!("{n} resets will be left on this account."),
    });
    lines.join("\n")
}

fn spawn_claim(key: String) {
    std::thread::spawn(move || {
        let res = crate::switching::claude::claim_claude_reset(&key);
        crate::logging::log(&format!(
            "event=reset_claim provider=claude account={key} outcome={}",
            match &res {
                Ok(crate::providers::claude::usage::ClaimOutcome::Reset) => "reset".to_string(),
                Ok(crate::providers::claude::usage::ClaimOutcome::NoChange(s)) => s.clone(),
                Err(e) => format!("error:{e}"),
            }
        ));
        // A capped account's cache is normally trusted until its reset time;
        // expire it so the next poll shows what the claim actually changed.
        if res.is_ok() {
            expire_cached_usage(&key);
        }
        match res {
            Ok(crate::providers::claude::usage::ClaimOutcome::Reset) => {
                notify(&format!("Reset used on {key} — limit cleared"));
                request_poll_now();
            }
            Ok(crate::providers::claude::usage::ClaimOutcome::NoChange(status)) => {
                notify(&format!("No reset applied for {key}: {status}"));
                request_poll_now();
            }
            Err(e) => notify(&format!("Claim failed for {key}: {e}")),
        }
    });
}

/// Mark `key`'s cached usage as old so the next poll refetches it even if it
/// reads as locked until reset, and drop auto-swap's leave memory for it.
fn expire_cached_usage(key: &str) {
    let res = crate::credentials::with_state_lock_absorb(|st| {
        if let Some(c) = st.find_mut(key).and_then(|a| a.cached_usage.as_mut()) {
            c.fetched_at = 0;
        }
        // Without this, an account auto-swap just left stays unservable for
        // the post-leave settle window even though its limit was cleared.
        if let Some(m) = st.swap_memory.get_mut(CLAUDE_SLUG) {
            m.note_reset_claimed(key);
        }
        Ok(())
    });
    if let Err(e) = res {
        crate::logging::log(&format!("expire_cached_usage({key}): {e}"));
    }
}

type UiTask = Box<dyn FnOnce() + Send>;

/// Work handed to the UI thread from a worker; drained by [`run_ui_tasks`].
static UI_TASKS: std::sync::Mutex<Vec<UiTask>> = std::sync::Mutex::new(Vec::new());

/// Queue `task` to run on the UI thread at its next event-loop tick.
pub(super) fn run_on_ui_thread(task: impl FnOnce() + Send + 'static) {
    UI_TASKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(task));
    // The Windows tray loop blocks between ticks; end its wait.
    crate::platform::wake_ui_thread();
}

/// Run the tasks queued by [`run_on_ui_thread`]. Called from every platform's
/// UI-thread event loop (the macOS tick, the GTK timeout, the Win32 pump).
pub(crate) fn run_ui_tasks() {
    let tasks = std::mem::take(&mut *UI_TASKS.lock().unwrap_or_else(|e| e.into_inner()));
    for task in tasks {
        task();
    }
}

/// Run `work` on a worker thread while holding the shared swap guard. The
/// poller holds that guard for a whole `run_cycle` (network fetches, token
/// refreshes), so taking it on the UI thread would freeze the tray; the
/// worker waits instead, keeping the serialization with the poller.
fn spawn_serialized_with_poller(work: impl FnOnce(&mut SwapGuards) + Send + 'static) {
    let shared = shared_swap_guard();
    crate::store::spawn_in_config_scope(move || {
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        work(&mut guard);
    });
}

fn handle_switch(slug: &str, key: &str) {
    // Explicit user choice may select an exhausted account. Serialize with the
    // poller so it cannot immediately undo the choice with an older decision;
    // off the UI thread, since the guard can be held across network fetches.
    let (slug, key) = (slug.to_string(), key.to_string());
    let provider = slug.clone();
    spawn_latest_switch(&provider, move |guard| {
        switch_serialized(guard, &slug, &key)
    });
}

/// Latest requested switch per provider. `std::sync::Mutex` is not FIFO, so
/// workers queued behind the poller can wake in any order; each carries the
/// generation it was requested at and skips if a newer click superseded it.
static SWITCH_GEN: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> =
    std::sync::Mutex::new(None);

fn spawn_latest_switch(slug: &str, work: impl FnOnce(&mut SwapGuards) + Send + 'static) {
    let gen = {
        let mut m = SWITCH_GEN.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.get_or_insert_with(Default::default);
        let g = n.entry(slug.to_string()).or_insert(0);
        *g += 1;
        *g
    };
    let slug = slug.to_string();
    spawn_serialized_with_poller(move |guard| {
        run_pending_captures(&slug);
        let current = SWITCH_GEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get(&slug).copied());
        if current == Some(gen) {
            work(guard);
        }
    });
}

fn switch_serialized(guard: &mut SwapGuards, slug: &str, key: &str) {
    // State v2 (codex-switch-e2e): dispatch by provider slug instead of
    // hard-gating on Claude — `capabilities().supports_switching` already
    // keeps the "Switch to this account" row from being built for a
    // non-switching provider, and `switch_to_provider_account` itself
    // re-checks that capability as belt-and-suspenders against a stray click id
    // shaped like `switch:<slug>:<key>` reaching this function directly.
    // `hold`: the choice is recorded in state.json, so the poller (and a
    // `usagio switch` from the CLI) honour it the same way.
    let result = if slug == CLAUDE_SLUG {
        switch_to(key, true)
    } else {
        switch_to_provider_account(slug, key, true)
    };
    match result {
        Ok(label) => {
            if crate::switching::provider::provider_switch_pending(slug, key) {
                notify(&format!("Switch to {label} queued until idle"));
            } else {
                let g = guard.for_provider(slug);
                g.last_swap = Some(std::time::Instant::now());
                notify(&format!("Switched to {label}"));
            }
            request_poll_now();
        }
        Err(e) => notify(&format!("Switch failed: {e}")),
    }
}

/// Handle a click on a "Paste API key ▸ <Provider>" row. The paste-a-key
/// dialog isn't wired yet — every API-key provider currently returns
/// `ProviderError::Unsupported` from `capture_api_key` — so surface a plain
/// "coming soon" notification instead of silently swallowing the click. Once
/// the paste-a-key dialog lands, this dispatches into `provider.capture_api_key`
/// with the user-entered nickname + key.
fn handle_apikey_capture(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        notify(&format!(
            "Paste API key: provider '{slug}' is not registered"
        ));
        return;
    };
    // Ask the provider for the capability so the outcome reflects the real
    // provider state — a future wire-up (exposing `ApiKeyCapture`) will just
    // skip the notify branch below without touching this handler.
    match require(provider.api_key_capture())
        .and_then(|c| c.capture_api_key(String::new(), String::new()))
    {
        Ok(acct) => notify(&format!(
            "Captured {} API-key account (persistence lands in a later phase): {}",
            provider.display_name(),
            acct.identity
                .email
                .clone()
                .or(acct.identity.display_name.clone())
                .unwrap_or_else(|| "<unnamed>".into()),
        )),
        Err(_) => notify(&format!(
            "{} API-key capture — coming soon",
            provider.display_name()
        )),
    }
}

fn handle_remove(slug: &str, key: &str) {
    if !confirm(&format!("Remove account {key}? This cannot be undone.")) {
        return;
    }
    // State v2 (codex-switch-e2e): a non-Claude provider's accounts can now
    // actually render a "Remove…" row (see `build_snapshot`), so route it to
    // the matching state.json bucket instead of the old blanket
    // "not yet supported" refusal.
    let result = if slug == CLAUDE_SLUG {
        remove_account(key)
    } else {
        remove_provider_account_generic(slug, key)
    };
    match result {
        Ok(_) => {
            notify(&format!("Removed {key}"));
            sweep_sign_in_stores_later();
        }
        Err(e) => notify(&format!("Remove failed: {e}")),
    }
}

// Context Ledger menu rendering + click handler removed in 0.4.2. The
// submenu shelled out via osascript to open a new Terminal window, which
// silently failed for anyone who hadn't granted Automation permission — a
// broken menu row is worse than no menu row. The CLI (`usagio context
// [--provider slug]`) is unchanged and remains the supported entry point.

/// Enable or disable auto-swap. Surfaces a save failure so the menu checkmark
/// and on-disk state can't silently disagree.
fn set_autoswap(enabled: bool) {
    let r = with_state_lock(|_| {
        let mut st = State::load()?;
        st.autoswap_disabled = !enabled;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save auto-swap setting: {e}"));
    }
}

/// Settings ▸ Auto-swap ▸ "Use credits when every plan is used up". Flips the
/// stored value under the lock (not the possibly stale menu snapshot's).
fn toggle_use_credits() {
    let r = with_state_lock(|_| {
        let mut st = State::load()?;
        st.use_credits_when_exhausted = !st.use_credits_when_exhausted;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save the credits setting: {e}"));
    }
}

/// Settings ▸ Tray Icon selection. `"at-risk"` clears the pin (the default —
/// tray follows the agent closest to its limit); any other value pins the tray
/// to that provider slug. Reads-then-writes under the lock so a stale menu
/// snapshot can't clobber a concurrent change.
fn set_tray_icon_mode(mode: &str) {
    let r = with_state_lock(|_| {
        let mut st = State::load()?;
        st.tray_icon_mode = if mode == "at-risk" {
            None
        } else {
            Some(mode.to_string())
        };
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save tray icon setting: {e}"));
    }
}

/// Flip one Settings ▸ Notifications ▸ per-trigger checkbox. `trigger` is one
/// of "threshold" / "resetback" / "pace" (the three click-id suffixes
/// `menu_tree_from_snapshot` wires up) — reads-then-flips the on-disk value so a stale
/// menu snapshot can't clobber a setting changed elsewhere in the meantime.
fn toggle_notification_trigger(trigger: &str) {
    let r = with_state_lock(|_| {
        let mut st = State::load()?;
        let cfg = &mut st.notification_config;
        match trigger {
            "threshold" => cfg.threshold_enabled = !cfg.threshold_enabled,
            "resetback" => cfg.reset_back_enabled = !cfg.reset_back_enabled,
            "pace" => cfg.pace_enabled = !cfg.pace_enabled,
            _ => {}
        }
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save notification setting: {e}"));
    }
}

/// Set the swap threshold AND enable auto-swap.
fn set_autoswap_threshold(v: f64) {
    let r = with_state_lock(|_| {
        let mut st = State::load()?;
        st.trigger_pct = Some(v);
        st.autoswap_disabled = false;
        st.save()
    });
    if let Err(e) = r {
        notify(&format!("Could not save threshold: {e}"));
    }
}

/// A native confirm dialog; true only if the user clicks the destructive
/// button. Cross-platform via `rfd::MessageDialog` (already a crate
/// dependency for the Backups Save…/Restore… file panels) rather than the
/// macOS-only `osascript -e 'display dialog …'` this used to shell out to —
/// `rfd::MessageDialog` has native backends on macOS (NSAlert), Windows
/// (MessageBoxW), and Linux (our xdg-portal `rfd` feature shells out to
/// `zenity(1)` for message dialogs specifically; if `zenity` isn't installed
/// the dialog fails closed to `Cancel`, so a missing binary can't accidentally
/// confirm a destructive action).
pub(super) fn confirm(question: &str) -> bool {
    confirm_with_button(question, "Remove")
}

/// What the user answered: `Some(true)` for `button`, `Some(false)` for any
/// other button, `None` when the dialog produced no button at all.
fn confirm_answer(result: rfd::MessageDialogResult, button: &str) -> Option<bool> {
    match result {
        rfd::MessageDialogResult::Custom(label) => Some(label == button),
        _ => None,
    }
}

fn confirm_with_button(question: &str, button: &str) -> bool {
    let result = rfd::MessageDialog::new()
        .set_title("usagio")
        .set_description(question)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            button.to_string(),
            "Cancel".to_string(),
        ))
        .show();
    // Both buttons are custom, so a non-button result means the dialog was
    // dismissed without one (Esc or close on Windows) or could not be shown at
    // all (Linux without zenity). Still fail closed, and say nothing changed
    // without claiming either cause.
    confirm_answer(result, button).unwrap_or_else(|| {
        crate::logging::log(&format!(
            "event=confirm_dialog_no_answer button={button:?} ; nothing changed"
        ));
        notify(
            "Nothing was changed — the confirmation closed without a choice \
             (if no dialog appeared on Linux, install zenity)",
        );
        false
    })
}

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;
