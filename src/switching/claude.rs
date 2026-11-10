//! Claude account switching: token freshness, refresh-token claims, and writing the shared login.

use anyhow::{anyhow, bail, Context, Result};

use crate::claude_local::{keychain_write, read_claude_json_raw, write_claude_identity};
use crate::lifecycle::{active_refresh_cas, ActiveRefreshOutcome};
use crate::poll::claude::log_manual_switch;
use crate::poll::trackers::reset_usage_fetch_tracker;
use crate::providers::claude::{oauth, usage};
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::state_ops::with_state_lock;
use crate::store::{Account, State};
use crate::switching::identity::sync_active_from_keychain;
use crate::switching::manual_pick::pick_tier;
use crate::switching::provider::provider_by_slug;
use crate::{autoswap, credentials, logging, providers};

/// Refresh a token if it expires within this many seconds. Sourced from the
/// credential-sync module so the reactive (switch / cmd_token / poll) path
/// and the proactive (fsnotify + inactive-refresh) path can never drift.
pub(crate) const REFRESH_SKEW_SECS: i64 = credentials::REFRESH_SKEW_SECS;

/// Make `email` the active login. Does all network work (token refresh, identity
/// backfill) OUTSIDE the state lock, then commits keychain + ~/.claude.json +
/// state under the lock with a fresh reload. Returns the display label.
/// `hold` marks a manual choice auto-swap must respect; `false` clears any
/// earlier hold.
pub(crate) fn switch_to(email: &str, hold: bool) -> Result<String> {
    // v1 state is Claude-only; look up the "claude" provider now so later
    // phases can key the switch on the account's containing bucket instead.
    let provider = provider_by_slug(CLAUDE_SLUG)?;
    let (acct, identity, backfilled) = prepare_switch(provider, email)?;
    let label = acct.email.clone().unwrap_or_else(|| email.to_string());
    Ok(
        switch_to_guarded(provider, email, &acct, &identity, backfilled, None, hold)?
            .unwrap_or(label),
    )
}

/// Like `switch_to`, but only if `expect_active` is still the active account
/// when the lock is taken (a compare-and-set). Used by the auto-swap daemon so a
/// concurrent manual switch isn't overridden by a stale decision. Returns the
/// label on a real switch, or `None` if it was skipped.
pub(crate) fn switch_to_if_still_active(
    provider: &'static dyn Provider,
    email: &str,
    expect_active: &str,
) -> Result<Option<String>> {
    let (acct, identity, backfilled) = prepare_switch(provider, email)?;
    switch_to_guarded(
        provider,
        email,
        &acct,
        &identity,
        backfilled,
        Some(expect_active),
        false,
    )
}

/// Phase 1 of a switch (no lock): refresh the token and resolve the identity
/// over the network, returning what the locked commit phase needs.
fn prepare_switch(
    provider: &'static dyn Provider,
    email: &str,
) -> Result<(Account, serde_json::Value, bool)> {
    let state = State::load()?;
    let mut acct = state
        .find(email)
        .cloned()
        .with_context(|| format!("no account matches '{email}'"))?;
    if state.active.as_deref() == Some(email) {
        // GUARANTEE: usagio must NEVER POST /token for the ACTIVE account.
        // Anthropic/OpenAI refresh tokens are single-use — a POST rotates the
        // whole family server-side and invalidates the copy the live vendor CLI
        // holds, forcing a `/login`. Switching TO the already-active account
        // (e.g. `usagio switch <active>` to relaunch, or clicking the active
        // row) must therefore ADOPT the vendor slot's current token, exactly
        // like the poll loop's active path — never mint one. See
        // `active_refresh_cas`'s doc. The CAS may write the slot, so it runs
        // under the state lock.
        let outcome = with_state_lock(|lock| Ok(active_refresh_cas(lock, provider, &mut acct)))?;
        if matches!(outcome, ActiveRefreshOutcome::RefreshFailed) {
            logging::log(&format!(
                "switch: active account {email} slot unreadable; keeping stored \
                 token (never POST /token for the active account)"
            ));
        }
    } else {
        // Inactive account: usagio owns its token, so a reactive refresh is
        // safe. On a hard failure (single-use token rotated elsewhere / revoked)
        // bail with a distinctive error so the menu / CLI can prompt a fresh
        // login instead of writing a stale token that `claude` will reject.
        match ensure_fresh_with_fallback(provider, email, &mut acct) {
            Ok(_) => {}
            Err(oauth::RefreshError::InvalidGrant) => {
                flag_needs_relogin(email, &acct.refresh_token);
                bail!(
                    "account {email} needs re-login (refresh token rejected); run \
                     `claude /login` for it or click the account row in the menu"
                );
            }
            Err(e) => bail!("token refresh for {email} failed: {e}"),
        }
    }
    let (identity, backfilled) = resolve_identity(provider, &acct)?;
    Ok((acct, identity, backfilled))
}

