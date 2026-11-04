//! Threshold / reset-back / weekly-pace notifications driven by the watch
//! cycle. `evaluate` is pure over a (prev, curr) usage-log snapshot pair;
//! `dedup_and_apply` walks the resulting `Vec<Trigger>` against a small
//! per-account `NotifState` so a crossing that already fired doesn't re-fire
//! every poll, and so a genuine window reset re-arms the same thresholds for
//! the next cycle. `fire` hands the formatted (summary, body) to notify-rust.
//!
//! The module is deliberately unopinionated about where `NotifState` lives —
//! v1 hangs it off `store::Account` (see the `notif_state` field), a future
//! phase can move it into the StateV2 per-account bucket instead. The pure
//! `evaluate` / `dedup_and_apply` split lets tests exercise the crossing
//! algebra without any I/O; `fire` is the only function that touches DBus /
//! NSUserNotification.

use std::collections::BTreeSet;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::trait_def::ResetGrant;
use crate::usage_log::{AccountKey, PaceEstimate, Snapshot};

/// Which usage window a trigger pertains to. Serialized as a kebab-case
/// string so persisted state (crossings set on `NotifState`) is
/// human-inspectable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Window {
    Session,
    Weekly,
}

impl Window {
    /// Short human label used in notification bodies.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Window::Session => "Session (5h)",
            Window::Weekly => "Weekly (7d)",
        }
    }
}

/// One notification-worthy event. `Threshold` carries the exact percent that
/// was crossed so the same window can fire twice per cycle (e.g. 70 then 90).
/// `WeeklyPace` carries the projected 100%-hit time so the body can name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The `window`'s utilization just crossed `pct` upward (typ. 70 or 90).
    Threshold { window: Window, pct: u8 },
    /// The `window` just reset — utilization dropped from a non-trivial prior
    /// level, so the user has fresh headroom.
    ResetBack { window: Window },
    /// Weekly pace projects hitting 100% at `hit_time` before the weekly
    /// reset. Fires at most once per weekly window.
    WeeklyPace { hit_time: DateTime<Utc> },
    /// A one-shot usage-limit reset grant just appeared on this account (the
    /// "hey, you got a reset!" moment). Fires once per grant id. `label` is the
    /// provider's description of the grant.
    ResetGrantAvailable { label: String },
    /// A banked reset is now usable right now — the account is at a limit and
    /// the reset can be spent to unblock it. Fires once per grant id.
    ResetUsableNow { label: String },
}

/// Per-trigger enable flags. Thresholds and reset-back default on; pace is
/// experimental and defaults off (the user opts in from Settings ▸
/// Notifications).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NotificationConfig {
    pub(crate) threshold_enabled: bool,
    pub(crate) reset_back_enabled: bool,
    pub(crate) pace_enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        NotificationConfig {
            threshold_enabled: true,
            reset_back_enabled: true,
            pace_enabled: false,
        }
    }
}

/// Persisted per-account bookkeeping used to dedup fires across ticks. Kept
/// tiny (a `BTreeSet` and a `bool`) so hanging it off every `Account` in
/// state.json is cheap. `BTreeSet` (not `HashSet`) so JSON output is
/// deterministic across saves — a state.json diff shouldn't churn on set
/// iteration order.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NotifState {
    /// Thresholds already fired since the last reset of their window.
    #[serde(default)]
    pub(crate) crossings: BTreeSet<(Window, u8)>,
    /// True if this account has already fired a WeeklyPace notification
    /// during its current weekly window.
    #[serde(default)]
    pub(crate) pace_fired_this_window: bool,
    /// The login expiry (unix millis) already warned about. A renewal moves
    /// the expiry, so the next approach warns again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) login_expiry_warned: Option<i64>,
    /// Reset-grant ids we've already announced ("you got a reset!"), so the
    /// arrival notification fires once per grant, not every poll.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) reset_grants_seen: BTreeSet<String>,
    /// Reset-grant ids we've already announced as usable-now, so the
    /// "claim it now" notification fires once per grant.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) reset_grants_usable_fired: BTreeSet<String>,
    /// State-driven notifications whose one retry after a failed delivery has
    /// been spent ("pace", "avail:<id>", "usable:<id>"); a second failure
    /// commits the dedup bit instead of retrying forever.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) retry_spent: BTreeSet<String>,
}

/// Thresholds we check on every tick, ascending so a `Vec<Trigger>` from
/// `evaluate` is deterministically ordered (session-70, session-90,
/// weekly-70, weekly-90). Keeping this a `const` (not a config field) is
/// intentional — the "70 / 90" ladder is the product spec; users tune what's
/// enabled via `NotificationConfig`, not which numbers are checked.
const THRESHOLDS: [u8; 2] = [70, 90];

/// The menu-bar "Threshold alerts (…)" checkbox label, derived from
/// `THRESHOLDS` so the UI can never drift from the numbers actually checked.
/// Length-agnostic: if the ladder gains/loses a rung the label follows. Both
/// the macOS and cross-platform menu renderers call this rather than
/// hard-coding "70% / 90%".
pub(crate) fn threshold_alert_label() -> String {
    let pcts = THRESHOLDS
        .iter()
        .map(|t| format!("{t}%"))
        .collect::<Vec<_>>()
        .join(" / ");
    format!("Threshold alerts ({pcts})")
}

// ---------------------------------------------------------------------------
// Pure algebra: evaluate + dedup_and_apply
// ---------------------------------------------------------------------------

