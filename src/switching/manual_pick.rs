//! Manual-pick holds and their settle/release rules.

use crate::rows::{provider_rows, Row};
use crate::state_ops::with_state_lock;
use crate::store::State;
use crate::watch::cadence::TRIGGER_PCT;
use crate::{autoswap, logging};

/// Whether a manual pick of `key` should hold. Only a pick of an account
/// already at/over the trigger does: the user is acking it's over and owns
/// the choice until it's exhausted. Under the trigger it only blocks
/// proactive flip-back (`State::manual_picks`) and auto-swaps off at the
/// trigger like any other.
#[cfg(test)]
fn pick_holds(state: &State, slug: &str, key: &str) -> bool {
    pick_tier(state, slug, key) >= autoswap::Tier::Draining
}

/// The tier a manual pick of `key` settles at from its cached reading: Ready
/// under the trigger, Draining at/over it, Blocked at the limit. A pick holds
/// while its tier is Draining or worse (auto-swap v2 `manual_pick_tier`).
pub(crate) fn pick_tier(state: &State, slug: &str, key: &str) -> autoswap::Tier {
    let trigger = state.trigger_pct.unwrap_or(TRIGGER_PCT);
    provider_rows(state, slug)
        .iter()
        .find(|r| r.email.eq_ignore_ascii_case(key))
        .filter(|r| r.has_data())
        .map(|r| tier_for_pct(r.max_pct(), trigger))
        .unwrap_or(autoswap::Tier::Ready)
}

pub(crate) fn tier_for_pct(pct: f64, trigger: f64) -> autoswap::Tier {
    if pct >= autoswap::policy::BLOCKED_PCT {
        autoswap::Tier::Blocked
    } else if pct >= trigger {
        autoswap::Tier::Draining
    } else {
        autoswap::Tier::Ready
    }
}

/// How long after a manual switch its first reading still settles whether
/// the pick holds (see `settle_manual_pick`).
const MANUAL_PICK_SETTLE_SECS: i64 = autoswap::policy::MANUAL_PICK_SETTLE_S;

/// Re-decide `slug`'s manual hold on the first reading of the picked account
/// taken after the switch. `pick_holds` decided from the cached reading, which
/// for an account that wasn't active can be minutes old: a pick read at 93%
/// that was really at 96% would otherwise auto-swap away instead of holding.
/// A reading later than `MANUAL_PICK_SETTLE_SECS` after the switch (usage
/// 429s can delay it) no longer says where the pick started, so it keeps the
/// switch-time decision. Returns whether `state` changed.
fn settle_manual_pick(state: &mut State, slug: &str, rows: &[Row], trigger: f64) -> bool {
    let Some(at) = state.manual_pick_at.get(slug).copied() else {
        return false;
    };
    let Some(key) = state.manual_pick(slug).map(str::to_string) else {
        return false;
    };
    let Some(row) = rows.iter().find(|r| r.email.eq_ignore_ascii_case(&key)) else {
        return false;
    };
    let Some(fetched) = row.fetched_at.filter(|&f| f > at && row.has_data()) else {
        return false;
    };
    state.manual_pick_at.remove(slug);
    if fetched > at + MANUAL_PICK_SETTLE_SECS {
        return true;
    }
    let hold = row.max_pct() >= trigger;
    state.set_manual_hold(slug, hold.then_some(key.as_str()));
    let tier = tier_for_pct(row.max_pct(), trigger);
    state.set_manual_pick_tier(slug, Some(tier.as_str()));
    logging::log(&format!(
        "event=manual_pick_settled provider={slug} account={key} pct={:.0} hold={hold} tier={}",
        row.max_pct(),
        tier.as_str()
    ));
    true
}

/// End `slug`'s manual hold once the held account reads under the trigger.
/// A hold only exists because the account was picked at/over the trigger;
/// once its window resets it is a fresh account like any other, and a hold
/// left over would keep auto-swap from preparing the soonest-resetting
/// account when it runs out again. The pick itself stays (it still blocks
/// proactive flip-back). Returns whether `state` changed.
fn release_reset_hold(state: &mut State, slug: &str, rows: &[Row], trigger: f64) -> bool {
    if state.manual_pick_at.contains_key(slug) {
        return false; // first post-switch reading still decides the hold
    }
    let Some(key) = state.manual_hold(slug).map(str::to_string) else {
        return false;
    };
    let Some(row) = rows.iter().find(|r| r.email.eq_ignore_ascii_case(&key)) else {
        return false;
    };
    if !row.has_data() || row.max_pct() >= trigger {
        return false;
    }
    state.set_manual_hold(slug, None);
    state.set_manual_pick_tier(slug, Some(autoswap::Tier::Ready.as_str()));
    logging::log(&format!(
        "event=manual_hold_released provider={slug} account={key} pct={:.0} reason=reset",
        row.max_pct()
    ));
    true
}

/// `settle_manual_pick` for every provider with a pick still waiting on a
/// post-switch reading, and `release_reset_hold` for every held one. Judged
/// against the saved trigger, the threshold `pick_tier` decided the hold by
/// (not the cycle's effective one: auto-swap off runs at a 101% sentinel).
pub(crate) fn settle_manual_picks() {
    let settled = with_state_lock(|_| {
        let mut st = State::load()?;
        let trigger = st.trigger_pct.unwrap_or(TRIGGER_PCT);
        let mut slugs: Vec<String> = st.manual_pick_at.keys().cloned().collect();
        slugs.extend(st.manual_holds.keys().cloned());
        slugs.sort();
        slugs.dedup();
        let mut changed = false;
        for slug in slugs {
            let rows = provider_rows(&st, &slug);
            changed |= settle_manual_pick(&mut st, &slug, &rows, trigger);
            changed |= release_reset_hold(&mut st, &slug, &rows, trigger);
        }
        if changed {
            st.save()?;
        }
        Ok(())
    });
    if let Err(e) = settled {
        logging::log(&format!("settling manual pick failed: {e:#}"));
    }
}

#[cfg(test)]
#[path = "manual_pick_tests.rs"]
mod tests;
