//! The Claude usage refresh pass (`refresh_usage_cache`).

use chrono::{DateTime, Utc};

use crate::claude_local::read_claude_identity;
use crate::cli::token::poll_active_cas;
use crate::lifecycle::ActiveRefreshOutcome;
use crate::poll::plan::{detect_lapsed_plan, persist_plan, recheck_lapsed_plan, Recheck};
use crate::poll::trackers::{
    active_fetch_in_backoff, claim_subscription_check, get_usage_fetch_tracker, note_polled_active,
    note_refresh_error, record_fetch_attempt_failed, record_inactive_fetch_failure,
    record_usage_fetch_429, record_usage_fetch_non_429, record_usage_fetch_success,
    INACTIVE_FETCH_FLOOR_SECS,
};
use crate::providers::claude::{oauth, usage};
use crate::providers::CLAUDE_SLUG;
use crate::rows::{attach_prepaid_balance, cached_from_usage, cached_to_account_usage};
use crate::state_ops::{save_if_changed, with_state_lock};
use crate::store::{Account, CachedUsage, State};
use crate::switching::claude::ensure_fresh_with_fallback;
use crate::switching::identity::{
    mirror_inactive_rotation, sync_active_claude_identity_from, sync_active_from_keychain_with,
};
use crate::switching::provider::provider_by_slug;
use crate::watch::cadence::{
    tier_interval_for_pct, TRIGGER_PCT, WATCH_INTERVAL_SECS, WATCH_TIGHT_BAND,
};
use crate::watch::cycle::log_epoch_ends;
use crate::{autoswap, countdown, logging, notifications, usage_log};

// ---------------------------------------------------------------------------
// Usage refresh (the ONE network path)
// ---------------------------------------------------------------------------

/// Outcome of a usage refresh.
#[derive(Default)]
pub(crate) struct RefreshOutcome {
    /// True if the ACTIVE account got HTTP 429 (caller should back off).
    pub(crate) rate_limited: bool,
    /// The active account's usage was requested this cycle (v2 budget).
    pub(crate) active_fetched: bool,
    /// Any account's usage request 429'd this cycle (v2 storm guard).
    pub(crate) saw_429: bool,
    /// Accounts whose usage was fetched this cycle.
    pub(crate) fetched: usize,
    /// Accounts whose token refresh or usage fetch failed this cycle.
    pub(crate) failed: usize,
}

/// The ONE place that calls the usage API. Does all network work (token refresh
/// if near expiry, usage fetch) OUTSIDE the state lock, then takes the lock,
/// reloads state, merges the fresh tokens + `cached_usage` by email, and saves —
/// so a concurrent switch is never clobbered. On 429/transient error it KEEPS
/// the existing cache.
/// Max age a *locked* account's cached usage may reach before we refresh it
/// anyway, even though `is_locked_until_reset` says to skip. Bounds staleness so
/// an account that reset or got a temporary limit boost EARLIER than its cached
/// reset can't stay frozen at a stale "locked" reading until the (wrong) reset.
pub(crate) const MAX_LOCKED_STALENESS_SECS: i64 = autoswap::policy::MAX_LOCKED_STALENESS_S;

pub(crate) fn refresh_usage_cache(force: bool) -> RefreshOutcome {
    refresh_usage_cache_with(force, false)
}