/// `ensure_fresh` with a disk-fallback safety net for the reactive paths
/// (`cmd_token`, `prepare_switch`). If the initial refresh fails with
/// `InvalidGrant`, the stored refresh token may just be behind a rotation the
/// vendor CLI (or another usagio process) already wrote to disk — or fsnotify
/// may have missed the FSEvent (common after a suspend/resume). Rescan every
/// registered credential path via `credentials::last_chance_fallback`; if a
/// usable blob for this account is sitting on disk, adopt it into state.json
/// and retry the refresh once with the fresh grant. This matches the policy
/// `refresh_inactive_if_stale` already uses on the proactive path so all three
/// entry points agree before we surface "needs re-login" to the user.
pub(crate) fn ensure_fresh_with_fallback(
    provider: &'static dyn Provider,
    email: &str,
    acct: &mut Account,
) -> std::result::Result<bool, oauth::RefreshError> {
    match oauth::ensure_fresh(acct, REFRESH_SKEW_SECS) {
        Ok(b) => Ok(b),
        Err(oauth::RefreshError::InvalidGrant) => {
            let key = crate::providers::trait_def::AccountKey::new(provider.provider_id(), email);
            if credentials::last_chance_fallback(provider, &key) {
                // Reload the account with the tokens the fallback just
                // adopted, then retry the refresh once. If the reload fails
                // or the account vanished, fall through to InvalidGrant.
                if let Ok(st) = State::load() {
                    if let Some(fresh) = st.find(email).cloned() {
                        *acct = fresh;
                        return oauth::ensure_fresh(acct, REFRESH_SKEW_SECS);
                    }
                }
            }
            Err(oauth::RefreshError::InvalidGrant)
        }
        Err(e) => Err(e),
    }
}

/// Get a usable bearer for a reset claim. The ACTIVE account's token belongs
/// to Claude Code: adopt the slot's current one (never POST /token, like
/// `cmd_token`/`prepare_switch`); any other account refreshes normally.
fn claim_refresh_token(
    provider: &'static dyn Provider,
    email: &str,
    acct: &mut Account,
    is_active: bool,
) -> std::result::Result<(), String> {
    if is_active {
        // `active_refresh_cas` requires the state lock (it may write the slot).
        let cas = with_state_lock(|lock| {
            let mut st = State::load()?;
            let outcome = st
                .find_mut(email)
                .map(|a| (active_refresh_cas(lock, provider, a), a.clone()));
            st.save()?;
            Ok(outcome)
        });
        match cas {
            Ok(Some((ActiveRefreshOutcome::Adopted, fresh))) => *acct = fresh,
            Ok(Some((ActiveRefreshOutcome::RefreshFailed, _))) | Ok(None) => {
                logging::log(&format!(
                    "claim: active account {email} slot unreadable; using stored token"
                ));
            }
            Err(e) => {
                // Degrade, don't fail: the stored token is what this path
                // always used before the locked CAS existed.
                logging::log(&format!(
                    "claim: reading the active login for {email} failed ({e:#}); using stored token"
                ));
            }
        }
        return Ok(());
    }
    match ensure_fresh_with_fallback(provider, email, acct) {
        Ok(_) => Ok(()),
        Err(oauth::RefreshError::InvalidGrant) => {
            flag_needs_relogin(email, &acct.refresh_token);
            Err(format!(
                "{email} needs re-login before a reset can be claimed"
            ))
        }
        Err(e) => Err(format!("token refresh for {email} failed: {e}")),
    }
}

