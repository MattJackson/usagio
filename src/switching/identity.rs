//! Keeping stored Claude identities in step with the live `~/.claude.json` / keychain login.

use crate::claude_local::{keychain_read, read_claude_identity};
use crate::providers::claude::usage;
use crate::providers::Provider;
use crate::state_ops::with_state_lock;
use crate::store::{Account, State};
use crate::{lifecycle, logging};

/// Whether the keychain / `~/.claude.json` identity is our tracked account:
/// match on `accountUuid` when both sides have one, otherwise case-insensitive
/// email. An account with no known identity yet adopts (self-heal); a known
/// email with nothing to compare against does NOT (stay safe). Pure, for tests.
pub(crate) fn identity_matches(
    acct_uuid: Option<&str>,
    acct_email: Option<&str>,
    json_uuid: Option<&str>,
    json_email: Option<&str>,
) -> bool {
    lifecycle::identity_fields_match(acct_uuid, acct_email, json_uuid, json_email)
}

/// Best-effort mirror of a rotated INACTIVE account's new grant back to the
/// vendor CLI's OS-native slot — but ONLY if the vendor's own active identity
/// (`read_active_identity`) currently agrees this account is the one it's
/// using. Claude Code's keychain / credentials-file slot is a single shared
/// resource across every locally known account — there is no per-account
/// file — so blindly mirroring an inactive account's rotation into it would
/// silently clobber whatever account is genuinely logged in right now. This
/// is the write-path counterpart of the identity check `sync_active_from_
/// keychain` already uses before adopting a rotation FROM the slot.
///
/// Returns `None` when mirroring was skipped (no vendor identity, or it
/// doesn't match this account) — the caller logs that as `mirror_back=skip`,
/// distinct from an attempted mirror that failed (`Some(Err(_))`).
pub(crate) fn mirror_inactive_rotation(
    provider: &'static dyn Provider,
    acct: &Account,
) -> Option<std::result::Result<(), String>> {
    // Serialize the identity-check + keychain write against a concurrent switch
    // by holding the cross-process state lock across BOTH, and re-read the
    // vendor identity INSIDE the lock. A switch (`usagio switch` in another
    // process, or a menu switch on the main thread) holds the same lock across
    // its ~/.claude.json + keychain writes, so we observe either the pre-switch
    // identity (still matches → safe to mirror) or the post-switch identity
    // (mismatch → skip) — never a torn in-between where this inactive account's
    // rotated token would be written over the account a switch just committed,
    // clobbering a valid active token. `ACTIVE_SLOT_LOCK` alone can't close this
    // race: it's in-process only, and the racing switch is often a separate
    // process. This lock is held across a `security(1)` write, but the mirror
    // only runs when a rotation was actually detected (rare), and the switch
    // path already holds the state lock across its own keychain write.
    let outcome = with_state_lock(|| {
        let vendor_identity = match provider.read_active_identity() {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(None),
            // Surface the failure (logged as mirror_back=err) instead of
            // passing it off as a benign identity mismatch.
            Err(e) => return Ok(Some(Err(e.to_string()))),
        };
        let matches = identity_matches(
            acct.identity_uuid().as_deref(),
            acct.email.as_deref(),
            vendor_identity.uuid.as_deref(),
            vendor_identity
                .email
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
        );
        if !matches {
            return Ok(None);
        }
        let result = provider
            .mirror_rotated_token(&acct.keychain_blob)
            .map_err(|e| e.to_string());
        crate::logging::log(&format!(
            "event=mirror account={} result={}",
            acct.key(),
            match &result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("err:{e}"),
            }
        ));
        Ok(Some(result))
    });
    // On the Ok path, the inner Option is the mirror outcome; if we couldn't
    // even acquire the lock / run the closure this cycle, skip (None) — the
    // same "do nothing" treatment as a vendor-identity read miss.
    outcome.unwrap_or_default()
}

/// Refresh metadata independently of token rotation, reading
/// `~/.claude.json` itself. The poll shares one reading through
/// [`sync_active_claude_identity_from`] instead.
#[cfg(test)]
pub(crate) fn sync_active_claude_identity(state: &mut State) {
    sync_active_claude_identity_from(state, read_claude_identity().0.as_ref());
}

/// [`sync_active_claude_identity`] against an `oauthAccount` the caller has
/// already read from `~/.claude.json` (one parse shared across a poll cycle).
pub(crate) fn sync_active_claude_identity_from(
    state: &mut State,
    identity: Option<&serde_json::Value>,
) {
    let (Some(active), Some(identity)) = (state.active.clone(), identity) else {
        return;
    };
    let Some(account) = state.find_mut(&active) else {
        return;
    };
    let saved_at = account
        .oauth_account
        .as_ref()
        .and_then(|o| o.get("profileFetchedAt"))
        .and_then(|v| v.as_i64());
    let incoming_at = identity.get("profileFetchedAt").and_then(|v| v.as_i64());
    // The CLI's older on-disk profile must not undo a newer background fetch.
    if saved_at.is_some_and(|saved| incoming_at.is_none_or(|incoming| incoming < saved)) {
        return;
    }
    if identity_matches(
        account.identity_uuid().as_deref(),
        account.email.as_deref(),
        identity.get("accountUuid").and_then(|v| v.as_str()),
        identity.get("emailAddress").and_then(|v| v.as_str()),
    ) {
        update_claude_identity(account, identity);
    }
}