/// `refresh_usage_cache`, optionally deferring the ACTIVE account's fetch
/// (`defer_active`: the v2 per-provider request budget is spent, §5.7).
pub(crate) fn refresh_usage_cache_with(force: bool, defer_active: bool) -> RefreshOutcome {
    let mut state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            logging::log(&format!("poll: state load failed: {e}"));
            return RefreshOutcome::default();
        }
    };
    // v1 state is Claude-only; resolve the provider once at the top of the
    // cycle. Phase 3 (state v2) makes this a per-account bucket lookup — the
    // per-account body below already runs inside a loop, so the transition is
    // additive rather than restructural.
    let provider = match provider_by_slug(CLAUDE_SLUG) {
        Ok(p) => p,
        Err(e) => {
            logging::log(&format!("poll: provider unavailable: {e:#}"));
            return RefreshOutcome::default();
        }
    };
    // Parse ~/.claude.json once for the whole cycle: the keychain sync and
    // the commit-time metadata refresh below share this reading.
    let claude_identity = read_claude_identity().0;
    sync_active_from_keychain_with(provider, &mut state, claude_identity.as_ref());
    logging::log("poll: refreshing usage cache");

    // Per-account fetch floor uses the same tier ramp as `next_interval`; read
    // the user's configured trigger once so we don't reload state per account.
    let trigger_for_floor = state.trigger_pct.unwrap_or(TRIGGER_PCT);
    let now_ts = Utc::now().timestamp();
    // Per-account outcome for the replay rows (§6); "skip" unless fetched.
    let mut outcomes: std::collections::HashMap<String, &'static str> =
        std::collections::HashMap::new();
    // Token refreshes that failed before any fetch (no replay outcome).
    let mut refresh_failed = 0usize;
    let mut active_fetched = false;
    let mut saw_429 = false;

    let mut rate_limited = false;
    // (email, refreshed account after ensure_fresh, new cached usage or None,
    // updated notif state or None)
    let mut updates: Vec<(
        String,
        Account,
        Option<CachedUsage>,
        Option<notifications::NotifState>,
    )> = Vec::new();
    let mut emails: Vec<String> = state.accounts.iter().map(|a| a.key().to_string()).collect();
    // Active account first: it gets first claim on the request budget, and a
    // 429 on it stops the inactive fetches below for this cycle.
    if let Some(active) = state.active.as_deref() {
        if let Some(i) = emails.iter().position(|e| e == active) {
            let e = emails.remove(i);
            emails.insert(0, e);
        }
    }
    // A switch since the last refresh: fetch the new active account now rather
    // than trusting a cache taken while it was inactive.
    let active_just_switched = note_polled_active(state.active.as_deref());
    // Settings ▸ Notifications ▸: per-trigger enable flags, persisted on
    // `State` and toggled from the menu bar (`menubar::toggle_notification_trigger`).
    let notif_cfg = state.notification_config.clone();
    for email in &emails {
        let Some(acct) = state.find(email).cloned() else {
            continue;
        };
        let mut acct = acct;
        // Skip accounts already flagged needs_relogin — no useful token to
        // reason about, and hammering an invalid_grant refresh both wastes
        // requests and can trip rate limits shared across the tenant. Except
        // the ACTIVE account: its token is whatever Claude Code holds, and the
        // active-refresh CAS below is the only path that adopts it and clears
        // the flag — skipping it froze the active account's reading (no
        // auto-swap, wrong cadence) after a manual switch to a flagged account.
        let active_with_refresh =
            state.active.as_deref() == Some(email.as_str()) && provider.supports_active_refresh();
        if acct.needs_relogin && !active_with_refresh {
            continue;
        }
        // A lapsed subscription never calls the usage endpoint (every call is
        // a 403/429 that burns the tenant's request budget). It only gets a
        // profile re-check every SUBSCRIPTION_RECHECK_SECS (or on a forced
        // refresh), done below once the token is fresh.
        let recheck_subscription = acct.no_subscription;
        if recheck_subscription && !claim_subscription_check(email, now_ts, force) {
            continue;
        }
        // Locked accounts don't change until their reset — skip the network
        // refresh entirely, keeping the (accurate) locked cache. The
        // reset-boundary wake (`menubar::next_reset_wake_secs`) brings the
        // poller back at expiry, when `is_locked_until_reset` flips false and
        // the account refreshes to its fresh post-reset reading. This is the
        // primary rate-limit guard: a shelf of 100%-locked accounts polled
        // every cycle exhausts the shared tenant's request budget and
        // 429-starves the accounts that actually need refreshing (what froze
        // an unlocked account at a stale reading). The ACTIVE account is never
        // skipped — its live reading must stay current.
        let is_the_active_account = state.active.as_deref() == Some(email.as_str());
        if !is_the_active_account {
            if let Some(cu) = &acct.cached_usage {
                // Skip the refresh for a locked account ONLY while its cache is
                // still fresh. The skip is a rate-limit guard (a shelf of locked
                // accounts polled every cycle 429-starves the tenant), but it
                // trusted the stored reset time absolutely — so when an account
                // reset or got a temporary limit boost EARLIER than the cached
                // reset, usagio never re-checked and showed a stale "locked" read
                // indefinitely (e.g. weekly reset early / a 50%-boost freed it,
                // website 0% but menu 100%). Cap that: past MAX_LOCKED_STALENESS
                // we refresh even a "locked" account, bounding staleness to 12h
                // and catching early/unexpected resets, while still skipping the
                // vast majority of per-cycle locked polls.
                let stale = Utc::now().timestamp() - cu.fetched_at > MAX_LOCKED_STALENESS_SECS;
                if !force
                    && !recheck_subscription
                    && !stale
                    && countdown::is_locked_until_reset(&cached_to_account_usage(cu), Utc::now())
                {
                    logging::log(&format!(
                        "poll: {email} locked until reset; skipping refresh (event=refresh_skip_locked account={email})"
                    ));
                    continue;
                }
            }
        }
        // The active account is fetched first; if it just got a 429, leave
        // the inactive accounts alone this cycle (no token or usage calls)
        // so the budget recovers for the account a failover depends on.
        if !force && !is_the_active_account && !recheck_subscription && rate_limited {
            logging::log(&format!(
                "poll: active account rate limited; skipping {email} this cycle \
                 (event=fetch_skip_active_429 account={email})"
            ));
            continue;
        }
        // The ACTIVE account is adopt-only (`lifecycle::active_refresh_cas`):
        // usagio never POSTs a refresh grant for it, it adopts whatever the
        // vendor client left in the slot (or republishes a renewal whose live
        // write failed), all under the state lock so it cannot race a
        // concurrent switch/capture. Every other account uses the plain
        // network-outside-the-lock refresh below.
        // `supports_active_refresh` gates that path. Claude returns true; a
        // provider returning false would fall through to `ensure_fresh` for
        // its active account too, which POSTs — so a provider driven through
        // this loop must opt in.
        let is_active =
            state.active.as_deref() == Some(email.as_str()) && provider.supports_active_refresh();
        if is_active {
            let cas = poll_active_cas(provider, email);
            match cas {
                Ok((Some(ActiveRefreshOutcome::RefreshFailed), _)) => {
                    // Keep the existing cache this cycle; retry next tick.
                    note_refresh_error(email, Some("token refresh failed".to_string()));
                    refresh_failed += 1;
                    continue;
                }
                Ok((Some(_), Some(fresh))) => {
                    // `Adopted` leaves `fresh` holding the tokens Claude Code
                    // owns, which is what the usage fetch below uses.
                    acct = fresh;
                }
                Ok((Some(_), None)) | Ok((None, _)) => {
                    // Account vanished (a concurrent `rm`) mid-cycle.
                    continue;
                }
                Err(e) => {
                    logging::log(&format!(
                        "poll: active CAS state operation failed for {email}: {e:#}"
                    ));
                    refresh_failed += 1;
                    continue;
                }
            }
        } else {
            // Fingerprint BEFORE the refresh replaces the token in place.
            let old_prefix = logging::token_fingerprint(&acct.access_token);
            // Same disk-fallback as the other reactive entry points, so a
            // rotation that exists only on disk doesn't flag needs_relogin.
            match ensure_fresh_with_fallback(provider, email, &mut acct) {
                Ok(true) => {
                    // mirror_back is `skip` (not `ok`/`err`) when the vendor
                    // CLI's own active identity doesn't match this account —
                    // Claude Code's keychain/credentials-file slot is a
                    // single shared resource across every locally known
                    // account (there's no per-account file), so blindly
                    // mirroring an inactive account's rotation into it would
                    // clobber whichever account is genuinely logged in right
                    // now. Only mirror when the vendor agrees this account IS
                    // its current active identity.
                    let mirror_back = mirror_inactive_rotation(provider, &acct);
                    logging::log(&format!(
                        "event=inactive_refresh account={email} at_prefix={old_prefix}.. -> \
                         {}.. mirror_back={}",
                        logging::token_fingerprint(&acct.access_token),
                        match &mirror_back {
                            Some(Ok(())) => "ok",
                            Some(Err(_)) => "err",
                            None => "skip",
                        }
                    ));
                }
                Ok(false) => {}
                Err(oauth::RefreshError::InvalidGrant) => {
                    logging::log(&format!(
                        "token refresh permanently rejected for {email} (invalid_grant); \
                         flagging for re-login"
                    ));
                    logging::log(&format!(
                        "event=needs_relogin account={email} reason=invalid_grant"
                    ));
                    acct.needs_relogin = true;
                    // The merge only flags if the stored token is still the
                    // one that was rejected: `ensure_fresh` left `acct`
                    // holding the grant it actually sent (a saved rotation or
                    // the disk fallback's may differ from the snapshot's).
                    // Fall through to the merge step so the flag is persisted;
                    // skip the usage fetch — a rejected refresh means we don't
                    // have a usable access token to try the usage endpoint with.
                    updates.push((email.clone(), acct, None, None));
                    continue;
                }
                Err(e) => {
                    logging::log(&format!(
                        "token refresh failed for {email}: {e} (keeping cache)"
                    ));
                    note_refresh_error(email, Some(format!("token refresh failed: {e}")));
                    refresh_failed += 1;
                    continue;
                }
            }
        }
        if recheck_subscription {
            match recheck_lapsed_plan(provider, email, &acct.access_token) {
                Recheck::Restored(p) => {
                    persist_plan(CLAUDE_SLUG, email, &p, None);
                    acct.no_subscription = false;
                    // Fall through: fetch real usage for it this cycle.
                }
                Recheck::StillLapsed(p) => {
                    persist_plan(CLAUDE_SLUG, email, &p, None);
                    continue;
                }
                // Unknown (profile unreachable) — stay lapsed, try later.
                Recheck::Unknown => continue,
            }
        }
        // Per-account fetch floor: the account's own cadence tier gates whether
        // we actually hit `/api/oauth/usage` this cycle. The GLOBAL loop cadence
        // is driven by the active account's `max_pct` — so a single account in
        // the TIGHT band would otherwise drag every other account's fetch rate
        // to 30s and re-open the 429 loop (v0.7.2 miss: one dev3 429 escalated
        // the global backoff, but the per-account rate against `/oauth/usage`
        // was still 120/hr per account because every wake fetched everyone).
        // With this floor, an account whose OWN tier is MIDDLE/RELAXED/BASE
        // skips the fetch until its tier-interval has elapsed since its last
        // successful fetch, regardless of how tight the global loop got.
        //
        // The `force` bypass keeps `usagio poll --force` semantics (a manual
        // refresh always fetches). The token refresh above already happened, so
        // the token stays warm even on skip.
        //
        // v0.8.5: the tier ramp + burn-rate projection only make sense for the
        // ACTIVE account — it's the only one burning usage, so it's the only one
        // whose reading can race toward the trigger. An inactive account's
        // reading only moves at its reset (which `cache_crossed_reset` already
        // lets through) or if it's used elsewhere, so it gets a flat
        // `INACTIVE_FETCH_FLOOR_SECS` — an inactive account at 99% used to land
        // on the 30s TIGHT tier and spend the tenant's request budget the
        // active account needs to fail over on time. A failed inactive fetch
        // additionally backs that account off on its own (see
        // `record_inactive_fetch_failure`) instead of slowing the global loop.
        if !force && !is_the_active_account {
            let tracker = get_usage_fetch_tracker(email);
            // A reset crossing overrides the backoff: the account may have just
            // become a swap target, and `evaluate_swap` won't pick it off a
            // stale locked reading.
            let crossed_reset = acct
                .cached_usage
                .as_ref()
                .is_some_and(|cu| cache_crossed_reset(cu, Utc::now()));
            if let (Some(until), false) = (tracker.backoff_until_ts, crossed_reset) {
                if now_ts < until {
                    logging::log(&format!(
                        "poll: {email} backing off after failed fetch; skipping \
                         (event=fetch_skip_backoff account={email} failures={} remaining={}s)",
                        tracker.consecutive_failures,
                        until - now_ts
                    ));
                    continue;
                }
            }
            if let Some(cu) = &acct.cached_usage {
                let age = now_ts - cu.fetched_at;
                let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
                if age >= 0 && age < floor && !cache_crossed_reset(cu, Utc::now()) {
                    logging::log(&format!(
                        "poll: {email} usage cache still fresh; skipping fetch \
                         (event=fetch_skip_fresh account={email} age={age}s floor={floor}s inactive)"
                    ));
                    continue;
                }
            }
        }
        if !force && is_the_active_account && defer_active {
            logging::log(&format!(
                "poll: {email} active fetch deferred (event=budget_defer account={email} \
                 why=per_minute)"
            ));
            continue;
        }
        if !force && is_the_active_account && !active_just_switched {
            if let Some(cu) = &acct.cached_usage {
                {
                    if let Some(why) =
                        v2_active_fetch_skip(CLAUDE_SLUG, email, cu, trigger_for_floor, now_ts)
                    {
                        logging::log(&format!(
                            "poll: {email} active fetch skipped (event=fetch_skip_active \
                             account={email} why={why})"
                        ));
                        continue;
                    }
                }
            }
        }
        if is_the_active_account {
            active_fetched = true;
        }
        let cu = match usage::fetch(&acct.access_token) {
            Ok(u) => {
                record_usage_fetch_success(email);
                let mut cu = cached_from_usage(&u);
                attach_prepaid_balance(&mut cu, &acct);
                if let Some(prev) = &acct.cached_usage {
                    log_epoch_ends(CLAUDE_SLUG, email, prev, &cu);
                }
                outcomes.insert(email.clone(), "ok");
                Some(cu)
            }
            Err(usage::FetchError::RateLimited) => {
                saw_429 = true;
                outcomes.insert(email.clone(), "429");
                record_fetch_attempt_failed(email, now_ts);
                // Only the ACTIVE account's 429 slows the global loop. An
                // inactive account's 429 backs off that account alone —
                // otherwise one idle (or dead) account pushes the loop to
                // 1200s and the active account hits its limit between polls
                // with no swap (v0.8.5: dev@ 429s froze dev6 at 90% for 20min
                // while it ran to 100%).
                if is_the_active_account {
                    rate_limited = true;
                } else {
                    record_inactive_fetch_failure(email, now_ts);
                }
                record_usage_fetch_429(email);
                let tracker = get_usage_fetch_tracker(email);
                logging::log(&format!(
                    "usage 429 for {email}; keeping cache \
                     (event=usage_429 account={email} consecutive={})",
                    tracker.consecutive_429s
                ));
                // A lapsed subscription 429s as often as it 403s — check.
                if let Some(p) = detect_lapsed_plan(provider, email, &acct.access_token, now_ts) {
                    persist_plan(CLAUDE_SLUG, email, &p, None);
                }
                None
            }
            Err(usage::FetchError::Forbidden) => {
                outcomes.insert(email.clone(), "err");
                record_fetch_attempt_failed(email, now_ts);
                record_usage_fetch_non_429(email);
                if !is_the_active_account {
                    record_inactive_fetch_failure(email, now_ts);
                }
                logging::log(&format!("usage 403 for {email}; keeping cache"));
                note_refresh_error(email, Some("usage request refused (HTTP 403)".to_string()));
                if let Some(p) = detect_lapsed_plan(provider, email, &acct.access_token, now_ts) {
                    persist_plan(CLAUDE_SLUG, email, &p, None);
                }
                None
            }
            Err(usage::FetchError::Auth) if is_active => {
                outcomes.insert(email.clone(), "err");
                record_fetch_attempt_failed(email, now_ts);
                // The active account's token was invalidated between our CAS
                // and this fetch (Claude Code rotated again in that narrow
                // window). Not ours to fix this cycle — skip and let the
                // next cycle's CAS pick up the new rotation.
                logging::log(&format!(
                    "active account token invalidated for {email}; waiting for Claude Code to rotate"
                ));
                continue;
            }
            Err(e) => {
                outcomes.insert(email.clone(), "err");
                record_fetch_attempt_failed(email, now_ts);
                // Any non-429, non-Auth error (network, DNS, JSON parse, …)
                // resets the escalation counter — flaky network isn't the
                // "server is asking us to back off" signal escalation exists
                // for (Sonnet adversarial review BAD 4).
                record_usage_fetch_non_429(email);
                if !is_the_active_account {
                    // A persistently failing inactive account (e.g. a 403 on
                    // a revoked org) would otherwise be re-fetched every
                    // cycle forever — its cache never refreshes, so the
                    // freshness floor never kicks in.
                    record_inactive_fetch_failure(email, now_ts);
                }
                logging::log(&format!("usage error for {email}: {e}; keeping cache"));
                note_refresh_error(email, Some(format!("usage fetch failed: {e}")));
                None
            }
        };
        // Persist a per-tick snapshot into the long-form history log, then run
        // notifications against the (prev, curr) pair. Best-effort throughout:
        // a full disk / permission failure never breaks the poll cycle.
        let mut new_notif_state: Option<notifications::NotifState> = None;
        if let Some(cu) = &cu {
            let account_key = usage_log::AccountKey::new(CLAUDE_SLUG, email.clone());
            let prev_snap = usage_log::last_snapshot(&account_key);
            let curr_snap = replay_row(CLAUDE_SLUG, email, cu, is_the_active_account, "ok");
            // One NotifState for the whole tick, carried across both the
            // threshold/pace path (needs a prev snapshot) and the reset-grant
            // path (edge-triggered purely through state, so it runs even on the
            // first poll). Persisted once at the end if anything changed.
            let mut ns = acct.notif_state.clone();
            for g in &cu.reported.reset_grants {
                for trig in &notifications::reset_grant_triggers(&mut ns, std::slice::from_ref(g)) {
                    if let Err(e) = notifications::fire(trig, &account_key) {
                        logging::log(&format!("notify: {email}: {e:#}"));
                        // Not delivered: leave the grant un-announced so the
                        // next poll retries instead of losing it for good.
                        notifications::release_dedup(&mut ns, trig, Some(&g.id));
                    }
                }
            }
            if let Some(prev) = prev_snap {
                let raw = notifications::evaluate(&prev, &curr_snap, &notif_cfg);
                let mut kept = notifications::dedup_and_apply(&mut ns, &prev, &curr_snap, raw);
                // Pace check (default off) — feeds through the same dedup path.
                // Read pace (historical slope) BEFORE the append below: appending
                // curr_snap advances the current-month file's mtime and would
                // otherwise invalidate the usage-log cache mid-tick, forcing pace
                // (and the next account's last_snapshot) to re-parse the whole
                // month. curr_snap is passed to evaluate_pace explicitly, so the
                // current point is still considered; only the historical read is
                // reordered (v0.5.17 PERF).
                let pace = usage_log::pace(&account_key);
                if let Some(pt) =
                    notifications::evaluate_pace(pace.as_ref(), &curr_snap, &notif_cfg, Utc::now())
                {
                    let extra =
                        notifications::dedup_and_apply(&mut ns, &prev, &curr_snap, vec![pt]);
                    kept.extend(extra);
                }
                for trig in &kept {
                    if let Err(e) = notifications::fire(trig, &account_key) {
                        logging::log(&format!("notify: {email}: {e:#}"));
                        notifications::release_dedup(&mut ns, trig, None);
                    }
                }
            }
            // Persist the tick's NotifState if either path changed it (threshold
            // crossings, pace, or newly-announced reset grants).
            if ns != acct.notif_state {
                new_notif_state = Some(ns);
            }
            // Append LAST: all reads for this account (last_snapshot above, pace
            // in the prev block) have completed against the warm cache.
            if let Err(e) = usage_log::append(&curr_snap) {
                logging::log(&format!("usage_log: append failed for {email}: {e:#}"));
            }
        }
        updates.push((email.clone(), acct, cu, new_notif_state));
    }

    // One replay row per account per cycle (§6): the 429/err/skip ones repeat
    // the cached reading with its own `fetched_at`.
    for email in &emails {
        let outcome = outcomes.get(email).copied().unwrap_or("skip");
        if outcome == "ok" {
            continue;
        }
        if let Some(cu) = state.find(email).and_then(|a| a.cached_usage.as_ref()) {
            let is_active = state.active.as_deref() == Some(email.as_str());
            if outcome == "skip" && !skip_row_due(CLAUDE_SLUG, email, is_active, now_ts) {
                continue;
            }
            let row = replay_row(CLAUDE_SLUG, email, cu, is_active, outcome);
            if let Err(e) = usage_log::append(&row) {
                logging::log(&format!("usage_log: append failed for {email}: {e:#}"));
            }
        }
    }

    // Merge under the lock with a fresh reload so we don't clobber a switch.
    let merged = with_state_lock(|| {
        let mut st = State::load()?;
        let before = serde_json::to_vec(&st).unwrap_or_default();
        sync_active_claude_identity_from(&mut st, claude_identity.as_ref());
        for (email, acct, cu, ns) in &updates {
            if let Some(a) = st.find_mut(email) {
                merge_poll_update(a, acct, cu.as_ref(), ns.as_ref());
            }
        }
        save_if_changed(&st, &before)?;
        Ok(())
    });
    if let Err(e) = merged {
        logging::log(&format!("poll: saving refreshed cache failed: {e:#}"));
    }
    logging::log("poll: done");
    let fetched = outcomes.values().filter(|o| **o == "ok").count();
    RefreshOutcome {
        rate_limited,
        active_fetched,
        saw_429,
        fetched,
        failed: outcomes.len() - fetched + refresh_failed,
    }
}