/// Claim a banked usage-limit reset for the Claude account keyed by `email`.
/// Refreshes the token (a claim must authenticate), reads the org uuid from the
/// profile and the grant id from the cached usage the menu was built from, then
/// POSTs the claim. Returns the server's [`ClaimOutcome`] or a user-facing
/// error string. SPENDS a one-shot reset — only ever called from the explicit
/// "Claim reset…" menu action.
pub(crate) fn claim_claude_reset(email: &str) -> std::result::Result<usage::ClaimOutcome, String> {
    let provider = providers::get(CLAUDE_SLUG).ok_or("claude provider unavailable")?;
    let mut acct = State::load()
        .map_err(|e| format!("loading state: {e:#}"))?
        .find(email)
        .cloned()
        .ok_or_else(|| format!("account {email} not found"))?;
    // Pick the grant from the cache the menu row was built from — the one the
    // user saw as claimable. First ready grant wins (see `ResetGrant::is_ready`).
    let grant_id = acct
        .cached_usage
        .as_ref()
        .map(|c| &c.reported.reset_grants)
        .into_iter()
        .flatten()
        .find(|g| g.is_ready())
        .map(|g| g.id.clone())
        .ok_or("no claimable reset on this account")?;
    let is_active = State::load().ok().is_some_and(|st| {
        st.active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(email))
    });
    claim_refresh_token(provider, email, &mut acct, is_active)?;
    let profile = usage::fetch_profile(&acct.access_token)
        .ok_or("could not read account profile (for organization id)")?;
    let org =
        usage::org_uuid_from_profile(&profile).ok_or("account profile has no organization id")?;
    usage::claim_reset(&acct.access_token, &org, &grant_id).map_err(|e| e.to_string())
}

/// Best-effort: mark an account `needs_relogin=true` in state.json, but only
/// while it still holds `rejected_refresh_token` (the grant `ensure_fresh`
/// reports it sent): a rotation or renewal saved meanwhile is not flagged. If
/// the state lock is held or the save fails, the next `refresh_usage_cache`
/// tick re-observes the invalid_grant and sets it again — callers that need
/// certainty already surface a distinct error to the user.
pub(crate) fn flag_needs_relogin(email: &str, rejected_refresh_token: &str) {
    // L7 (round-1 codeaudit): the previous `let _ =` swallowed save errors
    // silently, so a persistent state.json write failure would never be
    // visible in the log. The next `refresh_usage_cache` tick re-sets the
    // flag from the invalid_grant observation, so we still make progress,
    // but the failure should be noisy.
    logging::log(&format!(
        "event=needs_relogin account={email} reason=invalid_grant"
    ));
    match credentials::flag_needs_relogin_if_unchanged(email, rejected_refresh_token) {
        Ok(credentials::FlagOutcome::Rotated) => logging::log(&format!(
            "flag_needs_relogin({email}): refresh token changed since the \
             invalid_grant; not flagging"
        )),
        Ok(_) => {}
        Err(e) => logging::log(&format!("flag_needs_relogin({email}): save failed: {e:#}")),
    }
}

