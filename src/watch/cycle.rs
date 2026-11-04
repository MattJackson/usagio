//! One watch cycle: refresh, assess, and swap.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::cli::capture::warn_expiring_logins;
use crate::history::append_history;
use crate::lifecycle::refresh_provider_active_account;
use crate::poll::claude::refresh_usage_cache_with;
use crate::poll::providers::refresh_provider_usage_caches_with;
use crate::poll::trackers::{
    fetch_tracker_key, record_inactive_fetch_failure, record_usage_fetch_429,
    record_usage_fetch_non_429, record_usage_fetch_success,
};
use crate::providers::claude::usage;
use crate::providers::trait_def::ProviderError;
use crate::providers::CLAUDE_SLUG;
use crate::rows::{
    attach_prepaid_balance, cached_from_usage, cached_from_usage_snapshot, provider_rows, Row,
};
use crate::state_ops::{provider_active, with_state_lock};
use crate::store::{CachedUsage, State};
use crate::switching::manual_pick::settle_manual_picks;
use crate::switching::provider::{
    complete_pending_switches, provider_by_slug, provider_supports_swap,
};
use crate::watch::cadence::{cadence_max_pct, NEXT_CHECK_MIN_SECS};
use crate::{autoswap, claude_profiles, credentials, logging, providers};

/// Anti-thrash state carried across watch cycles.
#[derive(Default)]
pub(crate) struct SwapGuard {
    pub(crate) last_swap: Option<std::time::Instant>,
    // The account the user switched to by hand, as `(slug, key)`. Mirrors
    // `State::manual_hold`, refreshed every cycle. Auto-swap stays on it
    // until it is exhausted or lapses, and never prepares a switch away from
    // it while every account is blocked.
    pub(crate) manual_locked_choice: Option<(String, String)>,
    // Any account the user switched to by hand, as `(slug, key)`. Mirrors
    // `State::manual_pick`. Blocks proactive flip-back off it; reaching the
    // trigger still swaps.
    pub(crate) manual_pick: Option<(String, String)>,
    // Settings ▸ Auto-swap ▸ "Use credits when every plan is used up".
    // Mirrors `State::use_credits_when_exhausted`, refreshed every cycle.
    pub(crate) use_credits: bool,
    // Auto-swap v2's in-memory shell state for this provider (request
    // budget, decision-log dedupe). Its persisted memory is in state.json.
    pub(crate) v2: autoswap::shell::ShellState,
}

/// Anti-thrash state for every provider: one independent `SwapGuard` each, so
/// a Codex swap never starts Claude's cooldown or no-return window.
#[derive(Default)]
pub(crate) struct SwapGuards {
    pub(crate) by_provider: std::collections::HashMap<String, SwapGuard>,
}

impl SwapGuards {
    pub(crate) fn for_provider(&mut self, slug: &str) -> &mut SwapGuard {
        self.by_provider.entry(slug.to_string()).or_default()
    }
}

/// One switchable provider's result for a poll cycle.
pub(crate) struct ProviderCycle {
    pub slug: String,
    /// The provider's active account after this cycle (post-swap).
    pub active: Option<String>,
    /// The swap made this cycle, if any: (from, to).
    pub swapped: Option<(String, String)>,
    /// The active account's usage fetch was rate-limited (HTTP 429).
    pub rate_limited: bool,
    /// Accounts whose usage was fetched this cycle.
    pub fetched: usize,
    /// Accounts whose token refresh or usage fetch failed this cycle.
    pub failed: usize,
    /// The ACTIVE account's `Row::max_pct()` (session OR weekly, whichever is
    /// tighter), feeding `next_interval`'s cadence ramp. v0.5.13: scoped to the
    /// active account (see `cadence_max_pct`) so an inactive, already-maxed
    /// account can't pin the cadence at the 30s backstop indefinitely.
    pub max_pct: Option<f64>,
    /// Whether a swap is actionable this cycle: an eligible target exists for
    /// the active account, even if a cooldown currently blocks it. Feeds
    /// `next_interval` so the 30s backstop only engages when there is actually
    /// something to catch — a maxed active account with no eligible target
    /// (single-account user, or all others full/needs_relogin) falls back to
    /// BASE instead of hammering the usage endpoint into HTTP 429 (v0.5.17).
    pub actionable: bool,
    /// Auto-swap v2's urgency for this provider (`None` under the classic
    /// engine): selects the v2 cadence (60 s TIGHT floor, 120 s band cap).
    pub urgency: Option<autoswap::Urgency>,
    /// Auto-swap v2's next check time (estimate crossing, reset, cooldown
    /// expiry, pre-verify refresh): the loop wakes then even with no fetch due.
    pub next_check_at: Option<DateTime<Utc>>,
}

