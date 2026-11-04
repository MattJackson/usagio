//! Subscription-lapse detection and recheck.

use crate::notifications::notify;
use crate::poll::trackers::{claim_subscription_check, clear_inactive_fetch_backoff};
use crate::providers::trait_def::PlanStatus;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::state_ops::{save_if_changed, with_state_lock};
use crate::store::State;
use crate::switching::provider::provider_by_slug;
use crate::{lifecycle, logging};

/// How often to ask the profile endpoint about an account's subscription —
/// both to confirm a suspected lapse (403/429 from the usage endpoint) and to
/// notice a renewal of a lapsed one. The profile endpoint is separate from
/// `/api/oauth/usage`, so these checks don't spend the usage request budget.
pub(crate) const SUBSCRIPTION_RECHECK_SECS: u64 = 1800;

/// Outcome of re-checking a lapsed account's plan.
pub(crate) enum Recheck {
    /// Still lapsed — skip the usage endpoint.
    StillLapsed(PlanStatus),
    /// Plan is back — fetch usage this cycle.
    Restored(PlanStatus),
    /// The provider can't say without its usage endpoint (e.g. Codex reports
    /// the plan only alongside usage) — fetch and read it from the snapshot.
    Unknown,
}

/// Ask `provider` (never its usage endpoint) whether a lapsed account's plan
/// is back. The caller has already claimed the throttled check.
pub(crate) fn recheck_lapsed_plan(
    provider: &dyn Provider,
    tracker_key: &str,
    access: &str,
) -> Recheck {
    match provider.check_plan(access) {
        Some(p) if p.active => {
            clear_inactive_fetch_backoff(tracker_key);
            Recheck::Restored(p)
        }
        Some(p) => {
            logging::log(&format!(
                "poll: {tracker_key} still has no subscription; skipping usage \
                 (event=fetch_skip_no_subscription account={tracker_key})"
            ));
            Recheck::StillLapsed(p)
        }
        None => Recheck::Unknown,
    }
}

/// The plan to persist after a failed usage fetch: a lapse detected now wins,
/// but a `Restored` plan confirmed earlier this cycle is kept (the throttled
/// detector returns `None` for an account already checked this cycle).
pub(crate) fn plan_after_failed_fetch(
    current: Option<PlanStatus>,
    detected: Option<PlanStatus>,
) -> Option<PlanStatus> {
    detected.or(current)
}

/// After a 403/429 from the usage endpoint, ask the provider (throttled per
/// account) whether the plan lapsed. `Some` only for a confirmed lapse.
pub(crate) fn detect_lapsed_plan(
    provider: &dyn Provider,
    tracker_key: &str,
    access: &str,
    now_ts: i64,
) -> Option<PlanStatus> {
    if !claim_subscription_check(tracker_key, now_ts, false) {
        return None;
    }
    provider.check_plan(access).filter(|p| !p.active)
}

/// Persist an account's plan (label + lapsed flag) for any provider, and
/// notify on a lapse/renewal transition. `refreshed` carries tokens rotated
/// this cycle when the caller is about to skip its own save (non-Claude
/// accounts only; Claude token writes go through the poll merge).
pub(crate) fn persist_plan(
    slug: &str,
    key: &str,
    plan: &PlanStatus,
    refreshed: Option<&(String, String, i64)>,
) {
    let lapsed = !plan.active;
    let res = with_state_lock(|| {
        let mut st = State::load()?;
        let loaded = serde_json::to_vec(&st).unwrap_or_default();
        let mut changed = false;
        let apply = |no_sub: &mut bool, label: &mut Option<String>| {
            let was = *no_sub;
            *no_sub = lapsed;
            if plan.label.is_some() {
                *label = plan.label.clone();
            }
            was != lapsed
        };
        if slug == CLAUDE_SLUG {
            if let Some(a) = st.find_mut(key) {
                changed = apply(&mut a.no_subscription, &mut a.plan);
            }
        } else if let Some(a) = st.find_provider_account_mut(slug, key) {
            changed = apply(&mut a.no_subscription, &mut a.plan);
            if let Some((at, rt, exp)) = refreshed {
                let provider = provider_by_slug(slug)?;
                lifecycle::set_provider_tokens(provider, a, at, rt, *exp)?;
            }
        }
        save_if_changed(&st, &loaded)?;
        Ok(changed)
    });
    match res {
        Ok(true) => notify_subscription_change(slug, key, lapsed),
        Ok(false) => {}
        Err(e) => logging::log(&format!("persist_plan({slug}/{key}): save failed: {e:#}")),
    }
}

pub(crate) fn notify_subscription_change(slug: &str, key: &str, lapsed: bool) {
    logging::log(&format!(
        "event=subscription_{} provider={slug} account={key}",
        if lapsed { "lapsed" } else { "restored" }
    ));
    notify(&if lapsed {
        format!("{key} ({slug}) has no subscription — usage checks paused until it's renewed")
    } else {
        format!("{key} ({slug}) subscription is back")
    });
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
