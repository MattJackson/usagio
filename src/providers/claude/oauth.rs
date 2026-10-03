//! OAuth refresh-token grant. We only ever refresh here; new accounts are
//! onboarded by capturing a real `claude` login from the keychain.

use serde::Deserialize;

use super::config;
use crate::store::Account;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// OPTIONAL in a refresh-grant response (RFC 6749 §6): when the server does
    /// not rotate the refresh token it may omit this, and the old one stays valid.
    #[serde(default)]
    refresh_token: Option<String>,
    /// Lifetime of the access token, in seconds.
    expires_in: i64,
    /// Lifetime of the refresh token, in seconds. Claude Code turns this into
    /// the keychain's `refreshTokenExpiresAt` and warns "Your login expires in
    /// N days" off it, so every rotation must carry it forward.
    #[serde(default)]
    refresh_token_expires_in: Option<i64>,
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Refresh outcomes distinguished so callers can react correctly.
///
/// The critical signal is `InvalidGrant`: Anthropic's OAuth server rotates
/// the refresh token on every successful refresh and single-uses the old
/// one, so a stale copy — most often produced when the user runs the real
/// `claude` CLI, which refreshes and rotates behind our back — comes back
/// as HTTP 400 invalid_grant and is permanently dead for that grant family.
/// The account needs a fresh `/login`; no amount of retrying will help.
#[derive(Debug)]
pub enum RefreshError {
    /// Anthropic said 400 (or the account has no refresh token to send).
    /// The stored refresh token is now permanently dead; user must re-login.
    InvalidGrant,
    /// Anthropic said 429. Back off; retry later.
    RateLimited,
    /// Network / 5xx / parse error. Retry with backoff is fine.
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::InvalidGrant => write!(f, "refresh token rejected (invalid_grant)"),
            RefreshError::RateLimited => write!(f, "token endpoint rate-limited (429)"),
            RefreshError::Transient(s) => write!(f, "transient: {s}"),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Refresh an account's access token in place, keeping the keychain blob in
/// sync. Returns true (it always changes the tokens on success).
///
/// Errors are typed so the caller can decide policy: `InvalidGrant` means the
/// account needs a fresh `/login` (flag `needs_relogin` in state); anything
/// else is worth another try on the next tick.
///
/// Raw grant only — it does not persist. Go through [`ensure_fresh`], which
/// serializes refreshers and saves the rotation.
fn refresh(acct: &mut Account) -> Result<bool, RefreshError> {
    if acct.refresh_token.is_empty() {
        // Nothing to send. Treat identically to a rejected grant so the
        // account gets flagged and skipped consistently.
        return Err(RefreshError::InvalidGrant);
    }
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": acct.refresh_token,
        "client_id": config::CLIENT_ID,
    });
    let tok = post_token(&body)?;
    let expires_at = now_millis().saturating_add(tok.expires_in.saturating_mul(1000));
    // Keep the existing refresh token if the server didn't rotate one.
    let refresh_token = tok
        .refresh_token
        .unwrap_or_else(|| acct.refresh_token.clone());
    acct.set_tokens(tok.access_token, refresh_token, expires_at);
    if let Some(secs) = tok.refresh_token_expires_in {
        acct.set_refresh_token_expires_at(now_millis().saturating_add(secs.saturating_mul(1000)));
    }
    crate::logging::log(&format!(
        "event=token_refreshed account={} rt_expires_in={}",
        acct.key(),
        tok.refresh_token_expires_in
            .map_or_else(|| "none".to_string(), |s| format!("{s}s"))
    ));
    Ok(true)
}