/// Fold one poll update (computed from a phase-1 snapshot, `snap`) into the
/// freshly reloaded account `a`. Tokens are already saved (rotations by
/// `oauth::ensure_fresh`, active-slot adoptions by the CAS). Everything here
/// guards against reverting what a concurrent writer saved since the snapshot.
pub(crate) fn merge_poll_update(
    a: &mut Account,
    snap: &Account,
    cu: Option<&CachedUsage>,
    ns: Option<&notifications::NotifState>,
) {
    // Only ever OR the flag in (a plain overwrite could clear a concurrent
    // `flag_needs_relogin`), and only when the stored refresh token is still the
    // one that was rejected: a renewal that landed mid-poll must not be
    // re-flagged by a failure of the superseded token.
    if a.refresh_token == snap.refresh_token {
        a.needs_relogin = a.needs_relogin || snap.needs_relogin;
    }
    // An older poll result never replaces a newer reading saved meanwhile.
    if let Some(cu) = cu {
        if a.cached_usage.as_ref().is_none_or(|stored| {
            // A stored stamp from the future (clock was ahead) never pins.
            stored.fetched_at <= cu.fetched_at
                || stored.fetched_at > Utc::now().timestamp() + autoswap::memory::FUTURE_SLACK_SECS
        }) {
            a.cached_usage = Some(cu.clone());
        }
    }
    // The poll never touches `login_expiry_warned`; keep whatever is stored.
    if let Some(ns) = ns {
        let warned = a.notif_state.login_expiry_warned;
        a.notif_state = ns.clone();
        a.notif_state.login_expiry_warned = warned;
    }
}

