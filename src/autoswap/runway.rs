//! Runway math shared by the engine and the binding-window display (§3.3, §4).
//!
//! Runway is minutes until a window reaches a limit at a given rate. The
//! division is guarded: with no rate, runway is infinite below the limit and
//! zero at or above it. Never NaN.

use chrono::{DateTime, Duration, Utc};

use super::policy::{
    BIND_MIN_DWELL_S, BIND_NO_RATE_LEAD_PTS, BIND_OVERRIDE_MIN, BIND_SWITCH_MARGIN, BLOCKED_PCT,
    RATE_EPSILON,
};
use super::{AccountIn, WindowId};

/// Minutes until `pct` reaches `limit` at `rate` pct/min. `0` at or over the
/// limit, `f64::INFINITY` with no (≤ ε) rate below it. Never NaN.
pub fn time_to(pct: f64, limit: f64, rate: f64) -> f64 {
    if !pct.is_finite() || pct >= limit {
        return 0.0;
    }
    if !rate.is_finite() || rate <= RATE_EPSILON {
        return f64::INFINITY;
    }
    (limit - pct) / rate
}

/// `pct + rate × minutes × safety`, the drift estimate of a window.
pub fn est_window(pct: f64, rate: f64, minutes: f64, safety: f64) -> f64 {
    let drift = rate.max(0.0) * minutes.max(0.0) * safety;
    if drift.is_finite() {
        pct + drift
    } else {
        pct
    }
}

/// The window that will block first, for display (§4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Binding {
    pub window: WindowId,
    pub pct: f64,
    /// Minutes until the window reaches 100% at the account's own rate;
    /// `None` when there's no rate (no-rate mode) or the window doesn't bind.
    pub time_to_block: Option<f64>,
    /// Whether this was chosen by the rate (`false`: the higher-pct fallback).
    pub by_rate: bool,
}

/// One displayed window candidate: (window, pct, time_to_block, binds).
struct DisplayWin {
    id: WindowId,
    pct: f64,
    ttb: f64,
    binds: bool,
}

fn display_windows(a: &AccountIn, now: DateTime<Utc>) -> Vec<DisplayWin> {
    let mut out = Vec::new();
    for (id, w, rate) in [
        (
            WindowId::Session,
            a.session.as_ref(),
            a.rate.own(WindowId::Session),
        ),
        (
            WindowId::Weekly,
            a.weekly.as_ref(),
            a.rate.own(WindowId::Weekly),
        ),
    ] {
        let Some(w) = w else { continue };
        // A reset-pending window is an upper bound only: shown as `—`.
        if w.resets_at.is_some_and(|r| r <= now) {
            continue;
        }
        let ttb = time_to(w.pct, 100.0, rate.unwrap_or(0.0));
        let binds = match w.resets_at {
            Some(r) if ttb.is_finite() => r >= now + minutes(ttb),
            _ => true,
        };
        out.push(DisplayWin {
            id,
            pct: w.pct,
            ttb,
            binds,
        });
    }
    out
}

fn minutes(m: f64) -> Duration {
    if !m.is_finite() || m > 1.0e7 {
        return Duration::days(3650);
    }
    Duration::milliseconds((m * 60_000.0) as i64)
}

/// The binding window of `a` at `now` (§4), with no trigger known: see
/// [`binding_window_at`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn binding_window(a: &AccountIn, now: DateTime<Utc>) -> Option<Binding> {
    binding_window_at(a, now, f64::INFINITY)
}

