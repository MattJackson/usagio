//! The non-Claude provider usage refresh pass.

use anyhow::Result;
use chrono::Utc;

use crate::poll::claude::{
    cache_crossed_reset, replay_row, skip_row_due, v2_active_fetch_skip, MAX_LOCKED_STALENESS_SECS,
};
use crate::poll::plan::{
    detect_lapsed_plan, persist_plan, plan_after_failed_fetch, recheck_lapsed_plan, Recheck,
};
use crate::poll::trackers::{
    claim_subscription_check, fetch_tracker_key, get_usage_fetch_tracker,
    note_polled_provider_active, note_refresh_error, record_fetch_attempt_failed,
    record_inactive_fetch_failure, record_usage_fetch_429, record_usage_fetch_non_429,
    record_usage_fetch_success, INACTIVE_FETCH_FLOOR_SECS,
};
use crate::providers::trait_def::{PlanStatus, ProviderError};
use crate::providers::CLAUDE_SLUG;
use crate::rows::{cached_from_usage_snapshot, cached_to_account_usage};
use crate::state_ops::with_state_lock;
use crate::store::{CachedUsage, ProviderAccount, State};
use crate::watch::cycle::log_epoch_ends;
use crate::{countdown, lifecycle, logging, providers, usage_log};

/// Fetch usage for every captured non-Claude provider account and persist it
/// into `state.providers[slug]`. The counterpart to the Claude loop in
/// `refresh_usage_cache` — without it, a captured Codex/etc. account renders
/// "no data yet" forever because `Provider::fetch_usage` is otherwise never
/// called. Best-effort throughout: a token-refresh or usage error keeps the
/// existing cache and moves on (never panics the poll loop). Applies the same
/// locked-until-reset skip so a maxed provider account doesn't burn requests.
/// Refresh non-Claude provider accounts (state v2 `providers[slug]`) under
/// the same fetch policy as Claude's `refresh_usage_cache`: the active account
/// is fetched first on its cadence tier; a 429 on it skips that provider's
/// inactive fetches this cycle; inactive accounts get the flat
/// `INACTIVE_FETCH_FLOOR_SECS` floor and their own failure backoff (a reset
/// crossing or `force` bypasses both). Returns the slugs whose ACTIVE account
/// was rate-limited, for the poll cadence.
/// What one `refresh_provider_usage_caches` pass saw, per provider slug.
#[derive(Default)]
pub(crate) struct ProviderRefresh {
    /// The ACTIVE account's usage fetch was rate-limited.
    pub(crate) rate_limited: std::collections::HashSet<String>,
    /// The active account's usage was requested (v2 budget).
    pub(crate) active_fetched: std::collections::HashSet<String>,
    /// Any account's request 429'd (v2 storm guard).
    pub(crate) saw_429: std::collections::HashSet<String>,
    /// Accounts whose usage was fetched, per slug.
    pub(crate) fetched: std::collections::HashMap<String, usize>,
    /// Accounts whose token refresh or usage fetch failed, per slug.
    pub(crate) failed: std::collections::HashMap<String, usize>,
}

#[cfg(test)]
pub(crate) fn refresh_provider_usage_caches(force: bool, trigger: f64) -> ProviderRefresh {
    refresh_provider_usage_caches_with(force, trigger, &std::collections::HashSet::new())
}