/// Whether a `skip` replay row (a cycle that didn't fetch the account) is due:
/// at most one per 60 s for the active account and one per hour for an
/// inactive one, whose cached reading can't change between its own fetches.
/// 429/err rows are always written.
pub(crate) fn skip_row_due(slug: &str, key: &str, active: bool, now_ts: i64) -> bool {
    type Last = std::collections::HashMap<String, i64>;
    static LAST: std::sync::Mutex<Option<Last>> = std::sync::Mutex::new(None);
    let every = if active { 60 } else { 3600 };
    let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(Default::default);
    let k = format!("{slug}/{key}/{active}");
    if map.get(&k).is_some_and(|t| now_ts - t < every) {
        return false;
    }
    map.insert(k, now_ts);
    true
}

/// A decisions-log line for a manual switch (§6: every switch, including
/// manual ones).
pub(crate) fn log_manual_switch(slug: &str, from: Option<&str>, to: &str) {
    if from.is_some_and(|f| f.eq_ignore_ascii_case(to)) {
        return;
    }
    let v = serde_json::json!({
        "ts": Utc::now().timestamp(),
        "provider": slug,
        "engine": "manual",
        "action": "switching",
        "reason": "manual",
        "from": from,
        "target": to,
    });
    if let Err(e) = usage_log::append_decision(&v) {
        logging::log(&format!("decisions log append failed: {e:#}"));
    }
}

