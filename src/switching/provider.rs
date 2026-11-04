//! Provider-generic switching, including the queued (pending) switch protocol.

use anyhow::{anyhow, bail, Context, Result};

use crate::poll::claude::log_manual_switch;
use crate::poll::trackers::{fetch_tracker_key, reset_usage_fetch_tracker};
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::state_ops::{provider_active, with_state_lock};
use crate::store::State;
use crate::switching::claude::switch_to_if_still_active;
use crate::switching::manual_pick::pick_tier;
use crate::{autoswap, credentials, lifecycle, logging, providers, store};

/// Make `key` the active login for the non-Claude provider `slug` (state
/// v2's `State::providers[slug]`). Mirrors `switch_to`'s contract for Claude:
/// rewrites the vendor's on-disk credential file via
/// `Provider::write_active_account`, then commits the new `active` selection
/// to state.json under the state lock. Returns the account key as the
/// display label (non-Claude providers have no separate display-name
/// resolution step the way Claude's identity backfill does). `hold` marks a
/// manual choice auto-swap must respect; `false` clears any earlier hold.
pub(crate) fn switch_to_provider_account(slug: &str, key: &str, hold: bool) -> Result<String> {
    let key = switch_provider_with_intent(slug, key, hold)?;
    Ok(provider_account_label(slug, &key))
}

/// The human label for `slug`'s account `key` (see
/// `trait_def::account_label`): its email, with a workspace tag only when
/// another captured account shares that email.
pub(crate) fn provider_account_label(slug: &str, key: &str) -> String {
    let siblings: Vec<String> = State::load()
        .ok()
        .and_then(|st| st.provider_accounts(slug).cloned())
        .map(|pa| pa.accounts.into_iter().map(|a| a.key).collect())
        .unwrap_or_default();
    providers::trait_def::account_label(key, siblings.iter().map(String::as_str))
}

pub(crate) fn switch_provider_with_intent(slug: &str, key: &str, manual: bool) -> Result<String> {
    switch_provider_expecting(slug, key, manual, None)
}

/// Whether `slug`'s queued switch is still exactly `expected`.
pub(crate) fn pending_switch_is(
    state: &State,
    slug: &str,
    expected: &store::PendingSwitch,
) -> bool {
    state
        .provider_accounts(slug)
        .and_then(|pa| pa.pending_switch.as_ref())
        == Some(expected)
}

/// `switch_provider_with_intent`, optionally as a retry of the queued switch
/// `expected` (compare-and-set): if the queue moved on meanwhile (a newer
/// explicit choice, or it already completed) do nothing rather than replay a
/// stale intent over it.
pub(crate) fn switch_provider_expecting(
    slug: &str,
    key: &str,
    manual: bool,
    expected: Option<&store::PendingSwitch>,
) -> Result<String> {
    let provider = provider_by_slug(slug)?;
    if !provider.capabilities().supports_switching {
        bail!("switching is not supported for {slug}");
    }
    if let Some(exp) = expected {
        if !pending_switch_is(&State::load()?, slug, exp) {
            return Ok(key.to_owned());
        }
    }
    let renewal_pending = State::load()?
        .provider_accounts(slug)
        .and_then(|accounts| accounts.pending_switch.as_ref())
        .is_some_and(|pending| pending.key == key && pending.renewed_account.is_some());
    if !renewal_pending {
        lifecycle::ensure_provider_fresh(provider, key)?;
    }
    let _refresh = lifecycle::refresh_lock()?;
    let status = provider.client_status()?;
    let ready = with_state_lock(|| {
        let mut state = State::load()?;
        // Authoritative compare-and-set: same state lock as the queue write.
        if expected.is_some_and(|exp| !pending_switch_is(&state, slug, exp)) {
            return Ok(false);
        }
        lifecycle::prepare_handoff(provider, &status, &mut state, key, manual)
    })?;
    if !ready {
        return Ok(key.to_owned());
    }
    commit_provider_switch(provider, slug, key, manual, manual)
}

/// Whether a switch to `key` is still queued; a state read failure is an error
/// (not "no longer pending").
pub(crate) fn try_provider_switch_pending(slug: &str, key: &str) -> Result<bool> {
    Ok(queued_provider_switch(slug, key)?.is_some())
}

