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
/// only; a click id for any other slug is ignored. `target` is
/// `<account>:<grant id>` (the grant the clicked row names), or just
/// `<account>` from a menu built before rows carried grant ids — then the
/// first ready grant is used. Runs off the UI thread — the claim does a token
/// refresh, a profile fetch and a POST — and refreshes usage on success so the
/// cleared windows show up right away. This SPENDS a one-shot reset, so it only
/// ever runs from this explicit click, never automatically.
fn handle_claim(slug: &str, target: &str) {
    let (key, grant_id) = split_claim_target(target);
    let grants = State::load()
        .ok()
        .and_then(|st| st.find(key).cloned())
        .and_then(|a| a.cached_usage)
        .map(|c| c.reported.reset_grants)
        .unwrap_or_default();
    let button = format!(
        "Use {}",
        chosen_grant(&grants, grant_id)
            .map(reset_name)
            .unwrap_or("reset")
    );
    handle_claim_with(
        slug,
        key,
        &grants,
        grant_id,
        |question, on_yes| confirm_then(question, &button, on_yes),
        spawn_claim,
    );
}

/// `<account>:<grant id>` → (account, Some(grant id)); a bare account → None.
/// Accounts are emails (no `:`); a grant id may contain one.
pub(super) fn split_claim_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((key, id)) if !id.is_empty() => (key, Some(id)),
        _ => (target.trim_end_matches(':'), None),
    }
}

/// The grant a click claims: the named one if it's still ready, else (old
/// click ids without a grant) the first ready grant.
pub(super) fn chosen_grant<'a>(
    grants: &'a [ResetGrant],
    grant_id: Option<&str>,
) -> Option<&'a ResetGrant> {
    match grant_id {
        Some(id) => grants.iter().find(|g| g.id == id && g.is_ready()),
        None => grants.iter().find(|g| g.is_ready()),
    }
}

/// "session reset", "session + weekly reset", "weekly reset", "extra-usage
/// reset" or "reset" — the same names the menu rows use.
fn reset_name(g: &ResetGrant) -> &'static str {
    match (g.clears_session(), g.clears_weekly()) {
        (true, true) => "session + weekly reset",
        (true, false) => "session reset",
        (false, true) => "weekly reset",
        (false, false) if g.clears.iter().any(|c| c.key == EXTRA_USAGE_KEY) => "extra-usage reset",
        (false, false) => "reset",
    }
}

/// The extra-usage allowance a Claude reset can clear.
const EXTRA_USAGE_KEY: &str = "seven_day_overage_included";

