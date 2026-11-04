//! `usagio capture` / `add`: save the logged-in account, and the shared login-persist path the sign-in window also uses.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;

use crate::notifications::notify;
use crate::providers::require;
use crate::providers::CLAUDE_SLUG;
use crate::state_ops::with_state_lock;
use crate::store::{Account, CachedUsage, ProviderAccount, State};
use crate::switching::identity::update_claude_identity;
use crate::switching::provider::{commit_provider_switch, provider_by_slug};
use crate::{lifecycle, logging, login, providers, store};

// ---------------------------------------------------------------------------
// capture — snapshot the current keychain login, keyed by its email
// ---------------------------------------------------------------------------

pub(crate) fn cmd_capture(args: &[String]) -> Result<()> {
    // Mirror the menu's `menubar::actions::handle_capture` dispatch exactly — the CLI is
    // the headless twin of the "Capture ▸ <provider>" menu items, sharing the
    // same `capture_current` / `capture_current_generic` code. Claude uses the
    // dedicated `state.accounts` bucket; every other registered provider uses
    // the generic `state.providers[slug]` slot. No arg defaults to Claude
    // (back-compat with the original Claude-only `usagio capture`).
    // Slugs are lowercase; match a `Codex` the way a `Codex:` qualifier is.
    let slug = args
        .first()
        .map_or_else(|| CLAUDE_SLUG.to_string(), |s| s.to_ascii_lowercase());
    let slug = slug.as_str();
    if slug == CLAUDE_SLUG {
        let (email, existed) = capture_current()?;
        println!(
            "{} {email} — it's the active login.",
            if existed { "Refreshed" } else { "Captured" }
        );
        return Ok(());
    }
    // Validate the provider up front for a clean error (and its display name)
    // before touching the keychain — same guard the menu path applies.
    let provider = provider_by_slug(slug)?;
    let (key, existed) = capture_current_generic(slug)?;
    println!(
        "{} {} {key} — it's the active {} login.",
        if existed { "Refreshed" } else { "Captured" },
        provider.display_name(),
        slug
    );
    Ok(())
}

/// Capture the account currently in the keychain, keyed by its email. Returns
/// (email, existed_already). Shared by the CLI and the menu bar.
///
/// Delegates identity resolution (email lookup + `~/.claude.json` snapshot)
/// to `providers::claude`'s `LoginCapture::capture_current_login` — the same
/// logic every other provider's capture path (`capture_current_generic`)
/// already dispatches through — instead of re-deriving it inline (v0.5.2
/// simplification-02). Persistence still targets `state.accounts` (the
/// dedicated Claude bucket, epoch-MILLIS `expires_at`) rather than
/// `state.providers["claude"]` (the generic v2 slot, epoch-SECONDS
/// `expires_at`): those are different on-disk shapes, and Claude's rows are
/// read from `state.accounts` everywhere else (`build_snapshot`, `switch_to`,
/// `remove_account`, …), so routing storage through
/// `capture_current_generic` as well would silently split a Claude account
/// across two incompatible buckets.
pub(crate) fn capture_current() -> Result<(String, bool)> {
    let provider = provider_by_slug(CLAUDE_SLUG)?;
    let captured = require(provider.login_capture())
        .and_then(|c| c.capture_current_login())
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| {
            anyhow!(
                "no claude.ai login found (keychain / ~/.claude/.credentials.json) — run \
                 `claude` and /login first"
            )
        })?;
    // `Account::from_keychain_blob` parses `expires_at` (epoch millis) out of
    // the verbatim blob exactly as before — reusing it here keeps this an
    // identity-resolution dedup, not a token-parsing rewrite.
    let mut acct = Account::from_keychain_blob(&captured.secret_blob)?;
    let email = captured.identity.email.clone().context(
        "could not determine this account's email (offline?) — connect and try `capture` again",
    )?;
    acct.email = Some(email.clone());
    // `capture_current_login` packs both fields into one native_blob (see its
    // doc comment in `providers/claude/mod.rs`); unpack them back into
    // `Account`'s dedicated fields.
    acct.oauth_account = captured
        .identity
        .native_blob
        .get("oauthAccount")
        .cloned()
        .filter(|v| !v.is_null());
    acct.user_id = captured
        .identity
        .native_blob
        .get("userID")
        .and_then(|v| v.as_str())
        .map(String::from);

    let (at_prefix, rt_prefix, expires_at) = (
        logging::token_fingerprint(&acct.access_token),
        logging::token_fingerprint(&acct.refresh_token),
        acct.expires_at,
    );
    let existed = with_state_lock(|| {
        let mut state = State::load()?;
        let existing = state.find(&email);
        let existed = existing.is_some();
        // Re-capturing only refreshes identity/tokens — keep the usage snapshot
        // the scheduler already fetched, so `list`/menu don't blank to "no data".
        acct.cached_usage = merged_cached_usage(existing);
        state.upsert(acct);
        state.active = Some(email.clone());
        state.save()?;
        Ok(existed)
    })?;
    logging::log(&format!(
        "event=capture account={email} existed={existed} at_prefix={at_prefix} \
         rt_prefix={rt_prefix} expires_at={expires_at}"
    ));
    Ok((email, existed))
}