/// `slug`'s queued switch, if it is the one to `key`.
pub(crate) fn queued_provider_switch(
    slug: &str,
    key: &str,
) -> Result<Option<store::PendingSwitch>> {
    let state = State::load()?;
    Ok(state
        .provider_accounts(slug)
        .and_then(|pa| pa.pending_switch.clone())
        .filter(|pending| pending.key.eq_ignore_ascii_case(key)))
}

pub(crate) fn provider_switch_pending(slug: &str, key: &str) -> bool {
    try_provider_switch_pending(slug, key).unwrap_or(false)
}

pub(crate) fn complete_pending_switches() {
    let Ok(state) = State::load() else { return };
    for (slug, accounts) in &state.providers {
        let Some(pending) = &accounts.pending_switch else {
            continue;
        };
        if accounts.find(&pending.key).is_none() || accounts.active != pending.from {
            if let Ok(provider) = provider_by_slug(slug) {
                if let Err(error) = recover_obsolete_pending_switch(provider, pending) {
                    logging::log(&format!(
                        "event=handoff_recovery_pending provider={slug} reason={error}"
                    ));
                }
            }
            continue;
        }
        if let Err(error) =
            switch_provider_expecting(slug, &pending.key, pending.manual, Some(pending))
        {
            logging::log(&format!(
                "event=pending_switch_retry provider={slug} reason={error}"
            ));
        }
    }
}