/// Pure: raw candidate triggers implied by the (prev, curr) transition, in
/// the order the menu should surface them. Threshold crossings come first
/// (session before weekly, ascending pct within each window), then
/// ResetBacks (session before weekly). `WeeklyPace` is *not* considered here
/// — it lives on the pace-estimator path (`evaluate_pace`) because it needs
/// history beyond the last tick.
///
/// A field that's `None` on either side of the pair produces no trigger for
/// that window — first-boot after a capture shouldn't blast the user with
/// "you crossed 70" for percentages that were simply unknown before.
pub(crate) fn evaluate(prev: &Snapshot, curr: &Snapshot, cfg: &NotificationConfig) -> Vec<Trigger> {
    let mut out = Vec::new();
    let pairs = [
        (Window::Session, prev.session_pct, curr.session_pct),
        (Window::Weekly, prev.weekly_pct, curr.weekly_pct),
    ];
    if cfg.threshold_enabled {
        for (w, p, c) in pairs {
            let (Some(p), Some(c)) = (p, c) else { continue };
            // A pair spanning a reset: the new window started from ~0, so
            // judge crossings against that, not the pre-reset level.
            let p = if is_reset(prev, curr, w) { 0.0 } else { p };
            for pct in THRESHOLDS {
                let t = pct as f32;
                if p < t && c >= t {
                    out.push(Trigger::Threshold { window: w, pct });
                }
            }
        }
    }
    if cfg.reset_back_enabled {
        for (w, p, c) in pairs {
            // Only a window that was meaningfully used announces fresh
            // headroom; a rollover of a barely-used window still re-arms
            // crossings (dedup_and_apply) but says nothing.
            let (Some(p), Some(_)) = (p, c) else { continue };
            if p > 50.0 && is_reset(prev, curr, w) {
                out.push(Trigger::ResetBack { window: w });
            }
        }
    }
    out
}

/// True iff the (prev, curr) percentages for one window describe a reset:
/// utilization dropped from a non-trivial prior level. The 50-percent floor
/// keeps normal noise (small fluctuations near zero) from being mistaken for
/// a reset — a genuine reset lands a high percentage back at (near-)zero, so
/// the drop must also be substantial (>= 20 points) or land under 10%.
fn is_window_reset(prev_pct: f32, curr_pct: f32) -> bool {
    prev_pct > 50.0 && curr_pct < prev_pct && (prev_pct - curr_pct >= 20.0 || curr_pct < 10.0)
}

/// True iff `curr` shows a reset for `w` relative to `prev`: the window's
/// reported reset time moved later, the old one was still ahead when `prev`
/// was read and had passed by `curr`, or (when reset times are missing or
/// unchanged) the pct heuristic above. A reset stamp already past at
/// `prev.ts` is not a credible upcoming reset: a vendor that reports "now +
/// 0 s" for an expired window would otherwise roll on every poll.
/// Public so the watch loop can query it independently of `evaluate` (e.g. to
/// log a reset even when `reset_back_enabled` is off).
fn is_reset(prev: &Snapshot, curr: &Snapshot, w: Window) -> bool {
    let (p, c, pr, cr) = match w {
        Window::Session => (
            prev.session_pct,
            curr.session_pct,
            prev.session_reset,
            curr.session_reset,
        ),
        Window::Weekly => (
            prev.weekly_pct,
            curr.weekly_pct,
            prev.weekly_reset,
            curr.weekly_reset,
        ),
    };
    let (Some(p), Some(c)) = (p, c) else {
        return false;
    };
    let rolled =
        matches!((pr, cr), (Some(pr), Some(cr)) if pr < cr && pr > prev.ts && pr <= curr.ts);
    rolled || is_window_reset(p, c)
}

/// Filter `triggers` against `state`, updating `state` in place. A Threshold
/// already present in `state.crossings` is dropped; the rest are recorded so
/// the next tick doesn't refire them. A ResetBack for a window first clears
/// that window's crossings (so a subsequent 70/90 crossing re-fires as
/// intended), and — for a weekly reset — clears `pace_fired_this_window` too.
/// Reset detection uses the (prev, curr) pair directly (not just the
/// Trigger stream) so it fires even when `cfg.reset_back_enabled` is false.
pub(crate) fn dedup_and_apply(
    state: &mut NotifState,
    prev: &Snapshot,
    curr: &Snapshot,
    triggers: Vec<Trigger>,
) -> Vec<Trigger> {
    // Reset detection first so a Threshold trigger in the same tick as a
    // reset (rare in practice — resets zero-out the window) still fires.
    if is_reset(prev, curr, Window::Session) {
        state.crossings.retain(|(w, _)| *w != Window::Session);
    }
    if is_reset(prev, curr, Window::Weekly) {
        state.crossings.retain(|(w, _)| *w != Window::Weekly);
        state.pace_fired_this_window = false;
        state.retry_spent.remove("pace");
    }
    let mut kept = Vec::with_capacity(triggers.len());
    for t in triggers {
        match &t {
            Trigger::Threshold { window, pct } => {
                if state.crossings.contains(&(*window, *pct)) {
                    continue;
                }
                state.crossings.insert((*window, *pct));
                kept.push(t);
            }
            Trigger::ResetBack { .. } => kept.push(t),
            Trigger::WeeklyPace { .. } => {
                // Guard against a caller that queued a pace trigger without
                // going through `evaluate_pace` (which already dedups). On the
                // weekly reset tick the pace fit still covers last week's
                // samples (the new window has none yet), so it is dropped
                // without spending the new window's one alert.
                if state.pace_fired_this_window || is_reset(prev, curr, Window::Weekly) {
                    continue;
                }
                state.pace_fired_this_window = true;
                kept.push(t);
            }
            // Reset-grant triggers are deduped at their source
            // (`reset_grant_triggers`), not here — pass them through unchanged.
            Trigger::ResetGrantAvailable { .. } | Trigger::ResetUsableNow { .. } => kept.push(t),
        }
    }
    kept
}