/// The cached usage to keep when (re)capturing an account: the snapshot we
/// already had for this email, if any (capture only refreshes identity/tokens; a
/// freshly captured account has no snapshot of its own). Pure, for tests.
pub(crate) fn merged_cached_usage(existing: Option<&Account>) -> Option<CachedUsage> {
    existing.and_then(|a| a.cached_usage.clone())
}

/// Capture whatever `slug` (a non-Claude provider) currently has logged in
/// on this host and persist it into state v2's `State::providers[slug]`
/// slot, exactly like `capture_current` does for Claude's `accounts` list.
/// Closes the gap documented in `menubar.rs::handle_capture`'s prior
/// "persistence lands in a later phase" notification. Returns the account
/// key and whether it already existed (so callers can say "Captured" vs.
/// "Refreshed").
pub(crate) fn capture_current_generic(slug: &str) -> Result<(String, bool)> {
    let provider = provider_by_slug(slug)?;
    let captured = require(provider.login_capture())
        .and_then(|c| c.capture_current_login())
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| anyhow!("nothing to capture — is {slug} logged in on this host?"))?;
    let key = provider.account_identifier(&captured.identity);
    let expires_at = Utc::now().timestamp() + captured.tokens.expires_in_secs;

    let existed = with_state_lock(|| {
        let mut state = State::load()?;
        state.adopt_legacy_provider_key(slug, &key);
        let existing = state.find_provider_account(slug, &key).cloned();
        let existed = existing.is_some();
        let acct = ProviderAccount {
            key: key.clone(),
            secret_blob: captured.secret_blob.clone(),
            access_token: captured.tokens.access.clone(),
            refresh_token: captured.tokens.refresh.clone().unwrap_or_default(),
            expires_at,
            identity_email: captured.identity.email.clone(),
            identity_uuid: captured.identity.uuid.clone(),
            identity_display_name: captured.identity.display_name.clone(),
            identity_native_blob: captured.identity.native_blob.clone(),
            // Re-capturing only refreshes identity/tokens — keep any usage
            // snapshot the scheduler already fetched (mirrors
            // `merged_cached_usage` above for Claude).
            cached_usage: existing.as_ref().and_then(|a| a.cached_usage.clone()),
            notif_state: existing
                .as_ref()
                .map(|a| a.notif_state.clone())
                .unwrap_or_default(),
            needs_relogin: false,
            no_subscription: false,
            plan: None,
        };
        state.upsert_provider_account(slug, acct);
        // Capture always reflects whatever is currently logged in, so it
        // also becomes the active account for this provider — matching
        // Claude's `capture_current`, which sets `state.active` the same way.
        state.provider_accounts_mut(slug).active = Some(key.clone());
        state.save()?;
        Ok(existed)
    })?;
    logging::log(&format!(
        "event=capture provider={slug} account={key} existed={existed}"
    ));
    Ok((key, existed))
}