/// Result of one poll: one entry per switchable provider with accounts.
pub(crate) struct CycleOutcome {
    pub providers: Vec<ProviderCycle>,
}

/// `verify_swap_target`, telling a 429 (`Err(true)`, which arms the v2 storm
/// guard) from any other failure (`Err(false)`).
pub(crate) fn verify_swap_target_detailed(slug: &str, key: &str) -> std::result::Result<Row, bool> {
    let state = State::load().map_err(|_| false)?;
    let tracker = fetch_tracker_key(slug, key);
    let now_ts = Utc::now().timestamp();
    let mut rate_limited = false;
    let previous: Option<CachedUsage>;
    let fetched: std::result::Result<CachedUsage, String> = if slug == CLAUDE_SLUG {
        let acct = state.find(key).ok_or(false)?;
        previous = acct.cached_usage.clone();
        // Claude's `expires_at` is epoch millis.
        if acct.needs_relogin || acct.expires_at <= Utc::now().timestamp_millis() {
            return Err(false);
        }
        match usage::fetch(&acct.access_token) {
            Ok(u) => {
                let mut cu = cached_from_usage(&u);
                attach_prepaid_balance(&mut cu, acct);
                Ok(cu)
            }
            Err(e) => {
                if matches!(e, usage::FetchError::RateLimited) {
                    rate_limited = true;
                    record_usage_fetch_429(&tracker);
                } else {
                    record_usage_fetch_non_429(&tracker);
                }
                Err(e.to_string())
            }
        }
    } else {
        let acct = state.find_provider_account(slug, key).ok_or(false)?;
        previous = acct.cached_usage.clone();
        // `ProviderAccount::expires_at` is epoch seconds (0 = unknown).
        if acct.needs_relogin || (acct.expires_at != 0 && acct.expires_at <= now_ts) {
            return Err(false);
        }
        match provider_by_slug(slug)
            .map_err(|_| false)?
            .fetch_account_usage(&acct.access_token, &acct.secret_blob)
        {
            Ok(snap) => Ok(cached_from_usage_snapshot(&snap)),
            Err(e) => {
                if matches!(e, ProviderError::RateLimited { .. }) {
                    rate_limited = true;
                    record_usage_fetch_429(&tracker);
                } else {
                    record_usage_fetch_non_429(&tracker);
                }
                Err(e.to_string())
            }
        }
    };
    let cu = match fetched {
        Ok(cu) => cu,
        Err(e) => {
            record_inactive_fetch_failure(&tracker, now_ts);
            logging::log(&format!(
                "swap target {slug}/{key}: usage re-check failed: {e}"
            ));
            return Err(rate_limited);
        }
    };
    record_usage_fetch_success(&tracker);
    if let Some(prev) = &previous {
        log_epoch_ends(slug, key, prev, &cu);
    }
    let saved = with_state_lock(|| {
        let mut st = State::load()?;
        if slug == CLAUDE_SLUG {
            if let Some(a) = st.find_mut(key) {
                a.cached_usage = Some(cu.clone());
            }
        } else if let Some(a) = st.find_provider_account_mut(slug, key) {
            a.cached_usage = Some(cu.clone());
        }
        st.save()?;
        Ok(provider_rows(&st, slug)
            .into_iter()
            .find(|r| r.email.eq_ignore_ascii_case(key)))
    });
    match saved {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(false),
        Err(e) => {
            logging::log(&format!(
                "swap target {slug}/{key}: saving reading failed: {e:#}"
            ));
            Err(false)
        }
    }
}