/// The binding window of `a` at `now` (§4): a window at or over `trigger`
/// first (higher pct); else the binding window with the shorter time to block
/// at the account's own rate — unless a window with no rate (flat or unknown)
/// shows a higher pct, which then binds; with nothing rated, the higher pct
/// (ties go to weekly). A reset-pending window is skipped. `None` when no
/// window can be shown.
pub fn binding_window_at(a: &AccountIn, now: DateTime<Utc>, trigger: f64) -> Option<Binding> {
    let wins = display_windows(a, now);
    let highest = |ws: Vec<&DisplayWin>| -> Option<Binding> {
        ws.into_iter()
            .max_by(|x, y| {
                x.pct
                    .total_cmp(&y.pct)
                    .then((x.id == WindowId::Weekly).cmp(&(y.id == WindowId::Weekly)))
            })
            .map(|w| Binding {
                window: w.id,
                pct: w.pct,
                time_to_block: w.ttb.is_finite().then_some(w.ttb),
                by_rate: false,
            })
    };
    let over: Vec<&DisplayWin> = wins.iter().filter(|w| w.pct >= trigger).collect();
    if !over.is_empty() {
        return highest(over);
    }
    let rated = wins
        .iter()
        .filter(|w| w.binds && w.ttb.is_finite())
        .min_by(|x, y| x.ttb.total_cmp(&y.ttb).then(y.pct.total_cmp(&x.pct)));
    let Some(r) = rated else {
        return highest(wins.iter().collect());
    };
    let unrated_higher: Vec<&DisplayWin> = wins
        .iter()
        .filter(|w| !w.ttb.is_finite() && w.pct > r.pct)
        .collect();
    if !unrated_higher.is_empty() {
        return highest(unrated_higher);
    }
    Some(Binding {
        window: r.id,
        pct: r.pct,
        time_to_block: Some(r.ttb),
        by_rate: true,
    })
}

/// Time to block for one window at the account's own rate (display), if it
/// binds. Used by the hysteresis to compare the shown window with the other.
fn window_ttb(a: &AccountIn, id: WindowId, now: DateTime<Utc>) -> Option<(f64, f64, bool)> {
    display_windows(a, now)
        .into_iter()
        .find(|w| w.id == id)
        .map(|w| (w.pct, w.ttb, w.binds))
}

/// The window currently shown in the tray for one account, with the time it
/// was chosen. Kept in memory by the menu-bar process (not persisted).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BindingShown {
    pub window: WindowId,
    pub since: DateTime<Utc>,
}

impl BindingShown {
    /// Fold a new `binding_window` result into the shown window, applying the
    /// display hysteresis (§4). `trigger` is the auto-swap trigger (a window at
    /// or over it overrides the dwell). Returns the window to display.
    pub fn update(
        prev: Option<BindingShown>,
        a: &AccountIn,
        now: DateTime<Utc>,
        trigger: f64,
    ) -> Option<BindingShown> {
        let fresh = binding_window_at(a, now, trigger)?;
        let Some(prev) = prev else {
            return Some(BindingShown {
                window: fresh.window,
                since: now,
            });
        };
        if prev.window == fresh.window {
            return Some(prev);
        }
        let switch = BindingShown {
            window: fresh.window,
            since: now,
        };
        // The shown window reset (now pending) or is gone: switch at once.
        let Some((cur_pct, cur_ttb, cur_binds)) = window_ttb(a, prev.window, now) else {
            return Some(switch);
        };
        let Some((oth_pct, oth_ttb, _)) = window_ttb(a, fresh.window, now) else {
            return Some(prev);
        };
        // Became blocked, or no longer binds (resets before it could block).
        if cur_pct >= BLOCKED_PCT || !cur_binds {
            return Some(switch);
        }
        // Never switch away from a window at or over the trigger.
        if cur_pct >= trigger {
            return Some(prev);
        }
        // Override, no dwell: the other window is about to block or already at
        // the trigger.
        if (oth_ttb.is_finite() && oth_ttb < BIND_OVERRIDE_MIN) || oth_pct >= trigger {
            return Some(switch);
        }
        let dwell = (now - prev.since).num_seconds() >= BIND_MIN_DWELL_S;
        if fresh.by_rate {
            // Shorter by the relative margin, after the dwell.
            let shorter = cur_ttb.is_infinite() || oth_ttb <= cur_ttb * (1.0 - BIND_SWITCH_MARGIN);
            if shorter && dwell {
                return Some(switch);
            }
            return Some(prev);
        }
        // No-rate mode: the other pct must lead by a few points.
        if oth_pct >= cur_pct + BIND_NO_RATE_LEAD_PTS {
            return Some(switch);
        }
        Some(prev)
    }
}