fn handle_claim_with(
    slug: &str,
    key: &str,
    grants: &[ResetGrant],
    grant_id: Option<&str>,
    ask: impl FnOnce(String, Box<dyn FnOnce() + Send>),
    claim: impl FnOnce(String, Option<String>) + Send + 'static,
) {
    if slug != CLAUDE_SLUG {
        return;
    }
    let key_owned = key.to_string();
    let grant = chosen_grant(grants, grant_id)
        .map(|g| g.id.clone())
        .or_else(|| grant_id.map(String::from));
    // `ask` runs `on_yes` only once the user confirms (later, on the UI thread).
    ask(
        claim_question(key, grants, grant_id),
        Box::new(move || claim(key_owned, grant)),
    );
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

/// The confirmation text: which reset, what it clears (with current usage),
/// what it leaves alone, a pointer to the session-only reset when the user
/// picked a session + weekly one while both are banked, and what stays banked.
pub(super) fn claim_question(key: &str, grants: &[ResetGrant], grant_id: Option<&str>) -> String {
    let Some(g) = chosen_grant(grants, grant_id) else {
        return format!("Use a reset on {key}?");
    };
    let mut lines = vec![
        format!("Use the {} on {key}?", reset_name(g)),
        String::new(),
    ];
    // The session and weekly limits; a side bucket (extra usage) only when
    // it's all this reset clears.
    let main_limits = g.clears_session() || g.clears_weekly();
    for c in g
        .clears
        .iter()
        .filter(|c| c.key == SESSION_LIMIT_KEY || c.key == WEEKLY_LIMIT_KEY || !main_limits)
    {
        let now = c
            .pct
            .map(|p| format!(" (now {}%)", p.round() as i64))
            .unwrap_or_default();
        lines.push(format!("Clears the {}{now}.", limit_name(&c.key)));
    }
    if !g.clears.is_empty() && !g.clears_weekly() {
        // The weekly % comes from any grant that lists it.
        let weekly = grants
            .iter()
            .flat_map(|o| o.clears.iter())
            .find(|c| c.key == WEEKLY_LIMIT_KEY)
            .and_then(|c| c.pct)
            .map(|p| format!(" (now {}%)", p.round() as i64))
            .unwrap_or_default();
        lines.push(format!("Your weekly limit{weekly} is not reset."));
    }
    let cheaper = g.clears_weekly()
        && g.clears_session()
        && grants
            .iter()
            .any(|o| o.id != g.id && o.is_ready() && o.clears_session() && !o.clears_weekly());
    if cheaper {
        lines.push(String::new());
        lines.push("Only need the session cleared? Use the session reset".to_string());
        lines.push("instead and keep this one for the weekly limit.".to_string());
    }
    // What stays banked: every available grant, minus the one being spent.
    let mut left: Vec<(&'static str, u32)> = Vec::new();
    for o in grants.iter().filter(|o| o.is_available()) {
        let n = if o.id == g.id {
            o.resets_left - 1
        } else {
            o.resets_left
        };
        if n == 0 {
            continue;
        }
        let name = reset_name(o);
        match left.iter_mut().find(|(k, _)| *k == name) {
            Some((_, c)) => *c += n,
            None => left.push((name, n)),
        }
    }
    lines.push(String::new());
    if left.is_empty() {
        lines.push("This is the last reset on this account.".to_string());
    } else {
        let parts: Vec<String> = left
            .iter()
            .map(|(name, n)| {
                if *n == 1 {
                    format!("1 {name}")
                } else {
                    format!("{n} {name}s")
                }
            })
            .collect();
        lines.push(format!("Still banked after this: {}.", parts.join(", ")));
    }
    lines.join("\n")
}

/// What a successful claim cleared, for the notification.
fn cleared_phrase(cleared: &[String]) -> String {
    let names: Vec<String> = cleared
        .iter()
        .filter(|k| *k == SESSION_LIMIT_KEY || *k == WEEKLY_LIMIT_KEY)
        .map(|k| limit_name(k))
        .collect();
    match names.len() {
        0 if cleared.iter().any(|k| k == EXTRA_USAGE_KEY) => {
            "extra-usage allowance cleared".to_string()
        }
        0 => "limit cleared".to_string(),
        1 => format!("{} cleared", names[0]),
        _ => format!("{} cleared", names.join(" and ")),
    }
}

fn spawn_claim(key: String, grant_id: Option<String>) {
    std::thread::spawn(move || {
        use crate::providers::claude::usage::ClaimOutcome;
        let before = State::load()
            .ok()
            .and_then(|st| st.find(&key).and_then(|a| a.cached_usage.clone()));
        let res = crate::switching::claude::claim_claude_reset(&key, grant_id.as_deref());
        log_claim_decision(&key, grant_id.as_deref(), before.as_ref(), &res);
        crate::logging::log(&format!(
            "event=reset_claim provider=claude account={key} grant={} outcome={}",
            grant_id.as_deref().unwrap_or("-"),
            match &res {
                Ok(ClaimOutcome::Reset { .. }) => "reset".to_string(),
                Ok(ClaimOutcome::NoChange(s)) => s.clone(),
                Err(e) => format!("error:{e}"),
            }
        ));
        // A capped account's cache is normally trusted until its reset time;
        // expire it so the next poll shows what the claim actually changed.
        if res.is_ok() {
            expire_cached_usage(&key);
        }
        match res {
            Ok(ClaimOutcome::Reset { cleared }) => {
                notify(&format!(
                    "Reset used on {key} — {}",
                    cleared_phrase(&cleared)
                ));
                request_poll_now();
            }
            Ok(ClaimOutcome::NoChange(status)) => {
                notify(&format!("No reset applied for {key}: {status}"));
                request_poll_now();
            }
            Err(e) => notify(&format!("Claim failed for {key}: {e}")),
        }
    });
}

/// A decisions-log line for a reset claim: the grant, the outcome, what the
/// server says it cleared, and the account's last reading before the claim
/// (every bucket, its reset time and the banked grants). The forced poll after
/// a claim writes the "after" reading to the history log, so the two together
/// show what a claim of this grant family really does.
fn log_claim_decision(
    key: &str,
    grant_id: Option<&str>,
    before: Option<&crate::store::CachedUsage>,
    res: &Result<crate::providers::claude::usage::ClaimOutcome, String>,
) {
    use crate::providers::claude::usage::ClaimOutcome;
    let (outcome, cleared, error) = match res {
        Ok(ClaimOutcome::Reset { cleared }) => ("reset".to_string(), cleared.clone(), None),
        Ok(ClaimOutcome::NoChange(s)) => (s.clone(), Vec::new(), None),
        Err(e) => ("error".to_string(), Vec::new(), Some(e.clone())),
    };
    let v = serde_json::json!({
        "ts": chrono::Utc::now().timestamp(),
        "provider": CLAUDE_SLUG,
        "engine": "claim",
        "action": "claim",
        "reason": "manual",
        "target": key,
        "grant": grant_id,
        "outcome": outcome,
        "cleared": cleared,
        "error": error,
        "before": before,
    });
    if let Err(e) = crate::usage_log::append_decision(&v) {
        crate::logging::log(&format!("decisions log append failed: {e:#}"));
    }
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
    let (slug, key) = (slug.to_string(), key.to_string());
    confirm_then(
        format!("Remove account {key}? This cannot be undone."),
        "Remove",
        move || remove_confirmed(&slug, &key),
    );
}

fn remove_confirmed(slug: &str, key: &str) {
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

/// Ask `question` in a native confirm dialog, then — only if the user clicks
/// `button` — run `on_yes` on the UI thread. Returns at once: the dialog is
/// asked on a worker thread so the UI thread's run loop keeps turning while it
/// is open. Asking on the UI thread froze the app: on macOS `rfd`'s blocking
/// `MessageDialog::show()` runs `CFUserNotificationDisplayAlert` on the main
/// thread, and with the run loop stopped none of the alert's buttons (Cancel
/// included) did anything until the app was killed.
pub(super) fn confirm_then(question: String, button: &str, on_yes: impl FnOnce() + Send + 'static) {
    confirm_then_with(question, button.to_string(), ask_blocking, on_yes);
}

/// [`confirm_then`] with the dialog injected, for tests.
fn confirm_then_with(
    question: String,
    button: String,
    ask: impl FnOnce(&str, &str) -> bool + Send + 'static,
    on_yes: impl FnOnce() + Send + 'static,
) {
    std::thread::spawn(move || {
        if ask(&question, &button) {
            run_on_ui_thread(on_yes);
        }
    });
}

/// What the user answered: `Some(true)` for `button`, `Some(false)` for any
/// other button, `None` when the dialog produced no button at all.
fn confirm_answer(result: rfd::MessageDialogResult, button: &str) -> Option<bool> {
    match result {
        rfd::MessageDialogResult::Custom(label) => Some(label == button),
        _ => None,
    }
}

/// Show the dialog and wait for the answer. Worker threads only (see
/// [`confirm_then`]). Cross-platform via `rfd` — NSAlert-style user
/// notification on macOS, MessageBoxW / TaskDialog on Windows, and
/// `zenity(1)` on Linux (our xdg-portal `rfd` feature); if `zenity` isn't
/// installed the dialog fails closed, so a missing binary can't accidentally
/// confirm a destructive action.
fn ask_blocking(question: &str, button: &str) -> bool {
    let buttons = rfd::MessageButtons::OkCancelCustom(button.to_string(), "Cancel".to_string());
    // macOS: the async dialog shows its alert on rfd's own thread. The sync
    // `show()` would hop back onto the main thread and freeze it again.
    #[cfg(target_os = "macos")]
    let result = block_on(
        rfd::AsyncMessageDialog::new()
            .set_title("usagio")
            .set_description(question)
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(buttons)
            .show(),
    );
    #[cfg(not(target_os = "macos"))]
    let result = rfd::MessageDialog::new()
        .set_title("usagio")
        .set_description(question)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(buttons)
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

/// Run `fut` to completion on this thread, parking between polls. Enough for
/// the one future it drives (rfd's dialog answer, completed by rfd's own
/// thread), so no async runtime is pulled in for it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// Serializes the tests that drain the shared UI task queue.
#[cfg(test)]
pub(super) static UI_TASKS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;