/// Epoch-end stranding log (§6, B18): when a window's reported reset changes
/// (it reset), log the last pre-reset pct — how much room expired unused.
pub(crate) fn log_epoch_ends(slug: &str, key: &str, old: &CachedUsage, new: &CachedUsage) {
    for (name, old_pct, old_reset, new_reset) in [
        (
            "session",
            old.session_pct,
            old.session_reset.as_deref(),
            new.session_reset.as_deref(),
        ),
        (
            "weekly",
            old.weekly_pct,
            old.weekly_reset.as_deref(),
            new.weekly_reset.as_deref(),
        ),
    ] {
        let (Some(pct), Some(o), Some(n)) = (old_pct, old_reset, new_reset) else {
            continue;
        };
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp());
        if parse(o).is_some() && parse(o) != parse(n) {
            // R2-m4: room left on an account held back by a `trigger_left`
            // latch is the capacity-ordering cost of the latch.
            let latched = State::load().ok().is_some_and(|st| {
                st.swap_memory
                    .get(slug)
                    .and_then(|m| m.trigger_latch(key))
                    .is_some_and(|l| l.window.as_str() == name && l.resets_at == parse(o))
            });
            let unused = (100.0 - pct).max(0.0);
            logging::log(&format!(
                "event=epoch_end provider={slug} account={key} window={name} last_pct={pct:.0} \
                 unused_pts={unused:.0} latched_pts={:.0} reset={o}",
                if latched { unused } else { 0.0 }
            ));
        }
    }
}

