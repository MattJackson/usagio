//! Shared account identity and credential persistence rules.
use crate::credentials::with_state_lock;
use crate::providers::{IdentitySnapshot, Provider, TokenGrant};
use crate::store::{Account, ProviderAccount, State};
use crate::{logging, providers};
use anyhow::{bail, Result};
use chrono::Utc;

/// Serialize refresh and switch preparation across usagio processes. Always
/// acquire this before the state lock, including on the legacy Claude path.
pub fn refresh_lock() -> Result<std::fs::File> {
    use fs2::FileExt;
    let dir = crate::store::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("refresh.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

/// One transaction policy for both account stores: serialize, reload ownership,
/// decide freshness, perform vendor IO outside the state lock, then commit the
/// whole credential before releasing refresh ownership.
struct PreparedRefresh<T> {
    value: T,
    owned: bool,
    expires_at_ms: i64,
}

fn refresh_transaction<T, E: From<anyhow::Error>>(
    skew_secs: i64,
    prepare: impl FnOnce() -> std::result::Result<PreparedRefresh<T>, E>,
    request: impl FnOnce(&T) -> std::result::Result<T, E>,
    commit: impl FnOnce(&T, std::result::Result<T, E>) -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let _ownership = refresh_lock().map_err(E::from)?;
    let prepared = prepare()?;
    if prepared.owned
        || !needs_refresh(
            prepared.expires_at_ms,
            Utc::now().timestamp_millis(),
            skew_secs,
        )
    {
        return Ok(prepared.value);
    }
    let result = request(&prepared.value);
    commit(&prepared.value, result)
}

pub(crate) fn needs_refresh(expires_at_ms: i64, now_ms: i64, skew_secs: i64) -> bool {
    expires_at_ms.saturating_sub(now_ms) <= skew_secs.saturating_mul(1000)
}

pub fn ensure_provider_fresh(provider: &dyn Provider, key: &str) -> Result<ProviderAccount> {
    let slug = provider.provider_id();
    refresh_transaction(
        crate::credentials::REFRESH_SKEW_SECS,
        || prepare_provider_refresh(provider, key),
        |snapshot| {
            if snapshot.needs_relogin {
                bail!("renew login for {key}");
            }
            if snapshot.refresh_token.is_empty() {
                return Err(crate::providers::ProviderError::Auth.into());
            }
            let (grant, blob) =
                provider.refresh_credential(&snapshot.secret_blob, &snapshot.refresh_token)?;
            let mut candidate = snapshot.clone();
            candidate.secret_blob = blob;
            candidate.access_token = grant.access;
            candidate.refresh_token = grant
                .refresh
                .unwrap_or_else(|| snapshot.refresh_token.clone());
            candidate.expires_at = Utc::now().timestamp().saturating_add(grant.expires_in_secs);
            candidate.needs_relogin = false;
            validate_stored_identity(provider, &candidate)?;
            Ok(candidate)
        },
        |snapshot, refreshed| commit_provider_refresh(provider, slug, key, snapshot, refreshed),
    )
}

fn prepare_provider_refresh(
    provider: &dyn Provider,
    key: &str,
) -> Result<PreparedRefresh<ProviderAccount>> {
    let slug = provider.provider_id();
    let (snapshot, active) = crate::credentials::with_state_lock(|| {
        let state = crate::store::State::load()?;
        let active = state
            .provider_accounts(slug)
            .and_then(|p| p.active.as_deref())
            .is_some_and(|a| a.eq_ignore_ascii_case(key));
        let account = state
            .find_provider_account(slug, key)
            .ok_or_else(|| anyhow::anyhow!("no {slug} account matches '{key}'"))?;
        validate_stored_identity(provider, account)?;
        Ok((account.clone(), active))
    })?;
    // Saved selection can lag a vendor login. A credential currently owned by
    // the client must never be refreshed by us, even if state calls it inactive.
    if let Some(blob) = matching_active_blob(provider, &snapshot.identity_snapshot())
        .ok()
        .flatten()
    {
        let grant = provider.parse_stored_blob(&blob)?;
        let adopted = crate::credentials::with_state_lock(|| {
            let mut state = crate::store::State::load()?;
            let account = state
                .find_provider_account_mut(slug, key)
                .ok_or_else(|| anyhow::anyhow!("no {slug} account matches '{key}'"))?;
            if account.secret_blob == snapshot.secret_blob {
                account.secret_blob = blob;
                account.access_token = grant.access;
                account.refresh_token = grant.refresh.unwrap_or_default();
                account.expires_at = chrono::Utc::now()
                    .timestamp()
                    .saturating_add(grant.expires_in_secs);
                account.needs_relogin = false;
                let result = account.clone();
                state.save()?;
                return Ok(result);
            }
            Ok(account.clone())
        })?;
        return Ok(PreparedRefresh {
            expires_at_ms: adopted.expires_at.saturating_mul(1000),
            owned: true,
            value: adopted,
        });
    }
    Ok(PreparedRefresh {
        expires_at_ms: snapshot.expires_at.saturating_mul(1000),
        owned: active,
        value: snapshot,
    })
}

fn commit_provider_refresh(
    provider: &dyn Provider,
    slug: &str,
    key: &str,
    snapshot: &ProviderAccount,
    refreshed: Result<ProviderAccount>,
) -> Result<ProviderAccount> {
    crate::credentials::with_state_lock(|| {
        let mut state = crate::store::State::load()?;
        let account = state
            .find_provider_account_mut(slug, key)
            .ok_or_else(|| anyhow::anyhow!("no {slug} account matches '{key}'"))?;
        // Capture/adoption may have installed a newer generation during the request.
        if account.secret_blob != snapshot.secret_blob
            || account.refresh_token != snapshot.refresh_token
        {
            validate_stored_identity(provider, account)?;
            return Ok(account.clone());
        }
        let candidate = match refreshed {
            Ok(value) => value,
            Err(error) => {
                if matches!(
                    error.downcast_ref::<crate::providers::ProviderError>(),
                    Some(crate::providers::ProviderError::Auth)
                ) {
                    account.needs_relogin = true;
                    state.save()?;
                }
                return Err(error);
            }
        };
        *account = candidate.clone();
        state.save()?;
        Ok(candidate)
    })
}

pub fn identity_fields_match(
    expected_uuid: Option<&str>,
    expected_email: Option<&str>,
    actual_uuid: Option<&str>,
    actual_email: Option<&str>,
) -> bool {
    match (expected_uuid, actual_uuid) {
        (Some(a), Some(b)) => a == b,
        _ => match (expected_email, actual_email) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (None, _) => true,
            _ => false,
        },
    }
}

pub fn identity_matches(expected: &IdentitySnapshot, actual: &IdentitySnapshot) -> bool {
    // Adoption requires evidence of identity, even for legacy incomplete records.
    (expected.uuid.is_some() || expected.email.is_some())
        && identity_fields_match(
            expected.uuid.as_deref(),
            expected.email.as_deref(),
            actual.uuid.as_deref(),
            actual.email.as_deref(),
        )
}

pub fn validate_stored_identity(provider: &dyn Provider, account: &ProviderAccount) -> Result<()> {
    if let Some(key) = provider.identify_credential(&account.secret_blob) {
        if key.provider != provider.provider_id() || !key.key.eq_ignore_ascii_case(&account.key) {
            bail!(
                "stored credentials belong to another account; renew login for {}",
                account.key
            );
        }
    }
    Ok(())
}

pub fn set_provider_tokens(
    provider: &dyn Provider,
    account: &mut ProviderAccount,
    access: &str,
    refresh: &str,
    expires_at: i64,
) -> Result<()> {
    validate_stored_identity(provider, account)?;
    let grant = TokenGrant {
        access: access.to_owned(),
        refresh: Some(refresh.to_owned()),
        expires_in_secs: expires_at.saturating_sub(chrono::Utc::now().timestamp()),
    };
    let blob = provider.patch_stored_blob(&account.secret_blob, &grant)?;
    account.secret_blob = blob;
    account.access_token = grant.access;
    account.refresh_token = refresh.to_owned();
    account.expires_at = expires_at;
    account.needs_relogin = false;
    Ok(())
}

/// Read a vendor-owned credential only when its identity remains stable across
/// the read. Claude stores identity separately from its secret; Codex embeds it.
/// Neither path may adopt a different login into the selected saved account.
pub fn matching_active_blob(
    provider: &dyn Provider,
    expected: &IdentitySnapshot,
) -> Result<Option<String>> {
    let Some(before) = provider.read_active_identity()? else {
        return Ok(None);
    };
    if !identity_matches(expected, &before) {
        return Ok(None);
    }
    let blob = provider.read_active_slot()?;
    let Some(after) = provider.read_active_identity()? else {
        return Ok(None);
    };
    if !identity_matches(&before, &after) || !identity_matches(expected, &after) {
        return Ok(None);
    }
    Ok(blob)
}

/// Outcome of one `active_refresh_adopt` pass.
#[derive(Debug)]
pub(crate) enum ActiveRefreshOutcome {
    /// Read the slot Claude Code owns and synced our in-memory `acct` to
    /// whatever it currently holds (whether it had drifted or was already in
    /// sync). No token was minted.
    Adopted,
    /// The slot was unreadable/unparseable; `acct` is left untouched and the
    /// caller keeps the existing cache for this cycle.
    RefreshFailed,
}

/// Adopt only the matching vendor-owned login. Caller holds the state lock.
/// Never POST a refresh grant for a credential owned by the vendor client.
pub(crate) fn active_refresh_cas(
    provider: &dyn Provider,
    acct: &mut Account,
) -> ActiveRefreshOutcome {
    let email = acct.email.clone().unwrap_or_default();

    let expected = providers::IdentitySnapshot {
        email: acct.email.clone(),
        uuid: acct.identity_uuid(),
        display_name: None,
        native_blob: serde_json::Value::Null,
    };
    let before = match matching_active_blob(provider, &expected) {
        Ok(Some(b)) => b,
        Ok(None) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=slot_empty"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
        Err(e) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=read:{e}"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
    };
    let cc_acct = match Account::from_keychain_blob(&before) {
        Ok(a) => a,
        Err(e) => {
            logging::log(&format!(
                "event=active_refresh_adopt_failed account={email} reason=parse:{e:#}"
            ));
            return ActiveRefreshOutcome::RefreshFailed;
        }
    };

    let drifted = cc_acct.access_token != acct.access_token;
    let cc_prefix = logging::tok_prefix(&cc_acct.access_token);
    acct.set_tokens(
        cc_acct.access_token,
        cc_acct.refresh_token,
        cc_acct.expires_at,
    );
    acct.keychain_blob = before;
    // The active slot is, by definition, what Claude Code is actively using —
    // adopting it means we now hold a valid grant, so clear any stale flag.
    acct.needs_relogin = false;
    if drifted {
        logging::log(&format!(
            "event=active_refresh_adopted account={email} adopted_at_prefix={cc_prefix}.. \
             note=cc_rotated_since_last_cycle"
        ));
    } else {
        logging::log(&format!(
            "event=active_refresh_in_sync account={email} at_prefix={cc_prefix}.."
        ));
    }
    ActiveRefreshOutcome::Adopted
}

/// Adopt a matching active credential into the provider account store.
pub(crate) fn refresh_provider_active_account(slug: &str) {
    let Some(provider) = providers::get(slug) else {
        return;
    };
    if !provider.supports_active_refresh() {
        return;
    }
    let result = with_state_lock(|| {
        let mut state = State::load()?;
        let Some(active_key) = state.provider_accounts(slug).and_then(|p| p.active.clone()) else {
            return Ok(()); // nothing captured / nothing active for this provider yet
        };
        let Some(acct) = state.find_provider_account(slug, &active_key).cloned() else {
            return Ok(()); // active key points at an account that's since been removed
        };
        let outcome = provider_active_refresh(slug, &acct);
        match outcome {
            ProviderRefreshOutcome::Nothing => return Ok(()),
            ProviderRefreshOutcome::Refreshed(new_blob, grant) => {
                if let Some(a) = state.find_provider_account_mut(slug, &active_key) {
                    a.secret_blob = new_blob;
                    a.access_token = grant.access.clone();
                    if let Some(r) = grant.refresh.clone() {
                        a.refresh_token = r;
                    }
                    a.expires_at = Utc::now().timestamp() + grant.expires_in_secs;
                    a.needs_relogin = false;
                }
                state.save()?;
            }
            ProviderRefreshOutcome::Failed => {
                // Already logged; leave state untouched, retry next cycle.
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        logging::log(&format!(
            "poll: {slug} active-refresh CAS state operation failed: {e:#}"
        ));
    }
}

/// Outcome of a provider-specific active-account refresh cycle, so the caller
/// can distinguish "vendor rejected the grant → flip needs_relogin" from
/// "network hiccup → retry next cycle" from "nothing to do".
enum ProviderRefreshOutcome {
    /// Fresh enough already, or CAS lost / drift adopted (state was already
    /// updated in-place by the CAS primitive); caller does nothing.
    Nothing,
    /// New blob + token grant to persist against the active account.
    Refreshed(String, TokenGrant),
    /// Anything else (network transient, 5xx, parse error, etc.) — already
    /// logged. Leave state alone, retry next cycle.
    Failed,
}

/// Read and decode the matching vendor credential; all adoption policy is shared.
fn provider_active_refresh(slug: &str, acct: &ProviderAccount) -> ProviderRefreshOutcome {
    let Some(provider) = providers::get(slug) else {
        return ProviderRefreshOutcome::Nothing;
    };
    match matching_active_blob(provider, &acct.identity_snapshot()) {
        Ok(Some(blob)) if blob != acct.secret_blob || acct.needs_relogin => {
            match provider.parse_stored_blob(&blob) {
                Ok(grant) => ProviderRefreshOutcome::Refreshed(blob, grant),
                Err(_) => ProviderRefreshOutcome::Failed,
            }
        }
        Ok(_) => ProviderRefreshOutcome::Nothing,
        Err(e) => {
            logging::log(&format!(
                "event=active_adopt_failed provider={slug} reason={e}"
            ));
            ProviderRefreshOutcome::Failed
        }
    }
}

/// Legacy Claude storage adapter. Refresh ordering and ownership policy use
/// the same transaction as provider accounts; only the state representation
/// (including millisecond expiry) differs.
pub(crate) fn ensure_claude_fresh(
    acct: &mut Account,
    skew_secs: i64,
) -> std::result::Result<bool, providers::claude::oauth::RefreshError> {
    use providers::claude::oauth::{self, RefreshError};
    if !needs_refresh(acct.expires_at, Utc::now().timestamp_millis(), skew_secs) {
        return Ok(false);
    }
    let initial = acct.clone();
    let refreshed = refresh_transaction::<_, RefreshError>(
        skew_secs,
        || {
            let mut current = initial.clone();
            let (_, active) = adopt_saved_rotation(&mut current);
            Ok(PreparedRefresh {
                expires_at_ms: current.expires_at,
                owned: active,
                value: current,
            })
        },
        |snapshot| {
            let mut candidate = snapshot.clone();
            oauth::refresh(&mut candidate)?;
            Ok(candidate)
        },
        |snapshot, response| {
            let candidate = response?;
            save_rotation(&candidate, &snapshot.refresh_token).map_err(|error| {
                RefreshError::Transient(format!(
                    "refreshed, but saving the new token failed: {error:#}"
                ))
            })?;
            Ok(candidate)
        },
    )?;
    let changed = refreshed.keychain_blob != initial.keychain_blob;
    *acct = refreshed;
    Ok(changed)
}

/// Adopt the saved tokens if another refresher already rotated this grant.
/// Returns (adopted, account is now the active one).
fn adopt_saved_rotation(acct: &mut Account) -> (bool, bool) {
    let Ok(st) = crate::store::State::load() else {
        return (false, false);
    };
    let active = st.active.as_deref() == Some(acct.key());
    let adopted = match st.find(acct.key()) {
        Some(saved) if saved.refresh_token != acct.refresh_token => {
            acct.adopt_tokens_if_newer(saved)
        }
        _ => false,
    };
    (adopted, active)
}

/// Save a rotation of the grant `sent`. Skipped (not an error) when the slot
/// moved on meanwhile: the account went active (its tokens now come only from
/// the vendor's slot), or a newer login replaced `sent` ("Renew login…").
fn save_rotation(acct: &Account, sent: &str) -> anyhow::Result<()> {
    crate::credentials::with_state_lock(|| {
        let mut st = crate::store::State::load()?;
        if st.active.as_deref() == Some(acct.key()) {
            return Ok(());
        }
        if let Some(a) = st.find_mut(acct.key()) {
            if a.refresh_token != sent {
                crate::logging::log(&format!("event=rotation_superseded account={}", acct.key()));
                return Ok(());
            }
            if a.adopt_tokens_if_newer(acct) {
                st.save()?;
            }
        }
        Ok(())
    })
}

/// Drain before changing credentials. Caller holds state and refresh locks.
/// The durable receipt prevents retries from sending a second shutdown request,
/// which some vendors interpret as force termination.
pub(crate) fn prepare_handoff(
    provider: &dyn Provider,
    status: &providers::ClientStatus,
    state: &mut State,
    key: &str,
    manual: bool,
) -> Result<bool> {
    let slug = provider.provider_id();
    let renewed_account = state
        .provider_accounts(slug)
        .and_then(|accounts| accounts.pending_switch.as_ref())
        .and_then(|pending| pending.renewed_account.clone());
    if let Some(renewed) = &renewed_account {
        anyhow::ensure!(
            renewed.key == key,
            "a renewed login is still completing its handoff"
        );
    }
    if status.running
        && state
            .provider_accounts(slug)
            .is_none_or(|accounts| accounts.pending_switch.is_none())
    {
        if let Some(account) = state.find_provider_account(slug, key) {
            if status
                .account_key
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(key))
                && status.account_id == account.identity_uuid
                && matching_active_blob(provider, &account.identity_snapshot())?.is_some()
            {
                let accounts = state.provider_accounts_mut(slug);
                // Already live in both storage and the daemon: reconcile
                // selection without rewriting tokens or interrupting work.
                accounts.pending_switch = Some(crate::store::PendingSwitch {
                    key: key.to_owned(),
                    manual,
                    from: accounts.active.clone(),
                    drain_token: None,
                    credentials_written: true,
                    renewed_account: None,
                    leave_latch: None,
                });
                state.save()?;
                return Ok(true);
            }
        }
    }
    if let Some(pending) = state
        .provider_accounts(slug)
        .and_then(|accounts| accounts.pending_switch.as_ref())
    {
        if pending.credentials_written && pending.key == key {
            if let Some(account) = state.find_provider_account(slug, key) {
                if matching_active_blob(provider, &account.identity_snapshot())?.is_some() {
                    // A replacement may already be running after a timeout or
                    // crash. Finish verification without draining it again.
                    return Ok(true);
                }
            }
        }
    }
    if !status.running {
        if let Some(token) = state
            .provider_accounts(provider.provider_id())
            .and_then(|accounts| accounts.pending_switch.as_ref())
            .and_then(|pending| pending.drain_token.as_deref())
        {
            provider.verify_handoff_stopped(token)?;
        }
        return Ok(true);
    }
    let token = status
        .handoff_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("client has no supported graceful handoff"))?;
    let accounts = state.provider_accounts_mut(slug);
    let already_requested = accounts
        .pending_switch
        .as_ref()
        .and_then(|pending| pending.drain_token.as_deref())
        == Some(token);
    let leave_latch = if manual {
        None
    } else {
        accounts
            .active
            .as_deref()
            .and_then(|from| crate::autoswap::shell::take_leave_latch(slug, from))
            .or_else(|| {
                accounts
                    .pending_switch
                    .as_ref()
                    .filter(|pending| !pending.manual && pending.from == accounts.active)
                    .and_then(|pending| pending.leave_latch.clone())
            })
    };
    accounts.pending_switch = Some(crate::store::PendingSwitch {
        key: key.to_owned(),
        manual,
        from: accounts.active.clone(),
        drain_token: Some(token.to_owned()),
        credentials_written: false,
        leave_latch,
        renewed_account,
    });
    state.save()?;
    if !already_requested || provider.handoff_request_is_idempotent() {
        provider.begin_handoff(token)?;
        logging::log(&format!(
            "event=client_drain_requested provider={slug} account={key}"
        ));
    }
    Ok(false)
}