/// The locked phase of a switch, optionally guarded by `expect_active`: if given
/// and the reloaded active account no longer matches it, the switch is skipped
/// (returns `Ok(None)`). The auto-swap daemon uses this so a manual switch that
/// lands between its decision and this commit isn't silently overridden by a now
/// stale choice. `acct`/`identity`/`backfilled` come from the caller's unlocked
/// phase-1 network work. Returns the display label on a real switch.
pub(crate) fn switch_to_guarded(
    provider: &'static dyn Provider,
    email: &str,
    acct: &Account,
    identity: &serde_json::Value,
    backfilled: bool,
    expect_active: Option<&str>,
    hold: bool,
) -> Result<Option<String>> {
    let label = acct.email.clone().unwrap_or_else(|| email.to_string());
    // Wait out any refresh in flight for this account, so the commit below
    // sees its saved rotation instead of writing a just-consumed refresh
    // token to the keychain.
    let _refresh = oauth::refresh_lock()?;
    let switched = with_state_lock(|_| {
        let mut st = State::load()?;
        if let Some(exp) = expect_active {
            if st.active.as_deref() != Some(exp)
                || autoswap::shell::pick_diverged_from_armed(CLAUDE_SLUG, &st)
            {
                logging::log(&format!(
                    "swap to {label} skipped: active changed to {:?} since the decision",
                    st.active
                ));
                return Ok(None);
            }
        }
        // Provider-agnostic: absorb any lagging on-disk rotations for the
        // outgoing account BEFORE we overwrite path[0] with the incoming
        // account's blob. This catches every configured credential file.
        credentials::absorb_before_switch(provider);
        // absorb_before_switch reaches back through the reentrant state lock
        // and rewrites state.json with any freshly-absorbed rotations. Our
        // in-memory `st` snapshot from a few lines above is now stale; reload
        // it here or our final `st.save()` will clobber the absorbed changes
        // (reintroducing the never-re-login regression). See H1 in the
        // round-1 codeaudit findings.
        //
        // NOTE: sync_active_from_keychain MUST run AFTER this reload, not
        // before it — otherwise the reload discards any keychain-derived
        // mutations to the in-memory snapshot, causing apply_account (via
        // the stale `st`) to later re-push pre-rotation tokens on switch-
        // back. See H_R2_1 in the round-2 codeaudit findings.
        st = State::load()?;
        // A remove may have landed during the unlocked phase 1; never make a
        // deleted account live (same rule as the provider path).
        if st.find(email).is_none() {
            bail!("{label} was removed before the switch could complete");
        }
        // Preserve any token rotation the currently-active session picked up
        // and wrote into the keychain (Claude legacy path). This mutates `st`
        // in memory; the final `st.save()` below persists the freshest tokens.
        sync_active_from_keychain(provider, &mut st);
        // A concurrent poll may have rotated this account's token after our
        // phase-1 snapshot; use whichever tokens are fresher so we never write a
        // stale (possibly already-superseded) refresh token to the keychain.
        let mut acct = acct.clone();
        if let Some(cur) = st.find(email) {
            if cur.expires_at > acct.expires_at {
                acct.adopt_tokens_if_newer(cur);
            }
        }
        let from = st.active.clone();
        // ~/.claude.json first, keychain last (the commit point), rollback on fail.
        apply_account(provider, &acct, identity, from.as_deref(), &label)?;
        // A pending live-write marker is only meaningful while its account
        // stays active with an unwritten renewal. The account leaving the slot
        // no longer owns it, and the incoming account's blob was just written
        // to it, so neither may later rewrite the slot over a vendor rotation.
        if let Some(prev) = from.as_deref() {
            if let Some(p) = st.find_mut(prev) {
                p.live_write_pending = None;
            }
        }
        if let Some(a) = st.find_mut(email) {
            a.live_write_pending = None;
            // Rotations are already saved by `oauth::ensure_fresh`; this
            // records an active-slot adoption from `prepare_switch`.
            a.adopt_tokens_if_newer(&acct);
            if backfilled {
                a.oauth_account = Some(identity.clone());
            }
        }
        st.active = Some(email.to_string());
        st.set_manual_pick(CLAUDE_SLUG, hold.then_some(email));
        let tier = pick_tier(&st, CLAUDE_SLUG, email);
        st.set_manual_pick_tier(CLAUDE_SLUG, hold.then_some(tier.as_str()));
        let hold = hold && tier >= autoswap::Tier::Draining;
        st.set_manual_hold(CLAUDE_SLUG, hold.then_some(email));
        st.note_active_change(
            CLAUDE_SLUG,
            from.as_deref(),
            email,
            if expect_active.is_some() {
                autoswap::memory::ChangeKind::Auto
            } else {
                autoswap::memory::ChangeKind::Manual
            },
        );
        // The login is already committed to the keychain + ~/.claude.json at this
        // point; if only the state.json bookkeeping write fails, say so clearly.
        if let Err(e) = st.save() {
            return Err(e).context("the login was switched but recording it in state.json failed");
        }
        Ok(Some((from, expect_active.is_none())))
    })?;
    if let Some((from, manual)) = switched {
        logging::log(&format!("switch -> {label}"));
        if manual {
            log_manual_switch(CLAUDE_SLUG, from.as_deref(), email);
        }
        Ok(Some(label))
    } else {
        Ok(None)
    }
}

