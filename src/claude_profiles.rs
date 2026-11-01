//! Low-frequency plan metadata refresh, independent of usage fetch eligibility.

use crate::providers::claude::usage;
use crate::store::{Account, State};
use crate::{
    identity_matches, logging, notify_subscription_change, update_claude_identity, with_state_lock,
};
use chrono::Utc;

const REFRESH_SECS: i64 = 1800;
const RETRY_SECS: i64 = 300;
pub(crate) const BATCH_SIZE: usize = 2;

/// Run after usage and swap decisions. This never refreshes tokens or touches
/// the vendor's active login. The existing credential lifecycle warms inactive
/// tokens before each watch cycle, including accounts whose usage is locked.
pub(crate) fn refresh(limit: usize, force: bool) {
    refresh_with(limit, force, usage::fetch_profile_metadata);
}

fn refresh_with(
    limit: usize,
    force: bool,
    mut fetch: impl FnMut(&str) -> Option<serde_json::Value>,
) {
    let Ok(state) = State::load() else { return };
    let now = Utc::now().timestamp();
    let mut candidates: Vec<_> = state
        .accounts
        .iter()
        .filter(|a| eligible(a, now, force))
        .collect();
    // Oldest due account first; a failed request cannot starve later accounts.
    candidates.sort_by_key(|a| a.profile_refresh_after.unwrap_or(0));
    for candidate in candidates.into_iter().take(limit) {
        let claimed = with_state_lock(|| {
            let mut state = State::load()?;
            let Some(account) = state.find_mut(candidate.key()) else {
                return Ok(None);
            };
            if !eligible(account, Utc::now().timestamp(), force)
                || account.profile_refresh_after != candidate.profile_refresh_after
            {
                return Ok(None);
            }
            // Persist before network I/O: restarts and concurrent pollers also
            // respect the retry window, even when the request fails.
            account.profile_refresh_after = Some(Utc::now().timestamp() + RETRY_SECS);
            let snapshot = account.clone();
            state.save()?;
            Ok(Some(snapshot))
        });
        let snapshot = match claimed {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => continue,
            Err(error) => {
                logging::log(&format!("profile refresh: claim failed: {error:#}"));
                continue;
            }
        };
        let Some(profile) = fetch(&snapshot.access_token) else {
            continue;
        };
        if let Err(error) = commit(&snapshot, &profile) {
            logging::log(&format!("profile refresh: save failed: {error:#}"));
        }
    }
}

fn eligible(account: &Account, now: i64, force: bool) -> bool {
    !account.needs_relogin
        // Lapsed accounts already use the subscription-recovery path.
        && !account.no_subscription
        && !account.access_token.is_empty()
        && account.expires_at > (now + 15).saturating_mul(1000)
        && (force || account.profile_refresh_after.is_none_or(|at| at <= now))
}

fn commit(snapshot: &Account, profile: &serde_json::Value) -> anyhow::Result<bool> {
    let (Some(mut identity), Some(mut plan)) = (
        usage::oauth_account_from_profile(profile),
        usage::plan_status_from_profile(profile),
    ) else {
        return Ok(false);
    };
    // A malformed or wrong-account response must never rename another slot.
    let email = identity.get("emailAddress").and_then(|v| v.as_str());
    let uuid = identity.get("accountUuid").and_then(|v| v.as_str());
    if plan.label.is_none()
        || email.is_some_and(|email| !email.eq_ignore_ascii_case(snapshot.key()))
        || (email.is_none() && uuid.is_none())
        || !identity_matches(
            snapshot.identity_uuid().as_deref(),
            snapshot.email.as_deref(),
            uuid,
            email,
        )
    {
        return Ok(false);
    }
    let changed = with_state_lock(|| {
        let mut state = State::load()?;
        let Some(account) = state.find_mut(snapshot.key()) else {
            return Ok(None);
        };
        // Usage may advance while the profile is in flight. Credentials and
        // identity must still be the exact generation we queried.
        if account.access_token != snapshot.access_token
            || account.refresh_token != snapshot.refresh_token
            || account.expires_at != snapshot.expires_at
            || account.keychain_blob != snapshot.keychain_blob
            || account.oauth_account != snapshot.oauth_account
            || account.plan != snapshot.plan
            || account.no_subscription != snapshot.no_subscription
            || account.needs_relogin != snapshot.needs_relogin
            || account.profile_refresh_after != snapshot.profile_refresh_after
        {
            return Ok(None);
        }
        let changed = account.no_subscription == plan.active;
        identity["profileFetchedAt"] = Utc::now().timestamp_millis().into();
        update_claude_identity(account, &identity);
        if plan.active {
            let get = |key: &str| account.oauth_account.as_ref()?.get(key)?.as_str();
            plan.label = usage::plan_label(
                get("planDisplayName"),
                get("organizationType"),
                get("organizationRateLimitTier"),
            );
        }
        account.no_subscription = !plan.active;
        if plan.label.is_some() {
            account.plan = plan.label;
        }
        account.profile_refresh_after = Some(Utc::now().timestamp() + REFRESH_SECS);
        state.save()?;
        Ok(Some(changed))
    })?;
    if changed == Some(true) {
        notify_subscription_change(crate::CLAUDE_SLUG, snapshot.key(), !plan.active);
    }
    Ok(changed.is_some())
}

#[cfg(test)]
#[path = "claude_profiles_tests.rs"]
mod tests;