/// Refresh only if the token expires within `skew_secs`. The ONE place a
/// refresh grant is spent and its rotation saved to state.json.
///
/// Anthropic single-uses refresh tokens, so two refreshers holding the same
/// one (the poll, the credential watcher, `usagio token` in another process)
/// can't both win: the loser gets invalid_grant and used to flag a healthy
/// account for re-login. So under a cross-process lock we first adopt any
/// rotation someone else already saved, and only POST if that's still stale —
/// then save before releasing the lock, so the next refresher sees it.
pub fn ensure_fresh(acct: &mut Account, skew_secs: i64) -> Result<bool, RefreshError> {
    if !needs_refresh(acct.expires_at, now_millis(), skew_secs) {
        return Ok(false);
    }
    let _lock = refresh_lock();
    let (adopted, now_active) = adopt_saved_rotation(acct);
    // A switch made it active since the caller looked: its token is now the
    // vendor CLI's to rotate, never ours.
    if now_active || !needs_refresh(acct.expires_at, now_millis(), skew_secs) {
        return Ok(adopted);
    }
    let sent = acct.refresh_token.clone();
    refresh(acct)?;
    save_rotation(acct, &sent).map_err(|e| {
        RefreshError::Transient(format!("refreshed, but saving the new token failed: {e:#}"))
    })?;
    Ok(true)
}

/// Exclusive `refresh.lock` in the config dir, held until dropped. Best
/// effort: if it can't be taken we refresh unserialized, as before. Always
/// taken BEFORE the state lock (a switch's commit takes it too, so it sees a
/// rotation that was in flight).
pub(crate) fn refresh_lock() -> Option<std::fs::File> {
    use fs2::FileExt;
    let dir = crate::store::config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("refresh.lock"))
        .ok()?;
    f.lock_exclusive().ok()?;
    Some(f)
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

/// Whether a token expiring at `expires_at` (unix millis) is within `skew_secs`
/// of `now_millis`. Saturating so a corrupt `expires_at` (e.g. near i64::MIN from
/// a malformed state.json) reads as "expired" instead of overflowing — a plain
/// subtraction would panic in debug and wrap to "fresh" in release. Pure, tested.
fn needs_refresh(expires_at: i64, now_millis: i64, skew_secs: i64) -> bool {
    expires_at.saturating_sub(now_millis) <= skew_secs.saturating_mul(1000)
}

#[cfg(test)]
thread_local! {
    /// Test-only override for the OAuth token endpoint, so integration tests
    /// (`main_tests.rs::refresh_usage_cache_does_not_touch_the_active_account`)
    /// can point `post_token` at an in-process mock HTTP server instead of the
    /// real Anthropic endpoint, and count exactly how many POSTs land.
    static TOKEN_URL_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Install (or clear) the current thread's token-URL override. Test-only.
#[cfg(test)]
pub(crate) fn set_token_url_override(url: Option<&str>) {
    TOKEN_URL_OVERRIDE.with(|c| *c.borrow_mut() = url.map(String::from));
}

#[cfg(test)]
fn token_url() -> String {
    TOKEN_URL_OVERRIDE
        .with(|c| c.borrow().clone())
        .unwrap_or_else(|| config::TOKEN_URL.to_string())
}

#[cfg(not(test))]
fn token_url() -> String {
    config::TOKEN_URL.to_string()
}

fn post_token(body: &serde_json::Value) -> Result<TokenResponse, RefreshError> {
    let resp = super::http_agent()
        .post(&token_url())
        .set("Content-Type", "application/json")
        .set("anthropic-beta", config::OAUTH_BETA)
        .send_json(body.clone());
    match resp {
        Ok(r) => r
            .into_json::<TokenResponse>()
            .map_err(|e| RefreshError::Transient(format!("parsing token response: {e}"))),
        Err(ureq::Error::Status(400, _)) => Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(401, _)) => Err(RefreshError::InvalidGrant),
        Err(ureq::Error::Status(429, _)) => Err(RefreshError::RateLimited),
        Err(ureq::Error::Status(code, _)) => {
            // Don't include the raw response body — it can echo submitted request
            // data and ends up in the debug log. The status code is enough.
            Err(RefreshError::Transient(format!(
                "token endpoint returned HTTP {code}"
            )))
        }
        Err(e) => Err(RefreshError::Transient(format!(
            "token request failed: {e}"
        ))),
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