/// Weekly-pace evaluation. Fires at most once per weekly window (deduped by
/// `NotifState.pace_fired_this_window`). Requires `cfg.pace_enabled`, a
/// pace estimate with sane confidence and a strictly-positive slope, a known
/// current weekly percentage below 100, and a projected hit-time strictly
/// after `now` (a projection into the past means the fit is degenerate).
/// Returns the trigger to consider firing; the caller still runs it through
/// `dedup_and_apply` to actually flip the "fired" bit.
pub(crate) fn evaluate_pace(
    pace: Option<&PaceEstimate>,
    curr: &Snapshot,
    cfg: &NotificationConfig,
    now: DateTime<Utc>,
) -> Option<Trigger> {
    if !cfg.pace_enabled {
        return None;
    }
    let pace = pace?;
    if pace.confidence < 0.5 || pace.slope_pct_per_hour <= 0.0 {
        return None;
    }
    let cur_pct = curr.weekly_pct? as f64;
    if !(0.0..100.0).contains(&cur_pct) {
        return None;
    }
    let hours_to_100 = (100.0 - cur_pct) / pace.slope_pct_per_hour;
    if !hours_to_100.is_finite() || hours_to_100 <= 0.0 {
        return None;
    }
    // Millis (not seconds) so a fractional hour doesn't quantize to whole
    // seconds — the printed body ends up wrong by up to 59s otherwise.
    let millis = (hours_to_100 * 3_600_000.0) as i64;
    let hit = now.checked_add_signed(Duration::milliseconds(millis))?;
    // The window resets before the projection lands: nothing to warn about.
    if curr.weekly_reset.is_some_and(|r| hit >= r) {
        return None;
    }
    Some(Trigger::WeeklyPace { hit_time: hit })
}

/// Undo the dedup bit a state-driven trigger set, after its delivery failed,
/// so the next tick retries it. `grant_id` is the grant a reset-grant trigger
/// was raised for. Threshold/reset crossings are edge-triggered on the
/// (prev, curr) pair, which advances regardless, so they are not retried
/// (fire is documented best-effort).
pub(crate) fn release_dedup(state: &mut NotifState, trigger: &Trigger, grant_id: Option<&str>) {
    let key = match (trigger, grant_id) {
        (Trigger::WeeklyPace { .. }, _) => "pace".to_string(),
        (Trigger::ResetGrantAvailable { .. }, Some(id)) => format!("avail:{id}"),
        (Trigger::ResetUsableNow { .. }, Some(id)) => format!("usable:{id}"),
        _ => return,
    };
    // One retry per event: the second failure keeps the dedup bit set, so a
    // permanently failing backend (or a late-erroring one that already showed
    // the alert) cannot cause a storm.
    if !state.retry_spent.insert(key) {
        return;
    }
    match (trigger, grant_id) {
        (Trigger::WeeklyPace { .. }, _) => state.pace_fired_this_window = false,
        (Trigger::ResetGrantAvailable { .. }, Some(id)) => {
            state.reset_grants_seen.remove(id);
        }
        (Trigger::ResetUsableNow { .. }, Some(id)) => {
            state.reset_grants_usable_fired.remove(id);
        }
        _ => {}
    }
}