/// One usage-log row for `key` (§6 replay fields).
pub(crate) fn replay_row(
    slug: &str,
    key: &str,
    cu: &CachedUsage,
    active: bool,
    outcome: &str,
) -> usage_log::Snapshot {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
    };
    usage_log::Snapshot {
        ts: Utc::now(),
        provider: slug.to_string(),
        account: key.to_string(),
        session_pct: cu.session_pct.map(|p| p as f32),
        weekly_pct: cu.weekly_pct.map(|p| p as f32),
        active_model: None,
        active: Some(active),
        session_reset: parse(&cu.session_reset),
        weekly_reset: parse(&cu.weekly_reset),
        fetched_at: Some(cu.fetched_at),
        outcome: Some(outcome.to_string()),
    }
}

/// The active account's fetch floor under v2 (§5): the tier ramp with a 60 s
/// TIGHT floor (no burn-rate projection — the blind projection covers that),
/// the provider's own backoff after a failed attempt, and never while the
/// active account is lb-blocked (R3-B2) — it is read again only once that
/// window is reset-pending, at the 12 h cap, or on a forced refresh. Returns
/// why it skips, or `None` to fetch.
pub(crate) fn v2_active_fetch_skip(
    slug: &str,
    key: &str,
    cu: &CachedUsage,
    trigger: f64,
    now_ts: i64,
) -> Option<&'static str> {
    let now = Utc::now();
    let age = now_ts - cu.fetched_at;
    if active_cache_lb_blocked(cu, now) && age < MAX_LOCKED_STALENESS_SECS {
        return Some("lb_blocked");
    }
    if cache_crossed_reset(cu, now) {
        return None;
    }
    let max_pct = match (cu.session_pct, cu.weekly_pct) {
        (Some(s), Some(w)) => Some(s.max(w)),
        (s, w) => s.or(w),
    };
    let floor = v2_active_floor_secs(max_pct, trigger, WATCH_INTERVAL_SECS) as i64;
    if age >= 0 && age < floor {
        return Some("fresh");
    }
    if active_fetch_in_backoff(slug, key, now_ts) {
        return Some("backoff");
    }
    None
}