/// A removal cancels the account choice, not the responsibility to restart a
/// daemon already told to drain. Never recreate a removed saved account.
pub(crate) fn recover_obsolete_pending_switch(
    provider: &dyn Provider,
    expected: &store::PendingSwitch,
) -> Result<()> {
    let slug = provider.provider_id();
    let restart = with_state_lock(|| {
        let mut state = State::load()?;
        let accounts = state.provider_accounts_mut(slug);
        if accounts.pending_switch.as_ref() != Some(expected) {
            return Ok(None);
        }
        if let Some(active) = accounts
            .active
            .clone()
            .filter(|key| accounts.find(key).is_some())
        {
            if let Some(renewed) = &expected.renewed_account {
                if renewed.key != active {
                    if let Some(saved) = accounts
                        .accounts
                        .iter_mut()
                        .find(|account| account.key == renewed.key)
                    {
                        *saved = renewed.clone();
                    }
                }
            }
            let pending = accounts.pending_switch.as_mut().unwrap();
            pending.key = active.clone();
            pending.from = Some(active);
            pending.manual = false;
            pending.renewal = false;
            pending.credentials_written = false;
            pending.leave_latch = None;
            pending.renewed_account = None;
            state.save()?;
            return Ok(None);
        }
        if let Some(token) = &expected.drain_token {
            return Ok(Some(token.clone()));
        }
        accounts.pending_switch = None;
        state.save()?;
        Ok(None)
    })?;
    if let Some(token) = restart {
        let _refresh = lifecycle::refresh_lock()?;
        provider.verify_handoff_stopped(&token)?;
        let identity = provider
            .read_active_identity()?
            .context("cannot restore the drained client's live login")?;
        provider.handoff_client(&identity, Some(&token))?;
        with_state_lock(|| {
            let mut state = State::load()?;
            let accounts = state.provider_accounts_mut(slug);
            if accounts.pending_switch.as_ref() == Some(expected) {
                accounts.pending_switch = None;
                state.save()?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(crate) fn commit_provider_switch(
    provider: &dyn Provider,
    slug: &str,
    key: &str,
    hold: bool,
    manual: bool,
) -> Result<String> {
    commit_provider_switch_guarded(provider, slug, key, hold, manual, None)?
        .ok_or_else(|| anyhow!("switch intent changed before commit"))
}

/// `commit_provider_switch` with an optional auto-swap guard: when
/// `expect_active` is set, the CAS precondition (that account still active, and
/// no switch queued for a DIFFERENT key, e.g. a renewal staged meanwhile) is
/// re-checked in the first locked phase; `Ok(None)` if it no longer holds.
pub(crate) fn commit_provider_switch_guarded(
    provider: &dyn Provider,
    slug: &str,
    key: &str,
    hold: bool,
    manual: bool,
    expect_active: Option<&str>,
) -> Result<Option<String>> {
    let Some((acct, identity, drain_token)) = with_state_lock(|| {
        // Absorb any lagging on-disk rotation for the OUTGOING account BEFORE we
        // overwrite the vendor's credential file — the same guard Claude's
        // `switch_to_guarded` applies (main.rs, `absorb_before_switch`). Without
        // it, a single-use refresh token the vendor CLI rotated since our last
        // poll is left at its stale (now-dead) value, and a later switch-BACK
        // writes that dead token → forced interactive login, defeating the
        // "capture once, never re-login" guarantee. absorb rewrites state.json
        // under the reentrant lock, so load AFTER it to pick up the rotation.
        credentials::absorb_before_switch(provider);
        let mut state = State::load()?;
        if let Some(expect) = expect_active {
            let pa = state.provider_accounts(slug);
            let still_active = pa
                .and_then(|pa| pa.active.as_deref())
                .is_some_and(|c| c.eq_ignore_ascii_case(expect));
            let other_queued = pa
                .and_then(|pa| pa.pending_switch.as_ref())
                .is_some_and(|p| !p.key.eq_ignore_ascii_case(key));
            if !still_active || other_queued {
                return Ok(None);
            }
        }
        let renewed_account = state
            .provider_accounts(slug)
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .filter(|pending| pending.key == key)
            .and_then(|pending| pending.renewed_account.clone());
        let acct = renewed_account
            .clone()
            .or_else(|| state.find_provider_account(slug, key).cloned())
            .ok_or_else(|| anyhow!("no {slug} account matches '{key}'"))?;
        lifecycle::validate_stored_identity(provider, &acct)?;
        let identity = acct.identity_snapshot();
        let drain_token = state
            .provider_accounts(slug)
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .and_then(|pending| pending.drain_token.clone());
        let credentials_written = state
            .provider_accounts(slug)
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .is_some_and(|pending| pending.key == key && pending.credentials_written)
            && lifecycle::matching_active_blob(provider, &identity)?.is_some();
        let pending_renewal = state
            .provider_accounts(slug)
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .is_some_and(|pending| pending.key == key && pending.renewal);
        let from = state.provider_accounts_mut(slug).active.clone();
        let leave_latch = state
            .provider_accounts(slug)
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .and_then(|pending| pending.leave_latch.clone());
        state.provider_accounts_mut(slug).pending_switch = Some(store::PendingSwitch {
            key: acct.key.clone(),
            manual,
            from,
            drain_token: drain_token.clone(),
            credentials_written,
            leave_latch,
            renewed_account,
            renewal: pending_renewal && !manual,
        });
        state.save()?;
        if !credentials_written {
            provider
                .write_active_account(&acct.secret_blob, &identity)
                .map_err(|e| anyhow!("switching {slug} account: {e}"))?;
            state.upsert_provider_account(slug, acct.clone());
            state
                .provider_accounts_mut(slug)
                .pending_switch
                .as_mut()
                .unwrap()
                .credentials_written = true;
            state
                .provider_accounts_mut(slug)
                .pending_switch
                .as_mut()
                .unwrap()
                .renewed_account = None;
            state.save()?;
        }
        Ok(Some((acct, identity, drain_token)))
    })?
    else {
        return Ok(None);
    };
    // Starting a daemon can take seconds. Keep state reads, usage persistence,
    // and account management responsive while the refresh lock owns this switch.
    provider
        .handoff_client(&identity, drain_token.as_deref())
        .map_err(|error| anyhow!("credentials saved; client handoff will retry: {error}"))?;
    with_state_lock(|| {
        let mut state = State::load()?;
        let accounts = state.provider_accounts_mut(slug);
        let pending = accounts
            .pending_switch
            .as_ref()
            .ok_or_else(|| anyhow!("switch intent changed during client handoff"))?;
        anyhow::ensure!(
            pending.key == key
                && pending.credentials_written
                && pending.drain_token == drain_token
                && accounts.active == pending.from,
            "switch intent changed during client handoff"
        );
        anyhow::ensure!(
            accounts.find(key).is_some(),
            "account removed during client handoff"
        );
        let leave_latch = pending.leave_latch.clone();
        let renewal = pending.renewal && !manual;
        accounts.pending_switch = None;
        let prev = state
            .provider_accounts_mut(slug)
            .active
            .replace(acct.key.clone());
        // A login renewal (explicit `renewal` marker staged by `persist_login`)
        // must not touch the user's pick, hold, or the swap memory.
        let renewal = renewal
            && prev
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(&acct.key));
        if !renewal {
            state.set_manual_pick(slug, hold.then_some(acct.key.as_str()));
            let tier = pick_tier(&state, slug, &acct.key);
            state.set_manual_pick_tier(slug, hold.then_some(tier.as_str()));
            let hold = hold && tier >= autoswap::Tier::Draining;
            state.set_manual_hold(slug, hold.then_some(acct.key.as_str()));
            state.note_active_change(
                slug,
                prev.as_deref(),
                &acct.key,
                if manual {
                    autoswap::memory::ChangeKind::Manual
                } else {
                    autoswap::memory::ChangeKind::Auto
                },
            );
        }
        if let (Some(from), Some((kind, latch))) = (prev.as_deref(), leave_latch) {
            state
                .swap_memory
                .entry(slug.to_string())
                .or_default()
                .apply_latch(from, kind, latch);
        }
        // The vendor credential file is already switched at this point; if
        // recording it in state.json fails, say so explicitly rather than
        // surfacing a bare io error that reads as "the whole switch failed"
        // (matches the Claude sibling path's context).
        state.save().context(
            "the login was switched but recording it in state.json failed; \
             run `usagio switch` again to reconcile the bookkeeping",
        )?;
        // Same as Claude's `switch_to_guarded`: the account we left must not
        // re-escalate later on a stale 429 count.
        if manual {
            log_manual_switch(slug, prev.as_deref(), &acct.key);
        }
        if let Some(prev) = prev.filter(|p| !p.eq_ignore_ascii_case(&acct.key)) {
            reset_usage_fetch_tracker(&fetch_tracker_key(slug, &prev));
        }
        Ok(Some(acct.key))
    })
}

/// Resolve a provider by slug, mapping "not registered" to a clear error.
pub(crate) fn provider_by_slug(slug: &str) -> Result<&'static dyn Provider> {
    providers::get(slug)
        .with_context(|| format!("provider '{slug}' is not registered in this build"))
}

/// Auto-swap `slug` to `target`, but only if `expect_active` is still its
/// active account (a manual switch since the decision was made wins).
/// `Ok(None)` when the compare-and-set lost.
pub(crate) fn switch_if_still_active(
    slug: &str,
    target: &str,
    expect_active: &str,
) -> Result<Option<String>> {
    if slug == CLAUDE_SLUG {
        return switch_to_if_still_active(provider_by_slug(slug)?, target, expect_active);
    }
    let provider = provider_by_slug(slug)?;
    lifecycle::ensure_provider_fresh(provider, target)?;
    let _refresh = lifecycle::refresh_lock()?;
    let status = provider.client_status()?;
    switch_provider_if_still_active(provider, &status, target, expect_active)
}

/// Compare-and-set + handoff preparation under the state lock; the commit (which
/// runs the seconds-long client handoff) happens AFTER the lock is released so
/// `commit_provider_switch` can keep state reads responsive during the handoff.
pub(crate) fn switch_provider_if_still_active(
    provider: &dyn Provider,
    status: &providers::ClientStatus,
    target: &str,
    expect_active: &str,
) -> Result<Option<String>> {
    let slug = provider.provider_id();
    let ready = with_state_lock(|| {
        let mut state = State::load()?;
        if state
            .provider_accounts(slug)
            .is_some_and(|pa| pa.pending_switch.is_some())
        {
            return Ok(false);
        }
        let current = provider_active(&state, slug);
        if !current
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(expect_active))
            || autoswap::shell::pick_diverged_from_armed(slug, &state)
        {
            return Ok(false);
        }
        lifecycle::prepare_handoff(provider, status, &mut state, target, false)
    })?;
    if !ready {
        return Ok(None);
    }
    commit_provider_switch_guarded(provider, slug, target, false, false, Some(expect_active))
}

/// `switch_if_still_active`, writing the engine's leave latch for the account
/// left in the switch's own locked section (consumed by
/// `State::note_active_change`).
pub(crate) fn switch_if_still_active_latched(
    slug: &str,
    target: &str,
    expect_active: &str,
    latch: Option<(autoswap::memory::LatchKind, autoswap::memory::Latch)>,
) -> Result<Option<String>> {
    let _armed = autoswap::shell::arm_leave_latch(slug, expect_active, latch);
    switch_if_still_active(slug, target, expect_active)
}

pub(crate) fn provider_supports_swap(slug: &str) -> bool {
    match providers::get(slug) {
        Some(p) => {
            let c = p.capabilities();
            c.supports_usage && c.supports_switching
        }
        None => false,
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