/// Poll usage for every account (the only network path), record history, and
/// auto-swap each switchable provider's active account away if it has
/// reached `trigger` and a healthy target exists. One core for every
/// provider: Claude and Codex (and any future switchable provider) go through
/// the same `provider_swap_cycle`. Shared by `usagio watch` and the menu-bar
/// poller.
pub(crate) fn watch_cycle(
    trigger: f64,
    ceiling: f64,
    guards: &mut SwapGuards,
    force: bool,
) -> Result<CycleOutcome> {
    complete_pending_switches();
    // Pre-cycle: absorb any on-disk credential rotations the vendor CLI made
    // behind our back (fsnotify may not fire on remote/network volumes, and
    // we want the invariant to hold even on the polling path). Then refresh
    // INACTIVE Claude accounts eagerly so a user who switches to one later
    // doesn't have to wait for the reactive path to notice.
    for p in providers::all() {
        let _ = credentials::absorb_all_lagging(p.as_ref());
    }
    credentials::refresh_inactive_if_stale(State::load().ok().and_then(|s| s.active).as_deref());

    // v2 budget (§5.7): an active fetch also respects the per-minute cap.
    let budget_now = Utc::now().timestamp();
    let spent: std::collections::HashSet<String> = providers::all()
        .iter()
        .map(|p| p.provider_id())
        .filter(|slug| {
            guards
                .for_provider(slug)
                .v2
                .budget
                .allow("", budget_now, true, false, 0)
                .is_err()
        })
        .map(str::to_string)
        .collect();
    let claude_refresh = refresh_usage_cache_with(force, spent.contains(CLAUDE_SLUG));
    warn_expiring_logins();
    // Gap-4 (codex-switch-e2e): drive the active-account CAS refresh for
    // every non-Claude provider that has one wired (currently just codex),
    // then poll their usage under the same fetch policy as Claude.
    for p in providers::all() {
        if p.provider_id() != CLAUDE_SLUG {
            refresh_provider_active_account(p.provider_id());
        }
    }
    let provider_refresh = refresh_provider_usage_caches_with(force, trigger, &spent);
    settle_manual_picks();

    let state = State::load()?;
    let now_ts = Utc::now().timestamp();
    let mut providers_out = Vec::new();
    for p in providers::all() {
        let slug = p.provider_id();
        if !provider_supports_swap(slug) {
            continue;
        }
        let mut rows = provider_rows(&state, slug);
        if rows.is_empty() {
            continue;
        }
        let active = provider_active(&state, slug);
        if slug == CLAUDE_SLUG {
            append_history(&rows, active.as_deref());
        }
        let (rate_limited, active_fetched, saw_429) = if slug == CLAUDE_SLUG {
            (
                claude_refresh.rate_limited,
                claude_refresh.active_fetched,
                claude_refresh.saw_429,
            )
        } else {
            (
                provider_refresh.rate_limited.contains(slug),
                provider_refresh.active_fetched.contains(slug),
                provider_refresh.saw_429.contains(slug),
            )
        };
        let (fetched, failed) = if slug == CLAUDE_SLUG {
            (claude_refresh.fetched, claude_refresh.failed)
        } else {
            (
                provider_refresh.fetched.get(slug).copied().unwrap_or(0),
                provider_refresh.failed.get(slug).copied().unwrap_or(0),
            )
        };
        let max_pct = cadence_max_pct(&rows, active.as_deref());
        let guard = guards.for_provider(slug);
        guard.manual_locked_choice = state
            .manual_hold(slug)
            .map(|k| (slug.to_string(), k.to_string()));
        guard.manual_pick = state
            .manual_pick(slug)
            .map(|k| (slug.to_string(), k.to_string()));
        guard.use_credits = state.use_credits_when_exhausted;
        if active_fetched {
            guard.v2.budget.note_request(now_ts);
        }
        if saw_429 {
            guard.v2.budget.note_429(now_ts);
        }
        let (swapped, actionable, urgency, next_check_at) = match active.as_deref() {
            Some(a) => {
                let r = autoswap::shell::run_cycle(
                    slug,
                    &mut rows,
                    a,
                    trigger,
                    ceiling,
                    &mut guard.v2,
                )?;
                (r.swapped, r.actionable, Some(r.urgency), r.next_check_at)
            }
            None => (None, false, None, None),
        };
        let next_check_at = if State::load()
            .ok()
            .and_then(|latest| {
                latest
                    .provider_accounts(slug)
                    .and_then(|accounts| accounts.pending_switch.clone())
            })
            .is_some()
        {
            Some(Utc::now() + chrono::Duration::seconds(NEXT_CHECK_MIN_SECS as i64))
        } else {
            next_check_at
        };
        providers_out.push(ProviderCycle {
            slug: slug.to_string(),
            active,
            swapped,
            rate_limited,
            fetched,
            failed,
            max_pct,
            actionable,
            urgency,
            next_check_at,
        });
    }
    claude_profiles::refresh(
        if force {
            usize::MAX
        } else {
            claude_profiles::BATCH_SIZE
        },
        force,
    );
    Ok(CycleOutcome {
        providers: providers_out,
    })
}

/// One provider's auto-swap decision and execution for this cycle. Returns
/// the swap made (from, to), if any, and whether a swap is actionable (an
/// eligible target exists, even if a cooldown currently blocks it) — the
/// latter drives the cadence backstop (see `next_interval`).
/// Test entry point (`watch_cycle` dispatches inline so it can carry the v2
/// urgency to the cadence): v2 delegates to `autoswap::shell::run_cycle`.
#[cfg(test)]
pub(crate) fn provider_swap_cycle(
    slug: &str,
    rows: &mut [Row],
    active_email: &str,
    trigger: f64,
    ceiling: f64,
    guard: &mut SwapGuard,
) -> Result<(Option<(String, String)>, bool)> {
    {
        let r =
            autoswap::shell::run_cycle(slug, rows, active_email, trigger, ceiling, &mut guard.v2)?;
        Ok((r.swapped, r.actionable))
    }
}