/// Reset-grant notifications for this tick, deduped in place against `state`.
/// Two moments, each fired once per grant id:
///   * arrival — a grant with a reset left that we've never announced
///     (`ResetGrantAvailable`);
///   * usable — a grant that can be spent right now (`ResetUsableNow`), the
///     "you're blocked and have a reset to claim" alert.
///
/// Edge-triggered purely through `state`, so it needs no prev snapshot and is
/// correct even on the first poll after launch. Ids are never pruned: a grant
/// that's been spent drops out of `grants` (its `resets_left` hits 0) and
/// shouldn't re-announce if it transiently reappears.
pub(crate) fn reset_grant_triggers(state: &mut NotifState, grants: &[ResetGrant]) -> Vec<Trigger> {
    let mut out = Vec::new();
    for g in grants.iter().filter(|g| g.is_available()) {
        if state.reset_grants_seen.insert(g.id.clone()) {
            out.push(Trigger::ResetGrantAvailable {
                label: g.label.clone(),
            });
        }
        if g.usable_now && state.reset_grants_usable_fired.insert(g.id.clone()) {
            out.push(Trigger::ResetUsableNow {
                label: g.label.clone(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Presentation + delivery
// ---------------------------------------------------------------------------

/// Golden-tested (summary, body) pair for a Trigger. Kept separate from `fire`
/// so tests can assert the exact wording without dragging in notify-rust.
fn format_message(trigger: &Trigger, account: &AccountKey) -> (String, String) {
    let summary = format!("{} · {}", account.provider, account.account);
    let body = match trigger {
        Trigger::Threshold { window, pct } => {
            format!("{} usage crossed {pct}%", window.label())
        }
        Trigger::ResetBack { window } => {
            format!("{} window reset — headroom restored", window.label())
        }
        Trigger::WeeklyPace { hit_time } => {
            format!(
                "Weekly on pace to hit 100% at {}",
                hit_time.format("%Y-%m-%d %H:%M UTC")
            )
        }
        Trigger::ResetGrantAvailable { label } => {
            if label.is_empty() {
                "🎁 You got a usage-limit reset! Claim it from usagio when you're blocked."
                    .to_string()
            } else {
                format!("🎁 You got a usage-limit reset! {label}")
            }
        }
        Trigger::ResetUsableNow { .. } => {
            "You're out of usage — claim your banked reset now from usagio.".to_string()
        }
    };
    (summary, body)
}

/// Send a desktop notification for `trigger`. Best-effort from the caller's
/// perspective — the watch loop swallows the Err so a broken dbus connection
/// can't stall the poll cycle. Runs synchronously; notify-rust's own thread
/// model varies by backend and we don't need async here.
///
/// Under `cfg(test)` this is a no-op (`NullNotifier`, below) rather than the
/// real `notify_rust` call: `cargo test` must never pop a real Notification
/// Center banner. Every test that wants to assert on what *would* have been
/// shown should assert against `format_message` directly instead — it's the
/// pure half of this function and is exercised by the golden-message tests
/// further down.
pub(crate) fn fire(trigger: &Trigger, account: &AccountKey) -> Result<()> {
    let (summary, body) = format_message(trigger, account);
    null_notifier_or_real(&summary, &body)
}

// The real notifier routes through the `Platform` trait's `notify`, which fires
// via notify-rust on macOS/Linux and is a no-op on Windows (where notify-rust's
// WinRT toast backend can't be linked — see `Platform::notify` and Cargo.toml).
// Keeping OS-specificity in `src/platform/*` means this module carries no
// `#[cfg(target_os)]` of its own.
#[cfg(not(test))]
fn null_notifier_or_real(summary: &str, body: &str) -> Result<()> {
    crate::platform::current().notify(summary, body)
}

/// `NullNotifier`: the `cfg(test)` stand-in for the real notify-rust call.
/// Deliberately swallows `summary`/`body` rather than firing anything — see
/// the doc comment on `fire` above.
#[cfg(test)]
fn null_notifier_or_real(_summary: &str, _body: &str) -> Result<()> {
    Ok(())
}

/// Fire a plain "usagio: <msg>" notification, without going through the
/// `Trigger`/`AccountKey` pipeline. Used for one-off diagnostics (auto-swap,
/// stuck-account, migration failures) from `main::notify`. Routes through the
/// `Platform` trait's `notify` (notify-rust on macOS/Linux; a deliberate no-op
/// on Windows, see the note above).
fn fire_plain(msg: &str) -> Result<()> {
    null_notifier_or_real("usagio", msg)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Fire a native "usagio: {msg}" notification (best effort, cross-platform).
/// Delegates to `notifications::fire_plain`, which routes through notify-rust
/// so macOS/Linux/Windows all get a real notification — the previous osascript
/// path silently no-op'd on Linux and Windows. Best-effort: failures are
/// logged (R2-EH-04) so H6's user-facing notification guarantee has
/// diagnostic backing when the channel is unavailable (headless SSH, no
/// notification daemon, denied permission, etc.).
pub(crate) fn notify(msg: &str) {
    if let Err(e) = fire_plain(msg) {
        crate::logging::log(&format!("notify: fire_plain failed: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn snap(session: Option<f32>, weekly: Option<f32>) -> Snapshot {
        Snapshot {
            ts: Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            provider: "claude".to_string(),
            account: "a@x.io".to_string(),
            session_pct: session,
            weekly_pct: weekly,
            active_model: None,
            ..Default::default()
        }
    }

    // --- evaluate: no crossing produces nothing ------------------------------

    #[test]
    fn evaluate_no_crossing_returns_empty() {
        let prev = snap(Some(40.0), Some(30.0));
        let curr = snap(Some(45.0), Some(35.0));
        let cfg = NotificationConfig::default();
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    // --- evaluate: a single upward crossing --------------------------------

    #[test]
    fn evaluate_upward_70_produces_one_threshold() {
        let prev = snap(Some(50.0), Some(30.0));
        let curr = snap(Some(75.0), Some(35.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70,
            }]
        );
    }

    // --- evaluate: both 70 and 90 in one tick, ordered ---------------------

    #[test]
    fn evaluate_both_70_and_90_same_tick_are_ordered_70_then_90() {
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(95.0), None);
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 70
                },
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 90
                },
            ]
        );
    }

    // --- evaluate: session and weekly both cross, session-first ------------

    #[test]
    fn evaluate_orders_session_before_weekly() {
        let prev = snap(Some(50.0), Some(50.0));
        let curr = snap(Some(75.0), Some(75.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![
                Trigger::Threshold {
                    window: Window::Session,
                    pct: 70
                },
                Trigger::Threshold {
                    window: Window::Weekly,
                    pct: 70
                },
            ]
        );
    }

    // --- evaluate: missing prev/curr percent for a window is a no-op --------

    #[test]
    fn evaluate_skips_window_when_either_side_is_none() {
        let prev = snap(None, Some(50.0));
        let curr = snap(Some(75.0), Some(75.0));
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        // Session had `None` on prev → no session trigger; weekly is fine.
        assert_eq!(
            ts,
            vec![Trigger::Threshold {
                window: Window::Weekly,
                pct: 70
            }]
        );
    }

    // --- evaluate: disable flags gate their triggers -----------------------

    #[test]
    fn evaluate_respects_threshold_disabled_flag() {
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let cfg = NotificationConfig {
            threshold_enabled: false,
            ..NotificationConfig::default()
        };
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    #[test]
    fn evaluate_reset_back_produces_reset_trigger() {
        let prev = snap(Some(85.0), None);
        let curr = snap(Some(5.0), None);
        let cfg = NotificationConfig::default();
        let ts = evaluate(&prev, &curr, &cfg);
        assert_eq!(
            ts,
            vec![Trigger::ResetBack {
                window: Window::Session,
            }]
        );
    }

    #[test]
    fn evaluate_small_drop_is_not_a_reset() {
        // Drop is real but prev was <= 50; that's noise, not a window reset.
        let prev = snap(Some(45.0), None);
        let curr = snap(Some(5.0), None);
        let cfg = NotificationConfig::default();
        assert!(evaluate(&prev, &curr, &cfg).is_empty());
    }

    // --- dedup: same crossing fires only once per window --------------------

    #[test]
    fn dedup_prevents_a_threshold_from_firing_twice() {
        let mut state = NotifState::default();
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let cfg = NotificationConfig::default();

        let first = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert_eq!(first.len(), 1, "first crossing must fire");

        // A follow-up tick that stays above 70 without crossing again — the
        // pair itself produces no new Threshold from `evaluate`, and even if
        // it did, `state.crossings` would suppress it.
        let curr2 = snap(Some(80.0), None);
        let second = dedup_and_apply(&mut state, &curr, &curr2, evaluate(&curr, &curr2, &cfg));
        assert!(second.is_empty(), "same threshold must not re-fire");
        assert!(state.crossings.contains(&(Window::Session, 70)));
    }

    // --- reset: crossings for the reset window are cleared, then re-arm ----

    #[test]
    fn reset_clears_crossings_and_lets_threshold_refire() {
        let mut state = NotifState::default();
        let cfg = NotificationConfig::default();
        // 1st tick: cross 70.
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let _ = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert!(state.crossings.contains(&(Window::Session, 70)));

        // 2nd tick: session resets (drop from 75 to 5) → crossings clear +
        // a ResetBack fires.
        let reset = snap(Some(5.0), None);
        let fired = dedup_and_apply(&mut state, &curr, &reset, evaluate(&curr, &reset, &cfg));
        assert_eq!(
            fired,
            vec![Trigger::ResetBack {
                window: Window::Session,
            }]
        );
        assert!(state.crossings.is_empty(), "reset clears session crossings");

        // 3rd tick: cross 70 again — must fire again because we re-armed.
        let climb_lo = snap(Some(40.0), None);
        let climb_hi = snap(Some(75.0), None);
        let fired = dedup_and_apply(
            &mut state,
            &climb_lo,
            &climb_hi,
            evaluate(&climb_lo, &climb_hi, &cfg),
        );
        assert_eq!(
            fired,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70
            }]
        );
    }

    #[test]
    fn reset_rearms_thresholds_even_when_reset_back_is_disabled() {
        let mut state = NotifState::default();
        let cfg = NotificationConfig {
            reset_back_enabled: false,
            ..NotificationConfig::default()
        };
        let prev = snap(Some(50.0), None);
        let curr = snap(Some(75.0), None);
        let _ = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert!(state.crossings.contains(&(Window::Session, 70)));

        // Reset tick: no ResetBack notification (disabled), but the window's
        // crossings must still be cleared.
        let reset = snap(Some(5.0), None);
        let fired = dedup_and_apply(&mut state, &curr, &reset, evaluate(&curr, &reset, &cfg));
        assert!(fired.is_empty(), "reset-back is disabled: {fired:?}");
        assert!(state.crossings.is_empty(), "reset must still re-arm");

        let lo = snap(Some(40.0), None);
        let hi = snap(Some(75.0), None);
        let fired = dedup_and_apply(&mut state, &lo, &hi, evaluate(&lo, &hi, &cfg));
        assert_eq!(
            fired,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70
            }]
        );
    }

    #[test]
    fn session_and_weekly_thresholds_dedup_independently() {
        let mut state = NotifState::default();
        let cfg = NotificationConfig::default();
        let prev = snap(Some(50.0), Some(50.0));
        let curr = snap(Some(75.0), Some(75.0));
        let fired = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert_eq!(fired.len(), 2, "both windows cross 70: {fired:?}");
        assert!(state.crossings.contains(&(Window::Session, 70)));
        assert!(state.crossings.contains(&(Window::Weekly, 70)));
        // A weekly-only reset must leave the session crossing intact.
        let reset = snap(Some(75.0), Some(3.0));
        let _ = dedup_and_apply(&mut state, &curr, &reset, evaluate(&curr, &reset, &cfg));
        assert!(state.crossings.contains(&(Window::Session, 70)));
        assert!(!state.crossings.contains(&(Window::Weekly, 70)));
    }

    // --- weekly reset clears both crossings and pace_fired -----------------

    #[test]
    fn weekly_reset_clears_pace_fired_this_window() {
        let mut state = NotifState {
            crossings: [(Window::Weekly, 70), (Window::Weekly, 90)]
                .into_iter()
                .collect(),
            pace_fired_this_window: true,
            retry_spent: ["pace".to_string(), "threshold:other".to_string()]
                .into_iter()
                .collect(),
            ..NotifState::default()
        };
        // A weekly reset happens; `dedup_and_apply` must clear both, and re-arm
        // the one-shot "pace" delivery retry (only that key).
        let prev = snap(None, Some(85.0));
        let curr = snap(None, Some(3.0));
        let cfg = NotificationConfig::default();
        let _ = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert!(state.crossings.is_empty());
        assert!(!state.pace_fired_this_window);
        assert!(
            !state.retry_spent.contains("pace"),
            "a weekly reset re-arms the pace retry: {:?}",
            state.retry_spent
        );
        assert!(state.retry_spent.contains("threshold:other"));
    }

    // --- session reset does NOT clear pace_fired (pace is weekly only) -----

    #[test]
    fn session_reset_leaves_pace_fired_untouched() {
        let mut state = NotifState {
            crossings: BTreeSet::new(),
            pace_fired_this_window: true,
            ..NotifState::default()
        };
        let prev = snap(Some(85.0), None);
        let curr = snap(Some(3.0), None);
        let cfg = NotificationConfig::default();
        let _ = dedup_and_apply(&mut state, &prev, &curr, evaluate(&prev, &curr, &cfg));
        assert!(
            state.pace_fired_this_window,
            "session reset must not touch weekly pace state"
        );
    }

    // --- WeeklyPace: fires only once per window -----------------------------

    #[test]
    fn weekly_pace_fires_once_per_window_and_rearms_on_reset() {
        let cfg = NotificationConfig {
            pace_enabled: true,
            ..NotificationConfig::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 2.0,
            confidence: 0.9,
            sample_count: 12,
        };
        let curr = snap(None, Some(50.0));

        // 1st tick: emits a pace trigger; dedup lets it through.
        let t = evaluate_pace(Some(&pace), &curr, &cfg, now).expect("pace should fire");
        let mut state = NotifState::default();
        let prev = snap(None, Some(48.0));
        let kept = dedup_and_apply(&mut state, &prev, &curr, vec![t]);
        assert_eq!(kept.len(), 1);
        assert!(state.pace_fired_this_window);

        // 2nd tick: still on pace, same window — must not re-fire.
        let curr2 = snap(None, Some(55.0));
        let t = evaluate_pace(Some(&pace), &curr2, &cfg, now).expect("still on pace");
        let kept = dedup_and_apply(&mut state, &curr, &curr2, vec![t]);
        assert!(kept.is_empty(), "pace must fire at most once per window");

        // 3rd tick: weekly resets — pace_fired clears; pace fires again.
        let reset = snap(None, Some(3.0));
        let _ = dedup_and_apply(&mut state, &curr2, &reset, evaluate(&curr2, &reset, &cfg));
        assert!(!state.pace_fired_this_window);
        let t = evaluate_pace(Some(&pace), &reset, &cfg, now).expect("pace re-arms after reset");
        let kept = dedup_and_apply(&mut state, &reset, &reset, vec![t]);
        assert_eq!(kept.len(), 1, "pace should re-fire in the fresh window");
    }

    #[test]
    fn evaluate_pace_returns_none_when_disabled() {
        let cfg = NotificationConfig {
            pace_enabled: false,
            ..NotificationConfig::default()
        };
        let pace = PaceEstimate {
            slope_pct_per_hour: 5.0,
            confidence: 0.99,
            sample_count: 10,
        };
        let curr = snap(None, Some(50.0));
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        assert!(evaluate_pace(Some(&pace), &curr, &cfg, now).is_none());
    }

    #[test]
    fn evaluate_pace_returns_none_for_low_confidence_or_flat_slope() {
        let cfg = NotificationConfig {
            pace_enabled: true,
            ..NotificationConfig::default()
        };
        let curr = snap(None, Some(50.0));
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let low_conf = PaceEstimate {
            slope_pct_per_hour: 2.0,
            confidence: 0.1,
            sample_count: 10,
        };
        assert!(evaluate_pace(Some(&low_conf), &curr, &cfg, now).is_none());
        let flat = PaceEstimate {
            slope_pct_per_hour: 0.0,
            confidence: 0.9,
            sample_count: 10,
        };
        assert!(evaluate_pace(Some(&flat), &curr, &cfg, now).is_none());
    }

    // --- Golden: formatted messages are intentional -------------------------

    fn ak() -> AccountKey {
        AccountKey::new("claude", "dev@example.com")
    }

    #[test]
    fn golden_threshold_message() {
        let (s, b) = format_message(
            &Trigger::Threshold {
                window: Window::Session,
                pct: 70,
            },
            &ak(),
        );
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Session (5h) usage crossed 70%");
    }

    #[test]
    fn golden_reset_back_message() {
        let (s, b) = format_message(
            &Trigger::ResetBack {
                window: Window::Weekly,
            },
            &ak(),
        );
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Weekly (7d) window reset — headroom restored");
    }

    #[test]
    fn golden_weekly_pace_message() {
        let hit = Utc.with_ymd_and_hms(2026, 9, 5, 14, 30, 0).unwrap();
        let (s, b) = format_message(&Trigger::WeeklyPace { hit_time: hit }, &ak());
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "Weekly on pace to hit 100% at 2026-09-05 14:30 UTC");
    }

    // --- reset grants: arrival + usable, each fire once --------------------

    fn grant(id: &str, resets_left: u32, usable_now: bool) -> ResetGrant {
        ResetGrant {
            id: id.to_string(),
            label: "Opus launch reset".to_string(),
            resets_left,
            usable_now,
            expires_at: None,
        }
    }

    #[test]
    fn reset_grant_arrival_fires_once_then_dedups() {
        let mut state = NotifState::default();
        let grants = [grant("g1", 1, false)];
        let first = reset_grant_triggers(&mut state, &grants);
        assert_eq!(
            first,
            vec![Trigger::ResetGrantAvailable {
                label: "Opus launch reset".to_string()
            }]
        );
        // Same grant next tick — already announced, nothing fires.
        assert!(reset_grant_triggers(&mut state, &grants).is_empty());
    }

    #[test]
    fn reset_grant_usable_fires_separately_and_once() {
        let mut state = NotifState::default();
        // Arrives banked (not usable) — only the arrival fires.
        let banked = [grant("g1", 1, false)];
        assert_eq!(reset_grant_triggers(&mut state, &banked).len(), 1);
        // Later it becomes usable — only the usable alert fires (arrival is deduped).
        let usable = [grant("g1", 1, true)];
        assert_eq!(
            reset_grant_triggers(&mut state, &usable),
            vec![Trigger::ResetUsableNow {
                label: "Opus launch reset".to_string()
            }]
        );
        // Still usable next tick — nothing re-fires.
        assert!(reset_grant_triggers(&mut state, &usable).is_empty());
    }

    #[test]
    fn reset_grant_both_fire_when_usable_on_arrival() {
        let mut state = NotifState::default();
        let usable = [grant("g1", 1, true)];
        let out = reset_grant_triggers(&mut state, &usable);
        assert_eq!(out.len(), 2, "arrival + usable both fire on first sight");
        assert!(matches!(out[0], Trigger::ResetGrantAvailable { .. }));
        assert!(matches!(out[1], Trigger::ResetUsableNow { .. }));
    }

    #[test]
    fn reset_grant_spent_is_ignored() {
        let mut state = NotifState::default();
        // resets_left == 0 → not available → nothing fires.
        assert!(reset_grant_triggers(&mut state, &[grant("g1", 0, true)]).is_empty());
    }

    #[test]
    fn golden_reset_grant_messages() {
        let (s, b) = format_message(
            &Trigger::ResetGrantAvailable {
                label: "Opus 5.5 launch reset".to_string(),
            },
            &ak(),
        );
        assert_eq!(s, "claude · dev@example.com");
        assert_eq!(b, "🎁 You got a usage-limit reset! Opus 5.5 launch reset");
        let (_, b) = format_message(
            &Trigger::ResetUsableNow {
                label: String::new(),
            },
            &ak(),
        );
        assert_eq!(
            b,
            "You're out of usage — claim your banked reset now from usagio."
        );
    }

    // --- NotifState round-trips through serde -------------------------------

    #[test]
    fn notif_state_serde_roundtrips() {
        let s = NotifState {
            crossings: [(Window::Session, 70), (Window::Weekly, 90)]
                .into_iter()
                .collect(),
            pace_fired_this_window: true,
            ..NotifState::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: NotifState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        // The Window enum serializes as kebab-case, so a persisted state.json
        // reads well by eye.
        assert!(json.contains("session"), "{json}");
        assert!(json.contains("weekly"), "{json}");
    }

    // --- Mutation-gap tests: exact boundaries + exact pace projection --------

    #[test]
    fn threshold_alert_label_lists_the_ladder() {
        assert_eq!(threshold_alert_label(), "Threshold alerts (70% / 90%)");
    }

    #[test]
    fn evaluate_does_not_fire_when_prev_already_sits_exactly_on_the_threshold() {
        // prev == 70 means 70 was already reached on the previous tick; only a
        // strict below-to-at-or-above transition is a crossing.
        let cfg = NotificationConfig::default();
        let ts = evaluate(&snap(Some(70.0), None), &snap(Some(80.0), None), &cfg);
        assert!(ts.is_empty(), "{ts:?}");
        // ...while landing exactly ON the threshold from below does fire.
        let ts = evaluate(&snap(Some(69.0), None), &snap(Some(70.0), None), &cfg);
        assert_eq!(
            ts,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70
            }]
        );
    }

    #[test]
    fn reset_requires_a_strict_drop_from_strictly_above_the_floor() {
        let cfg = NotificationConfig::default();
        let reset = |p: f32, c: f32| {
            evaluate(&snap(Some(p), None), &snap(Some(c), None), &cfg).contains(
                &Trigger::ResetBack {
                    window: Window::Session,
                },
            )
        };
        assert!(reset(60.0, 5.0), "a real reset fires");
        assert!(!reset(60.0, 60.0), "no change is not a reset");
        assert!(!reset(50.0, 0.0), "the 50% floor is exclusive");
        let a = snap(Some(60.0), None);
        assert!(!is_reset(&a, &a, Window::Session));
        assert!(!is_reset(
            &snap(Some(50.0), None),
            &snap(Some(0.0), None),
            Window::Session
        ));
    }

    fn pace_cfg() -> NotificationConfig {
        NotificationConfig {
            pace_enabled: true,
            ..NotificationConfig::default()
        }
    }

    #[test]
    fn evaluate_pace_projects_the_exact_hit_time() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 10.0,
            confidence: 0.9,
            sample_count: 12,
        };
        // 50% left at 10%/h => exactly 5 hours out.
        let t = evaluate_pace(Some(&pace), &snap(None, Some(50.0)), &pace_cfg(), now).unwrap();
        assert_eq!(
            t,
            Trigger::WeeklyPace {
                hit_time: Utc.with_ymd_and_hms(2026, 9, 1, 17, 0, 0).unwrap()
            }
        );
    }

    #[test]
    fn evaluate_pace_accepts_confidence_exactly_at_the_floor() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 10.0,
            confidence: 0.5,
            sample_count: 12,
        };
        assert!(evaluate_pace(Some(&pace), &snap(None, Some(50.0)), &pace_cfg(), now).is_some());
    }

    #[test]
    fn evaluate_pace_rejects_a_projection_too_far_out_to_represent() {
        // A vanishingly small positive slope makes hours-to-100 overflow to
        // infinity; that must read as "no usable projection", not a bogus time.
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 1e-320,
            confidence: 0.9,
            sample_count: 12,
        };
        assert!(evaluate_pace(Some(&pace), &snap(None, Some(50.0)), &pace_cfg(), now).is_none());
    }

    #[test]
    fn pace_alert_is_suppressed_when_the_window_resets_first() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let pace = PaceEstimate {
            slope_pct_per_hour: 10.0,
            confidence: 0.9,
            sample_count: 12,
        };
        let mut curr = snap(None, Some(50.0));
        curr.weekly_reset = Some(now + Duration::hours(2));
        assert!(evaluate_pace(Some(&pace), &curr, &pace_cfg(), now).is_none());
        curr.weekly_reset = Some(now + Duration::hours(6));
        assert!(evaluate_pace(Some(&pace), &curr, &pace_cfg(), now).is_some());
    }

    #[test]
    fn a_small_decrease_is_not_a_window_reset() {
        assert!(!is_window_reset(91.0, 89.0));
        assert!(!is_window_reset(80.0, 79.0));
        assert!(is_window_reset(95.0, 5.0));
        assert!(is_window_reset(95.0, 60.0));
    }

    #[test]
    fn a_crossing_in_the_same_pair_as_a_reset_still_fires() {
        let prev = snap(None, Some(95.0));
        let curr = snap(None, Some(75.0));
        let cfg = NotificationConfig::default();
        let trig = evaluate(&prev, &curr, &cfg);
        assert!(
            trig.contains(&Trigger::Threshold {
                window: Window::Weekly,
                pct: 70
            }),
            "{trig:?}"
        );
        // Last week's crossings are still recorded when the pair arrives: the
        // reset must clear them BEFORE dedup, or the new window's 70 is
        // suppressed as "already crossed".
        let mut state = NotifState {
            crossings: [(Window::Weekly, 70), (Window::Weekly, 90)]
                .into_iter()
                .collect(),
            ..NotifState::default()
        };
        let fired = dedup_and_apply(&mut state, &prev, &curr, trig);
        assert!(
            fired.contains(&Trigger::Threshold {
                window: Window::Weekly,
                pct: 70
            }),
            "{fired:?}"
        );
    }

    #[test]
    fn a_failed_delivery_releases_the_dedup_bit_for_retry() {
        let mut st = NotifState::default();
        let g = ResetGrant {
            id: "g1".into(),
            label: "L".into(),
            resets_left: 1,
            usable_now: true,
            ..Default::default()
        };
        let trigs = reset_grant_triggers(&mut st, std::slice::from_ref(&g));
        assert_eq!(trigs.len(), 2);
        for t in &trigs {
            release_dedup(&mut st, t, Some("g1"));
        }
        assert_eq!(reset_grant_triggers(&mut st, &[g]).len(), 2, "retried");
        st.pace_fired_this_window = true;
        release_dedup(
            &mut st,
            &Trigger::WeeklyPace {
                hit_time: Utc::now(),
            },
            None,
        );
        assert!(!st.pace_fired_this_window);
    }

    #[test]
    fn a_failed_delivery_is_retried_at_most_once() {
        let mut st = NotifState::default();
        let g = ResetGrant {
            id: "g1".into(),
            label: "L".into(),
            resets_left: 1,
            usable_now: false,
            ..Default::default()
        };
        let t = reset_grant_triggers(&mut st, std::slice::from_ref(&g));
        release_dedup(&mut st, &t[0], Some("g1"));
        let t = reset_grant_triggers(&mut st, std::slice::from_ref(&g));
        assert_eq!(t.len(), 1, "first failure: retried once");
        release_dedup(&mut st, &t[0], Some("g1"));
        assert!(
            reset_grant_triggers(&mut st, std::slice::from_ref(&g)).is_empty(),
            "second failure: dedup bit stays set"
        );
    }

    // --- audit round 3 (B3) ---------------------------------------------------

    #[test]
    fn r3_a_reset_seen_by_the_reset_time_clears_crossings_and_fires_reset_back() {
        // Offline across the weekly reset while usage continued elsewhere:
        // 92 -> 75 is a small drop, but the reported reset moved past.
        let r = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let mut prev = snap(None, Some(92.0));
        prev.ts = r - Duration::hours(1);
        prev.weekly_reset = Some(r);
        let mut curr = snap(None, Some(75.0));
        curr.ts = r + Duration::days(3);
        curr.weekly_reset = Some(r + Duration::days(7));
        assert!(is_reset(&prev, &curr, Window::Weekly));
        let cfg = NotificationConfig::default();
        let trig = evaluate(&prev, &curr, &cfg);
        assert!(
            trig.contains(&Trigger::ResetBack {
                window: Window::Weekly
            }),
            "{trig:?}"
        );
        let mut state = NotifState {
            crossings: [(Window::Weekly, 70), (Window::Weekly, 90)]
                .into_iter()
                .collect(),
            ..NotifState::default()
        };
        dedup_and_apply(&mut state, &prev, &curr, trig);
        let mut next = curr.clone();
        next.weekly_pct = Some(91.0);
        let fired = dedup_and_apply(&mut state, &curr, &next, evaluate(&curr, &next, &cfg));
        assert!(
            fired.contains(&Trigger::Threshold {
                window: Window::Weekly,
                pct: 90
            }),
            "{fired:?}"
        );
        // A reset time that moved but has not passed yet is not a reset.
        let mut early = curr.clone();
        early.ts = r - Duration::minutes(30);
        assert!(!is_reset(&prev, &early, Window::Weekly));
    }

    #[test]
    fn a_rollover_of_a_barely_used_window_rearms_but_does_not_announce() {
        let r = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let mut prev = snap(Some(4.0), None);
        prev.ts = r - Duration::minutes(5);
        prev.session_reset = Some(r);
        let mut curr = snap(Some(1.0), None);
        curr.ts = r + Duration::minutes(5);
        curr.session_reset = Some(r + Duration::hours(5));
        assert!(is_reset(&prev, &curr, Window::Session), "still a rollover");
        let trig = evaluate(&prev, &curr, &NotificationConfig::default());
        assert!(
            !trig.contains(&Trigger::ResetBack {
                window: Window::Session
            }),
            "{trig:?}"
        );
        let mut state = NotifState {
            crossings: [(Window::Session, 70)].into_iter().collect(),
            ..NotifState::default()
        };
        dedup_and_apply(&mut state, &prev, &curr, trig);
        assert!(state.crossings.is_empty(), "the rollover re-arms crossings");
    }

    #[test]
    fn now_relative_reset_stamps_do_not_roll_on_every_poll() {
        // An expired Codex window reports 0 s left: each fetch stamps the
        // reset at its own "now".
        let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
        let tick = |i: i64, pct: f32| {
            let mut s = snap(Some(pct), None);
            s.ts = t0 + Duration::minutes(i);
            s.session_reset = Some(s.ts);
            s
        };
        let cfg = NotificationConfig::default();
        let mut state = NotifState::default();
        let (a, b, c) = (tick(0, 60.0), tick(1, 80.0), tick(2, 80.0));
        let first = dedup_and_apply(&mut state, &a, &b, evaluate(&a, &b, &cfg));
        assert_eq!(
            first,
            vec![Trigger::Threshold {
                window: Window::Session,
                pct: 70
            }]
        );
        assert!(!is_reset(&b, &c, Window::Session));
        let again = dedup_and_apply(&mut state, &b, &c, evaluate(&b, &c, &cfg));
        assert!(
            again.is_empty(),
            "re-fired on a now-relative stamp: {again:?}"
        );
    }

    #[test]
    fn r3_no_pace_alert_on_the_weekly_reset_tick() {
        // The pace fit still covers last week's samples on the reset tick:
        // it must not use up the new window's one alert.
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 1, 0).unwrap();
        let mut prev = snap(None, Some(95.0));
        prev.weekly_reset = Some(now - Duration::minutes(1));
        let mut curr = snap(None, Some(2.0));
        curr.ts = now;
        curr.weekly_reset = Some(now + Duration::days(7));
        let pace = PaceEstimate {
            slope_pct_per_hour: 0.8,
            confidence: 0.9,
            sample_count: 200,
        };
        let pt = evaluate_pace(Some(&pace), &curr, &pace_cfg(), now).expect("precondition");
        let mut state = NotifState {
            pace_fired_this_window: true,
            ..NotifState::default()
        };
        let kept = dedup_and_apply(&mut state, &prev, &curr, vec![pt]);
        assert!(kept.is_empty(), "{kept:?}");
        assert!(
            !state.pace_fired_this_window,
            "the new window's alert is unspent"
        );
    }
}
