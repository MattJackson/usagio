//! State-file helpers: the cross-process lock, change-only saves, and account removal.

use anyhow::{bail, Result};

use crate::credentials;
use crate::providers::CLAUDE_SLUG;
use crate::store::State;

/// Save the merged poll state only when it differs from what was loaded, so
/// idle cycles don't rewrite state.json and rotate the rolling backups.
pub(crate) fn save_if_changed(st: &State, before: &[u8]) -> Result<bool> {
    if serde_json::to_vec(st).ok().as_deref() == Some(before) {
        return Ok(false);
    }
    st.save()?;
    Ok(true)
}

/// Remove a captured account from a non-Claude provider's state v2 slot
/// (`State::providers[slug]`). Mirrors `remove_account` below for Claude —
/// added alongside state v2 so the "Remove…" row that now appears for a
/// provider whose accounts render in the menu (state v2 makes that possible
/// for the first time) doesn't dead-end.
pub(crate) fn remove_provider_account_generic(slug: &str, key: &str) -> Result<()> {
    with_state_lock(|| {
        let mut state = State::load()?;
        if !state.remove_provider_account(slug, key) {
            bail!("no {slug} account matches '{key}'");
        }
        state.save()
    })
}

/// Remove an account by email (used by the CLI `rm` and the menu bar).
pub(crate) fn remove_account(email: &str) -> Result<()> {
    with_state_lock(|| {
        let mut state = State::load()?;
        state.remove(email);
        if state
            .active
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(email))
        {
            state.active = None;
        }
        state.save()
    })
}

/// The key of `slug`'s active account, if any.
pub(crate) fn provider_active(state: &State, slug: &str) -> Option<String> {
    if slug == CLAUDE_SLUG {
        state.active.clone()
    } else {
        state.provider_accounts(slug).and_then(|p| p.active.clone())
    }
}

// ---------------------------------------------------------------------------
// Cross-process state lock
// ---------------------------------------------------------------------------

/// Run `f` holding an exclusive advisory lock on ~/.config/usagio/lock,
/// serializing state read-modify-write across processes (the daemon poll and
/// concurrent CLI/menu commands). The lock is fd-scoped, so the kernel releases
/// it if the holder dies. Reentrant on the current thread, so provider-
/// implemented callbacks (e.g. `absorb_credential`) can safely take the lock
/// again from inside another `with_state_lock` frame without self-deadlocking
/// (see credentials::with_state_lock for the depth-tracking rationale). Do NOT
/// do network I/O inside `f`.
pub(crate) fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    credentials::with_state_lock(f)
}