/// Save a login the user just completed in usagio's sign-in window or browser
/// ("Renew login…"). Unlike `capture`, it never changes which account is
/// active. Active Codex renewals are staged until the client drains, then
/// published and verified through the shared handoff. Claude retains its
/// existing live-login update path.
///
/// `new_store` is the sign-in window store a brand-new sign-in used; it's
/// remembered on the account so later renewals reuse that session.
pub(crate) fn persist_login(
    provider: &dyn providers::trait_def::Provider,
    captured: providers::trait_def::CapturedAccount,
    new_store: Option<&str>,
) -> Result<login::Renewed> {
    let slug = provider.provider_id();
    let key = provider.account_identifier(&captured.identity);
    if slug == CLAUDE_SLUG {
        return persist_claude_login(provider, key, captured, new_store);
    }
    let expires_at = Utc::now().timestamp() + captured.tokens.expires_in_secs;
    let active = with_state_lock(|| {
        let mut state = State::load()?;
        state.adopt_legacy_provider_key(slug, &key);
        let existing = state.find_provider_account(slug, &key).cloned();
        let acct = ProviderAccount {
            key: key.clone(),
            secret_blob: captured.secret_blob.clone(),
            access_token: captured.tokens.access.clone(),
            refresh_token: captured.tokens.refresh.clone().unwrap_or_default(),
            expires_at,
            identity_email: captured.identity.email.clone(),
            identity_uuid: captured.identity.uuid.clone(),
            identity_display_name: captured.identity.display_name.clone(),
            identity_native_blob: captured.identity.native_blob.clone(),
            cached_usage: existing.as_ref().and_then(|a| a.cached_usage.clone()),
            notif_state: existing
                .as_ref()
                .map(|a| a.notif_state.clone())
                .unwrap_or_default(),
            needs_relogin: false,
            no_subscription: existing.as_ref().is_some_and(|a| a.no_subscription),
            plan: existing.as_ref().and_then(|a| a.plan.clone()),
        };
        if let Some(store) = new_store {
            state
                .provider_accounts_mut(slug)
                .login_stores
                .insert(key.to_lowercase(), store.to_string());
        }
        let active = state.provider_accounts_mut(slug).active.as_deref() == Some(key.as_str());
        // A switch to a DIFFERENT account is already queued: never overwrite
        // that intent. Stage the renewed tokens only; the queued switch
        // finishes first.
        let other_queued = state
            .provider_accounts(slug)
            .and_then(|pa| pa.pending_switch.as_ref())
            .is_some_and(|pending| pending.key != key);
        if other_queued && active {
            logging::log(&format!(
                "event=renewal_handoff_deferred provider={slug} reason=other_switch_pending"
            ));
        }
        let active = active && !other_queued;
        if active {
            let accounts = state.provider_accounts_mut(slug);
            let drain_token = accounts
                .pending_switch
                .as_ref()
                .and_then(|pending| pending.drain_token.clone());
            accounts.pending_switch = Some(store::PendingSwitch {
                key: key.clone(),
                // A renewal is not a pick: `manual: false` on a switch whose
                // target is already active marks it so the commit leaves
                // manual_pick/manual_hold alone.
                manual: false,
                from: accounts.active.clone(),
                drain_token,
                credentials_written: false,
                leave_latch: None,
                renewed_account: Some(acct),
                renewal: true,
            });
        } else {
            state.upsert_provider_account(slug, acct);
            // A queued switch TO this account may already have written its old
            // credentials to the vendor file; make the retry rewrite them.
            if let Some(pending) = state
                .provider_accounts_mut(slug)
                .pending_switch
                .as_mut()
                .filter(|pending| pending.key == key)
            {
                pending.credentials_written = false;
            }
        }
        state.save()?;
        Ok(active)
    })?;
    let mut made_live = false;
    if active {
        let _refresh = lifecycle::refresh_lock()?;
        let attempt = (|| -> Result<bool> {
            let status = lifecycle::client_status(provider)?;
            let ready = with_state_lock(|| {
                let mut state = State::load()?;
                lifecycle::prepare_handoff(provider, &status, &mut state, &key, false)
            })?;
            if ready {
                commit_provider_switch(provider, slug, &key, false, false)?;
            }
            Ok(ready)
        })();
        match attempt {
            Ok(ready) => made_live = ready,
            Err(error) => logging::log(&format!(
                "event=renewal_handoff_pending provider={slug} reason={error}"
            )),
        }
    }
    Ok(login::Renewed {
        key,
        login_expires_at: None,
        made_live,
    })
}

