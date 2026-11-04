//! Menu ordering and the auto-pick choice.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use crate::autoswap;
use crate::display::humanize_until;
use crate::rows::{row_from_account, row_to_account_usage, Row};
use crate::store::State;
use crate::switching::claude::switch_to;
use crate::watch::cadence::TRIGGER_PCT;

/// Use the shared display priority; active status never changes account order.
pub(crate) fn menu_order(
    a: &Row,
    b: &Row,
    _active: Option<&str>,
    now: DateTime<Utc>,
) -> std::cmp::Ordering {
    let priority = |r: &Row| {
        crate::account_order::Priority::new(
            r.no_subscription,
            &row_to_account_usage(r),
            r.headroom(),
            now,
        )
    };
    priority(a)
        .cmp(&priority(b))
        .then_with(|| a.email.cmp(&b.email))
}

/// Switch to the account auto-pick considers best right now (the one with room
/// whose weekly window resets soonest, so its quota is used before it resets).
/// Uses cached usage only — no network. Returns `Some(email)` if it switched,
/// or `None` if the active account is already the best choice. Used by the
/// menu bar's "Now" item.
pub(crate) fn optimize_now() -> Result<Option<String>> {
    let state = State::load()?;
    let active = state.active.clone();
    let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
    let trigger = state.trigger_pct.unwrap_or(TRIGGER_PCT);
    let best = auto_pick_for(&state, &rows, trigger, trigger)?;
    if active.as_deref() == Some(best.as_str()) {
        return Ok(None);
    }
    switch_to(&best, false)?;
    Ok(Some(best))
}

/// Pick an account by the auto-swap ranking (`autoswap::assess::rank_cmp`):
/// among Ready accounts (at least `MIN_LANDING_MIN` of runway before the
/// trigger), the one whose weekly window resets earliest, ties to the most
/// runway; if none is Ready, the Draining (near-trigger) account with the most
/// hard runway. Never an account at its limit, signed out, or without a
/// subscription: with nothing left it errors naming the soonest unlock.
/// Operates entirely on cached rows — callers must not fetch first.
pub(crate) fn auto_pick_for(
    _state: &State,
    rows: &[Row],
    ceiling: f64,
    trigger: f64,
) -> Result<String> {
    auto_pick_v2(rows, ceiling, trigger, Utc::now())
}

/// Auto-pick by the v2 `rank_key`: the Ready account whose weekly reset comes
/// first (then the most runway), then the Draining one with the most hard
/// runway. Never an account at its limit.
fn auto_pick_v2(rows: &[Row], ceiling: f64, trigger: f64, now: DateTime<Utc>) -> Result<String> {
    if !rows.iter().any(|r| r.has_data()) {
        bail!(
            "no usage data yet — let the menu-bar app or `usagio watch` \
             populate it, or pass an explicit account email"
        );
    }
    let policy = autoswap::Policy::new(trigger, ceiling);
    let memory = autoswap::SwapMemory::default();
    let ctx = autoswap::assess::Ctx {
        now,
        policy: &policy,
        memory: &memory,
        sleep: &[],
    };
    let mut ranked: Vec<autoswap::assess::Assessed> = rows
        .iter()
        .filter(|r| r.has_data() && !r.needs_relogin && !r.no_subscription)
        .map(|r| {
            // Default rates: candidates are judged at the floored rate anyway,
            // and a CLI pick must not depend on reading the usage log.
            let mut a = autoswap::shell::account_in_from_row(r, Default::default());
            // A cached reading is all a manual pick has; rank it as if fresh.
            a.fetched_at = Some(now.timestamp());
            autoswap::assess::assess(&a, false, &ctx)
        })
        .collect();
    ranked.sort_by(|a, b| autoswap::assess::rank_cmp(a, b, None));
    let Some(pick) = ranked
        .iter()
        .find(|a| matches!(a.tier, autoswap::Tier::Ready | autoswap::Tier::Draining))
    else {
        let soonest = ranked
            .iter()
            .filter_map(|a| a.blocked_until.map(|u| (a, u)))
            .min_by_key(|(_, u)| *u);
        match soonest {
            Some((a, u)) => bail!(
                "all accounts are maxed out; {} resets soonest, in {}",
                a.key,
                humanize_until(u)
            ),
            None => bail!("no account currently has room"),
        }
    };
    let runway = if pick.soft_runway.is_finite() {
        format!(
            "{:.0} min to the trigger at the floored rate",
            pick.soft_runway
        )
    } else {
        "no limit in sight".to_string()
    };
    let note = if pick.tier == autoswap::Tier::Ready {
        ""
    } else {
        " (all accounts near the trigger — this one lasts longest)"
    };
    println!("Auto-picked {} — {runway}.{note}", pick.key);
    Ok(pick.key.clone())
}

#[cfg(test)]
#[path = "picking_tests.rs"]
mod tests;