/// Resolve the identity to write for this account. Returns (identity,
/// backfilled_from_network). Errors if it can't be resolved (e.g. offline and
/// never captured) — the caller then does NOT switch, rather than half-applying
/// one. `provider` is threaded through so later phases can dispatch off it; the
/// v1 body still uses the Claude-specific profile endpoint (behavior-preserving).
fn resolve_identity(
    provider: &'static dyn Provider,
    acct: &Account,
) -> Result<(serde_json::Value, bool)> {
    let _ = provider;
    if let Some(v) = &acct.oauth_account {
        return Ok((v.clone(), false));
    }
    let built = usage::fetch_profile(&acct.access_token)
        .as_ref()
        .and_then(usage::oauth_account_from_profile);
    built.map(|v| (v, true)).ok_or_else(|| {
        anyhow!(
            "could not resolve this account's identity (offline?) — \
             run `usagio capture` for it while logged in"
        )
    })
}

/// Apply an account as the active login: write ~/.claude.json identity FIRST
/// (atomic tmp+rename), then the keychain token LAST (the flaky commit point).
/// If the keychain write fails, roll ~/.claude.json back to its prior contents
/// so both halves stay consistent, and return Err (never a half-applied switch).
/// `provider` is threaded through for the same forward-compat reason as
/// `resolve_identity`; the v1 body is the Claude-specific keychain path.
pub(crate) fn apply_account(
    provider: &'static dyn Provider,
    acct: &Account,
    identity: &serde_json::Value,
    from: Option<&str>,
    to: &str,
) -> Result<()> {
    let _ = provider;
    let prior = read_claude_json_raw();
    let identity_result = write_claude_identity(identity, acct.user_id.as_deref());
    if let Err(e) = &identity_result {
        logging::log(&format!(
            "event=switch from={} to={to} identity_written=err:{e:#} keychain_written=skipped",
            from.unwrap_or("<none>"),
        ));
        identity_result?;
    }
    if let Err(e) = keychain_write(&acct.keychain_blob) {
        if let Some((bytes, mode)) = &prior {
            // If the rollback ALSO fails we're half-applied (~/.claude.json points
            // at the new account, keychain still holds the old) — surface that
            // explicitly rather than silently swallowing the rollback error.
            // Re-patch the prior identity into the CURRENT file so Claude Code's
            // own writes since the snapshot survive the rollback.
            if let Err(re) = crate::providers::claude::rollback_claude_identity(bytes, *mode)
                .map_err(|e| anyhow!("{e}"))
            {
                logging::log(&format!(
                    "event=switch from={} to={to} identity_written=ok \
                     keychain_written=err:{e:#} rollback=err:{re:#}",
                    from.unwrap_or("<none>"),
                ));
                return Err(e).context(format!(
                    "writing the account into the keychain, and rolling back \
                     ~/.claude.json failed too ({re:#}); it may now point at the new \
                     account while the keychain holds the old — run `usagio \
                     switch` again to reconcile"
                ));
            }
        }
        logging::log(&format!(
            "event=switch from={} to={to} identity_written=ok keychain_written=err:{e:#}",
            from.unwrap_or("<none>"),
        ));
        return Err(e).context("writing the account into the keychain");
    }
    logging::log(&format!(
        "event=switch from={} to={to} identity_written=ok keychain_written=ok",
        from.unwrap_or("<none>"),
    ));
    // v0.8.0: drop the just-left account's ephemeral 429 tracker so a stale
    // counter can't re-fire the moment it becomes active again (e.g. after
    // NO_RETURN expires). Opus adversarial review BAD 4 — without this, a
    // real recovery on the returning account would be ignored: the first
    // 429 lands on count=1 (which was reset to 0 by us here), and the
    // second bumps to 2, immediately re-escalating on possibly-stale data.
    if let Some(prev) = from.filter(|p| !p.eq_ignore_ascii_case(to)) {
        reset_usage_fetch_tracker(prev);
    }
    Ok(())
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