/// `refresh_provider_usage_caches`, deferring the active fetch of the slugs in
/// `defer_active` (their v2 request budget is spent, §5.7).
pub(crate) fn refresh_provider_usage_caches_with(
    force: bool,
    trigger: f64,
    defer_active: &std::collections::HashSet<String>,
) -> ProviderRefresh {
    let mut out = ProviderRefresh::default();
    let state = match State::load() {
        Ok(s) => s,
        Err(_) => return out,
    };
    for provider in providers::all() {
        let slug = provider.provider_id();
        if slug == CLAUDE_SLUG {
            continue;
        }
        let Some(pa) = state.providers.get(slug) else {
            continue;
        };
        let active_key = pa.active.clone();
        // A cache taken while the new active was INACTIVE must not be trusted
        // by the active fetch skip (same bypass as the Claude loop).
        let active_just_switched = note_polled_provider_active(slug, active_key.as_deref());
        let mut keys: Vec<String> = pa.accounts.iter().map(|a| a.key.clone()).collect();
        // Active first: it gets first claim on the request budget, and a 429
        // on it stops this provider's inactive fetches for the cycle.
        if let Some(i) = active_key
            .as_deref()
            .and_then(|a| keys.iter().position(|k| k.eq_ignore_ascii_case(a)))
        {
            let k = keys.remove(i);
            keys.insert(0, k);
        }
        let mut active_rate_limited = false;
        let mut outcomes: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        for key in keys.clone() {
            let Some(acct) = state.find_provider_account(slug, &key).cloned() else {
                continue;
            };
            let is_active = active_key
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(&key));
            if acct.needs_relogin {
                continue;
            }
            if let Err(e) = lifecycle::validate_stored_identity(provider.as_ref(), &acct) {
                logging::log(&format!(
                    "event=credential_identity_mismatch provider={slug} account={key} reason={e}"
                ));
                let _ = with_state_lock(|| {
                    let mut st = State::load()?;
                    if let Some(a) = st.find_provider_account_mut(slug, &key) {
                        if a.secret_blob == acct.secret_blob {
                            a.needs_relogin = true;
                            a.cached_usage = None;
                        }
                    }
                    st.save()
                });
                continue;
            }
            // Same lapsed-plan gate as the Claude loop.
            let now_ts = Utc::now().timestamp();
            let tracker_key = fetch_tracker_key(slug, &key);
            let recheck_subscription = acct.no_subscription;
            if recheck_subscription && !claim_subscription_check(&tracker_key, now_ts, force) {
                continue;
            }
            // Locked accounts don't change until reset — skip (same guard as
            // the Claude path; the reset-boundary wake brings us back). A manual
            // refresh (`force`) never skips, and past MAX_LOCKED_STALENESS we
            // refresh anyway so an early/unexpected reset can't freeze it.
            // The ACTIVE account is never skipped here (as in the Claude
            // path): `v2_active_fetch_skip` below decides it.
            if let Some(cu) = &acct.cached_usage {
                let stale = Utc::now().timestamp() - cu.fetched_at > MAX_LOCKED_STALENESS_SECS;
                if !force
                    && !is_active
                    && !recheck_subscription
                    && !stale
                    && countdown::is_locked_until_reset(&cached_to_account_usage(cu), Utc::now())
                {
                    continue;
                }
            }
            if !force && !is_active && !recheck_subscription {
                if active_rate_limited {
                    logging::log(&format!(
                        "poll: active {slug} account rate limited; skipping {key} this cycle \
                         (event=fetch_skip_active_429 provider={slug} account={key})"
                    ));
                    continue;
                }
                let crossed_reset = acct
                    .cached_usage
                    .as_ref()
                    .is_some_and(|cu| cache_crossed_reset(cu, Utc::now()));
                let tracker = get_usage_fetch_tracker(&tracker_key);
                if let (Some(until), false) = (tracker.backoff_until_ts, crossed_reset) {
                    if now_ts < until {
                        continue;
                    }
                }
                if let Some(cu) = &acct.cached_usage {
                    let age = now_ts - cu.fetched_at;
                    if age >= 0 && age < INACTIVE_FETCH_FLOOR_SECS as i64 && !crossed_reset {
                        continue;
                    }
                }
            }
            if !force && is_active && defer_active.contains(slug) {
                continue;
            }
            if !force && is_active && !active_just_switched {
                if let Some(cu) = &acct.cached_usage {
                    {
                        if v2_active_fetch_skip(slug, &key, cu, trigger, now_ts).is_some() {
                            continue;
                        }
                    }
                }
            }
            // Refresh the token if it's at/near expiry and we have a refresh
            // token; on failure keep the cache and try again next cycle.
            let request_account = if !is_active {
                match lifecycle::ensure_provider_fresh(provider.as_ref(), &key) {
                    Ok(fresh) => fresh,
                    Err(e) => {
                        logging::log(&format!(
                            "provider refresh failed for {slug}/{key}: {e}; keeping cache"
                        ));
                        note_refresh_error(
                            &tracker_key,
                            Some(format!("token refresh failed: {e}")),
                        );
                        *out.failed.entry(slug.to_string()).or_default() += 1;
                        continue;
                    }
                }
            } else {
                acct.clone()
            };
            let access = request_account.access_token.clone();
            let mut plan: Option<PlanStatus> = None;
            if recheck_subscription {
                match recheck_lapsed_plan(provider.as_ref(), &tracker_key, &access) {
                    Recheck::StillLapsed(p) => {
                        let _ =
                            persist_provider_observation(slug, &request_account, None, Some(&p));
                        continue;
                    }
                    Recheck::Restored(p) => plan = Some(p),
                    Recheck::Unknown => {}
                }
            }
            if is_active {
                out.active_fetched.insert(slug.to_string());
            }
            let cu = match provider.fetch_account_usage(&access, &request_account.secret_blob) {
                Ok(snap) => {
                    record_usage_fetch_success(&tracker_key);
                    *out.fetched.entry(slug.to_string()).or_default() += 1;
                    if snap.plan.is_some() {
                        plan = snap.plan.clone();
                    }
                    let cu = cached_from_usage_snapshot(&snap);
                    if let Some(prev) = &acct.cached_usage {
                        log_epoch_ends(slug, &key, prev, &cu);
                    }
                    outcomes.insert(key.clone(), "ok");
                    let row = replay_row(slug, &key, &cu, is_active, "ok");
                    if let Err(e) = usage_log::append(&row) {
                        logging::log(&format!("usage_log: append failed for {slug}/{key}: {e:#}"));
                    }
                    Some(cu)
                }
                Err(e) => {
                    record_fetch_attempt_failed(&tracker_key, now_ts);
                    *out.failed.entry(slug.to_string()).or_default() += 1;
                    if matches!(e, ProviderError::RateLimited { .. }) {
                        outcomes.insert(key.clone(), "429");
                        out.saw_429.insert(slug.to_string());
                        record_usage_fetch_429(&tracker_key);
                        if is_active {
                            active_rate_limited = true;
                            out.rate_limited.insert(slug.to_string());
                        }
                    } else {
                        outcomes.insert(key.clone(), "err");
                        record_usage_fetch_non_429(&tracker_key);
                        note_refresh_error(&tracker_key, Some(format!("usage fetch failed: {e}")));
                    }
                    if !is_active {
                        record_inactive_fetch_failure(&tracker_key, now_ts);
                    }
                    logging::log(&format!(
                        "provider usage error for {slug}/{key}: {e}; keeping cache"
                    ));
                    if matches!(
                        e,
                        ProviderError::RateLimited { .. } | ProviderError::Other(_)
                    ) {
                        plan = plan_after_failed_fetch(
                            plan,
                            detect_lapsed_plan(provider.as_ref(), &tracker_key, &access, now_ts),
                        );
                    }
                    None
                }
            };
            let persisted =
                persist_provider_observation(slug, &request_account, cu.as_ref(), plan.as_ref())
                    .unwrap_or(false);
            if cu.is_some() && persisted {
                logging::log(&format!(
                    "event=provider_usage_refreshed provider={slug} account={key}"
                ));
            }
        }
        // One replay row per account per cycle (§6), Codex included.
        for key in &keys {
            let outcome = outcomes.get(key).copied().unwrap_or("skip");
            if outcome == "ok" {
                continue;
            }
            if let Some(cu) = state
                .find_provider_account(slug, key)
                .and_then(|a| a.cached_usage.as_ref())
            {
                let is_active = active_key
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(key));
                if outcome == "skip" && !skip_row_due(slug, key, is_active, Utc::now().timestamp())
                {
                    continue;
                }
                let row = replay_row(slug, key, cu, is_active, outcome);
                if let Err(e) = usage_log::append(&row) {
                    logging::log(&format!("usage_log: append failed for {slug}/{key}: {e:#}"));
                }
            }
        }
    }
    out
}

/// Publish an observation only if its credential and starting cache still own
/// this account. A login or faster poll can finish while the request is in flight.
pub(crate) fn persist_provider_observation(
    slug: &str,
    snapshot: &ProviderAccount,
    usage: Option<&CachedUsage>,
    plan: Option<&PlanStatus>,
) -> Result<bool> {
    with_state_lock(|| {
        let mut state = State::load()?;
        let Some(current) = state.find_provider_account_mut(slug, &snapshot.key) else {
            return Ok(false);
        };
        if current.secret_blob != snapshot.secret_blob
            || current.access_token != snapshot.access_token
            || current.identity_snapshot() != snapshot.identity_snapshot()
            || current.cached_usage != snapshot.cached_usage
            || current.plan != snapshot.plan
            || current.no_subscription != snapshot.no_subscription
        {
            return Ok(false);
        }
        if let Some(usage) = usage {
            current.cached_usage = Some(usage.clone());
            state.save()?;
        }
        if let Some(plan) = plan {
            // The reentrant state lock keeps generation validation and the
            // existing plan notifications in one serialized operation.
            persist_plan(slug, &snapshot.key, plan, None);
        }
        Ok(true)
    })
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