pub(crate) fn persist_claude_login(
    provider: &dyn providers::trait_def::Provider,
    email: String,
    captured: providers::trait_def::CapturedAccount,
    new_store: Option<&str>,
) -> Result<login::Renewed> {
    let fresh = Account::from_keychain_blob(&captured.secret_blob)?;
    let native = &captured.identity.native_blob;
    let (blob, active) = with_state_lock(|| {
        let mut state = State::load()?;
        let active = state
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&email));
        match state.find_mut(&email) {
            Some(a) => {
                a.adopt_login(&fresh);
                if let Some(identity) = native.get("oauthAccount").filter(|v| v.is_object()) {
                    update_claude_identity(a, identity);
                }
            }
            None => {
                let mut acct = fresh.clone();
                acct.email = Some(email.clone());
                acct.oauth_account = native.get("oauthAccount").cloned().filter(|v| !v.is_null());
                state.upsert(acct);
            }
        }
        if let (Some(store), Some(a)) = (new_store, state.find_mut(&email)) {
            a.login_store = Some(store.to_string());
        }
        state.save()?;
        let a = state
            .find(&email)
            .context("account vanished while saving")?;
        let identity = providers::trait_def::IdentitySnapshot {
            native_blob: serde_json::json!({
                "oauthAccount": a.oauth_account.clone().unwrap_or_else(|| native["oauthAccount"].clone()),
                "userID": a.user_id,
            }),
            ..captured.identity.clone()
        };
        // Inside the lock, so a concurrent switch can't land between the
        // "is it active" check and this write.
        if active {
            if let Err(e) = require(provider.active_slot())
                .and_then(|slot| slot.write_active_account(&a.keychain_blob, &identity))
            {
                // Durable marker so the next poll's CAS republishes this exact
                // grant instead of adopting the old slot over it.
                let fp = logging::token_fingerprint(&a.refresh_token);
                if let Some(a) = state.find_mut(&email) {
                    a.live_write_pending = Some(fp);
                }
                state.save()?;
                return Err(anyhow!(
                    "saved, but making it live failed (will be retried on the next poll): {e}"
                ));
            }
        }
        Ok((a.keychain_blob.clone(), active))
    })?;
    Ok(login::Renewed {
        key: email,
        login_expires_at: store::blob_refresh_token_expires_at(&blob),
        made_live: active,
    })
}

/// Once per expiry, tell the user a renewable login is about to die (Claude
/// Code itself starts nagging "Your login expires in N days" at the same
/// point). Lapsed-plan accounts are skipped — they render as their plan.
pub(crate) fn warn_expiring_logins() {
    if providers::get(CLAUDE_SLUG)
        .and_then(|p| p.login_flow())
        .is_none()
    {
        return;
    }
    let now = Utc::now().timestamp_millis();
    let due = with_state_lock(|| {
        let mut st = State::load()?;
        let mut due = Vec::new();
        for a in st.accounts.iter_mut().filter(|a| !a.no_subscription) {
            let at = a.refresh_token_expires_at();
            if login::should_warn_expiry(at, now, a.notif_state.login_expiry_warned) {
                a.notif_state.login_expiry_warned = at;
                due.push((a.key().to_string(), at.unwrap_or(now)));
            }
        }
        if !due.is_empty() {
            st.save()?;
        }
        Ok(due)
    })
    .unwrap_or_default();
    for (email, at) in due {
        let days = ((at - now) / 86_400_000).max(0);
        let when = if days >= 1 {
            format!("in {days}d")
        } else {
            "today".to_string()
        };
        notify(&format!(
            "{email}: login expires {when} — Renew login… in usagio"
        ));
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
