//! Tray icon and tooltip selection.

use super::binding::{binding_for, binding_tooltip};
use super::model::{
    account_usage_for, acct_max_pct, locked_countdown_for, now_utc, AcctView, ProviderSection,
    Snapshot,
};
use super::render::recovery_status;
use crate::countdown;
use crate::countdown::DisplayState;
use chrono::{DateTime, Utc};

/// The account the tray icon/title should reflect, honoring
/// `State.tray_icon_mode` (carried on the snapshot):
///   - `None` — "at risk" (the default): across every provider's ACTIVE
///     account, the one closest to a limit (highest [`acct_max_pct`]). This is
///     the actionable signal — "which agent do I need to switch/worry about."
///   - `Some(slug)` — pin the tray to that provider's active account. Falls
///     back to "at risk" if the pinned provider has no captured/active account
///     (e.g. it was removed), so the tray never goes blank on a stale setting.
pub(super) fn tray_target<'a>(
    snap: &'a Snapshot,
    mode: Option<&str>,
) -> Option<(&'a ProviderSection, &'a AcctView)> {
    if let Some(slug) = mode {
        if let Some(sec) = snap.sections.iter().find(|s| s.provider_id == slug) {
            if let Some(a) = sec
                .accounts
                .iter()
                .find(|a| a.active)
                .or_else(|| sec.accounts.first())
            {
                return Some((sec, a));
            }
        }
        // Pinned provider absent → fall through to at-risk rather than blank.
    }
    // At risk: the active account that blocks soonest (its binding window's
    // time to block at its own pace), falling back to the highest pct.
    let now = now_utc();
    let all_actives: Vec<(&ProviderSection, &AcctView)> = snap
        .sections
        .iter()
        .filter_map(|sec| Some((sec, sec.accounts.iter().find(|a| a.active)?)))
        .collect();
    let actives: Vec<(&ProviderSection, &AcctView)> = all_actives
        .iter()
        .copied()
        .filter(|(_, a)| locked_countdown_for(a, now).is_none())
        .collect();
    if actives.is_empty() {
        // Every active account is locked: show the one that unlocks soonest
        // (its lock countdown) rather than a blank "no active account".
        // A maxed window with no reset time blocks indefinitely: the other
        // window's unlock time is not a real unlock, so rank it last.
        let unlock = |a: &AcctView| {
            let usage = account_usage_for(a);
            if countdown::has_indefinite_block(&usage) {
                return None;
            }
            match countdown::compute_display(&usage, now) {
                DisplayState::Locked { until, .. } => Some(until),
                _ => None,
            }
        };
        return all_actives
            .into_iter()
            .min_by_key(|(_, a)| unlock(a).unwrap_or(DateTime::<Utc>::MAX_UTC));
    }
    let ttb = |a: &AcctView| binding_for(a, now).and_then(|b| b.time_to_block);
    if actives.iter().any(|(_, a)| ttb(a).is_some()) {
        return actives.into_iter().min_by(|(_, a), (_, b)| {
            ttb(a)
                .unwrap_or(f64::INFINITY)
                .total_cmp(&ttb(b).unwrap_or(f64::INFINITY))
                .then(
                    acct_max_pct(b)
                        .unwrap_or(-1.0)
                        .total_cmp(&acct_max_pct(a).unwrap_or(-1.0)),
                )
        });
    }
    actives.into_iter().max_by(|(_, a), (_, b)| {
        acct_max_pct(a)
            .unwrap_or(-1.0)
            .partial_cmp(&acct_max_pct(b).unwrap_or(-1.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// The tray icon bytes for the current [`tray_target`]: the target provider's
/// 16px brand icon, so the tray shows *which* agent the percentage is for
/// (Linux/Windows; macOS keeps the usagio mark and names the agent in the
/// tooltip). Falls back to the usagio mark when there's no target or no bundled icon.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) fn tray_icon_bytes(snap: &Snapshot) -> &'static [u8] {
    tray_target(snap, snap.tray_icon_mode.as_deref())
        .and_then(|(sec, _)| crate::icons::png16_for(sec.provider_id))
        .unwrap_or_else(crate::icons::usagio_tray_icon)
}

/// The tray icon to push, if the target agent's icon changed since `last`.
/// Checked every tick (like the title), not only when the menu signature
/// changes: `tray_target` depends on burn rates the signature leaves out.
#[allow(dead_code)] // used by the Linux/Windows redraw loop only
pub(super) fn tray_icon_update(
    last: &mut Option<&'static [u8]>,
    snap: &Snapshot,
) -> Option<&'static [u8]> {
    let icon = tray_icon_bytes(snap);
    if *last == Some(icon) {
        return None;
    }
    *last = Some(icon);
    Some(icon)
}

/// The tooltip to push, if it changed since `last` (see [`tray_icon_update`]).
#[allow(dead_code)] // used by the macOS tick only
pub(super) fn tray_tooltip_update(last: &mut String, snap: &Snapshot) -> Option<String> {
    let tip = tooltip_for(snap);
    if *last == tip {
        return None;
    }
    last.clone_from(&tip);
    Some(tip)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn tooltip_for(snap: &Snapshot) -> String {
    match tray_target(snap, snap.tray_icon_mode.as_deref()) {
        // Preserve the v1 tooltip format verbatim: `email — session X, weekly Y`.
        // Multi-window providers still project onto the first two windows
        // (session-analog / weekly-analog) so the tooltip stays a stable
        // one-liner regardless of how many windows the provider carries.
        Some((sec, a)) => {
            if let Some((_, detail)) = recovery_status(sec, a, now_utc()) {
                return detail;
            }
            binding_tooltip(a, now_utc(), snap.threshold)
        }
        None => "usagio: no active account".to_string(),
    }
}

#[cfg(test)]
#[path = "tray_tests.rs"]
mod tests;