/// Merge fresh vendor metadata without dropping fields absent in older clients.
pub(crate) fn update_claude_identity(account: &mut Account, identity: &serde_json::Value) {
    let Some(fields) = identity.as_object() else {
        return;
    };
    let saved = account
        .oauth_account
        .get_or_insert_with(|| serde_json::json!({}));
    if !saved.is_object() {
        *saved = serde_json::json!({});
    }
    let tier_changed = ["organizationType", "organizationRateLimitTier"]
        .iter()
        .any(|key| {
            identity
                .get(*key)
                .is_some_and(|value| !value.is_null() && saved.get(*key) != Some(value))
        });
    if tier_changed
        && identity
            .get("planDisplayName")
            .and_then(|v| v.as_str())
            .is_none_or(|name| name.trim().is_empty())
    {
        saved.as_object_mut().unwrap().remove("planDisplayName");
    }
    for (key, value) in fields {
        if !value.is_null() {
            saved[key] = value.clone();
        }
    }
    if !account.no_subscription {
        if let Some(name) = identity
            .get("planDisplayName")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            account.plan = Some(name.to_string());
        } else if tier_changed {
            let get = |key: &str| account.oauth_account.as_ref()?.get(key)?.as_str();
            account.plan = usage::plan_label(
                None,
                get("organizationType"),
                get("organizationRateLimitTier"),
            );
        }
    }
}

/// If the account currently in the keychain is genuinely our active account,
/// adopt any token rotation a live `claude` session performed. Verified by
/// identity: `/login` into a *different* account rewrites ~/.claude.json's
/// oauthAccount, so a mismatch means the keychain is not our active account and
/// we must NOT overwrite its stored tokens. In-place rotation keeps the same
/// account uuid/email, so legitimate rotation is still captured.
pub(crate) fn sync_active_from_keychain(provider: &'static dyn Provider, state: &mut State) {
    sync_active_from_keychain_with(provider, state, read_claude_identity().0.as_ref());
}

/// [`sync_active_from_keychain`] against an `oauthAccount` already read from
/// `~/.claude.json`, so a caller can parse that file once and share it.
pub(crate) fn sync_active_from_keychain_with(
    provider: &'static dyn Provider,
    state: &mut State,
    json_oauth: Option<&serde_json::Value>,
) {
    sync_active_claude_identity_from(state, json_oauth);
    let _ = provider;
    let Some(active) = state.active.clone() else {
        return;
    };
    let Some(blob) = keychain_read() else { return };
    let Ok(fresh) = Account::from_keychain_blob(&blob) else {
        return;
    };
    let json_uuid = json_oauth
        .and_then(|o| o.get("accountUuid"))
        .and_then(|x| x.as_str())
        .map(String::from);
    let json_email = json_oauth
        .and_then(|o| o.get("emailAddress"))
        .and_then(|x| x.as_str())
        .map(str::to_ascii_lowercase);

    let Some(acct) = state.find_mut(&active) else {
        return;
    };
    // Determine whether the keychain/.claude.json identity matches this account.
    let matches = identity_matches(
        acct.identity_uuid().as_deref(),
        acct.email.as_deref(),
        json_uuid.as_deref(),
        json_email.as_deref(),
    );
    if !matches {
        logging::log(&format!(
            "sync: keychain identity does not match the active account; not adopting tokens \
             (event=sync_active_from_keychain account={active} outcome=identity_mismatch)"
        ));
        return;
    }
    // A renewal whose live write failed left a marker naming the stored grant
    // the slot never received: the slot is older, not fresher (same rule as
    // `lifecycle::active_refresh_cas`).
    if acct.live_write_pending.as_deref()
        == Some(logging::token_fingerprint(&acct.refresh_token).as_str())
        && acct.refresh_token != fresh.refresh_token
    {
        logging::log(&format!(
            "event=sync_active_from_keychain account={active} outcome=skipped \
             reason=pending_live_write"
        ));
        return;
    }
    logging::log(&format!(
        "event=sync_active_from_keychain account={active} outcome=adopted \
         at_prefix={} rt_prefix={}",
        logging::token_fingerprint(&fresh.access_token),
        logging::token_fingerprint(&fresh.refresh_token),
    ));
    acct.access_token = fresh.access_token;
    acct.refresh_token = fresh.refresh_token;
    acct.expires_at = fresh.expires_at;
    acct.keychain_blob = fresh.keychain_blob;
}