/// The cached reading has a window at the limit that hasn't reset yet (a
/// lower bound, valid at any age): an lb-blocked account.
pub(crate) fn active_cache_lb_blocked(cu: &CachedUsage, now: DateTime<Utc>) -> bool {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
    };
    [
        (cu.session_pct, parse(&cu.session_reset)),
        (cu.weekly_pct, parse(&cu.weekly_reset)),
    ]
    .into_iter()
    .any(|(p, r)| {
        p.is_some_and(|p| p >= autoswap::policy::BLOCKED_PCT) && r.is_none_or(|r| r > now)
    })
}

/// v2's active fetch floor: TIGHT (in band) is 60 s, not 30 s.
pub(crate) fn v2_active_floor_secs(max_pct: Option<f64>, trigger: f64, base: u64) -> u64 {
    let Some(pct) = max_pct else { return base };
    if pct >= trigger - WATCH_TIGHT_BAND {
        autoswap::policy::ACTIVE_TIGHT_FLOOR_S
    } else {
        tier_interval_for_pct(pct, trigger, base)
    }
}

/// A reset boundary invalidates even a recently fetched sample. Let that
/// account bypass its fetch floor so the boundary wake can confirm recovery.
pub(crate) fn cache_crossed_reset(cu: &CachedUsage, now: DateTime<Utc>) -> bool {
    [cu.session_reset.as_deref(), cu.weekly_reset.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
        .any(|reset| cu.fetched_at < reset.timestamp() && reset <= now)
}
