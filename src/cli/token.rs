//! `usagio token`: print a fresh access token.

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::lifecycle::{active_refresh_cas, ActiveRefreshOutcome};
use crate::providers::claude::oauth;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::state_ops::with_state_lock;
use crate::store::{Account, State};
use crate::switching::claude::{ensure_fresh_with_fallback, flag_needs_relogin};
use crate::{logging, providers};

// ---------------------------------------------------------------------------
// token
// ---------------------------------------------------------------------------

/// `usagio token` is Claude-only: accept `claude:<x>`, reject other qualifiers.
pub(crate) fn token_selector(sel: &str) -> Result<&str> {
    match sel.split_once(':') {
        Some((q, rest)) if q.eq_ignore_ascii_case(CLAUDE_SLUG) => Ok(rest),
        Some((q, _)) => bail!("`usagio token` is Claude-only; '{q}:' accounts are not supported"),
        None => Ok(sel),
    }
}

/// The poll's locked active-account CAS: `(outcome, stored account)`. An
/// idle cycle leaves the account unchanged, and `save` then neither rewrites
/// state.json nor rotates the rolling backups.
pub(crate) fn poll_active_cas(
    provider: &dyn Provider,
    email: &str,
) -> Result<(Option<ActiveRefreshOutcome>, Option<Account>)> {
    with_state_lock(|lock| {
        let mut st = State::load()?;
        let outcome = st
            .find_mut(email)
            .map(|a| active_refresh_cas(lock, provider, a));
        st.save()?;
        let fresh = st.find(email).cloned();
        Ok((outcome, fresh))
    })
}

/// `usagio token` for the account it saw active. GUARANTEE: never POST /token
/// for the ACTIVE account (single-use refresh token would invalidate the live
/// vendor CLI's copy → forced re-login): ADOPT the vendor slot's current token
/// instead, so a script gets the same bearer the vendor CLI is using. If the
/// slot is unreadable the stored token is returned rather than POST. Returns
/// `None` once the account is no longer active. Runs under the state lock
/// (`active_refresh_cas` may write the slot), like `claim_refresh_token`.
pub(crate) fn token_active_cas(
    provider: &dyn Provider,
    email: &str,
) -> Result<Option<(ActiveRefreshOutcome, Account)>> {
    with_state_lock(|lock| {
        let mut st = State::load()?;
        if !st
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(email))
        {
            return Ok(None);
        }
        let Some(a) = st.find_mut(email) else {
            return Ok(None);
        };
        let outcome = active_refresh_cas(lock, provider, a);
        let acct = a.clone();
        st.save()
            .context("recording the adopted token in state.json failed")?;
        Ok(Some((outcome, acct)))
    })
}

pub(crate) fn cmd_token(selector: Option<&str>) -> Result<()> {
    let state = State::load()?;
    let email = match selector.map(token_selector).transpose()? {
        Some(sel) => state.resolve(sel)?,
        None => match state.accounts.as_slice() {
            [only] => only.key().to_string(),
            [] => bail!("no accounts; capture one with: usagio capture"),
            _ => bail!("multiple accounts; specify one by email or prefix"),
        },
    };
    let provider = providers::get(CLAUDE_SLUG)
        .ok_or_else(|| anyhow!("internal: provider '{CLAUDE_SLUG}' not registered"))?;
    let active = if state.active.as_deref() == Some(email.as_str()) {
        token_active_cas(provider, &email)?
    } else {
        None
    };
    let acct = match active {
        Some((ActiveRefreshOutcome::Adopted, fresh)) => {
            // Never refreshed for the active account (see `token_active_cas`):
            // say so when Claude Code has left it expired.
            if fresh.expires_at <= Utc::now().timestamp_millis() {
                eprintln!(
                    "warning: {email}'s access token (the one Claude Code is using) has \
                     expired; run `claude` to renew it — usagio never refreshes the \
                     active login"
                );
            }
            fresh
        }
        Some((ActiveRefreshOutcome::RefreshFailed, stored)) => {
            logging::log(&format!(
                "token: active account {email} slot unreadable; returning stored \
                 token (never POST /token for the active account)"
            ));
            stored
        }
        None => {
            // Network I/O stays OUTSIDE the cross-process lock so a slow/hung
            // token endpoint can't stall the daemon poll or a concurrent
            // switch (which all take the same lock).
            let mut acct = State::load()?
                .find(&email)
                .cloned()
                .with_context(|| format!("no account matches '{email}'"))?;
            // Reactive path: try the disk fallback before flagging so a
            // rotation the vendor CLI wrote to ~/.claude/.credentials.json (or
            // that fsnotify missed) is adopted here instead of tripping
            // needs_relogin on a token that's already been superseded on disk.
            match ensure_fresh_with_fallback(provider, &email, &mut acct) {
                // `ensure_fresh` saved any rotation itself.
                Ok(_) => acct,
                Err(oauth::RefreshError::InvalidGrant) => {
                    flag_needs_relogin(&email, &acct.refresh_token);
                    bail!(
                        "account {email} needs re-login (refresh token rejected); run \
                         `claude /login` for it or click the account row in the menu"
                    );
                }
                Err(e) => bail!("token refresh for {email} failed: {e}"),
            }
        }
    };
    let token = acct.access_token.clone();
    println!("{token}");
    Ok(())
}

#[cfg(test)]
#[path = "token_tests.rs"]
mod tests;
