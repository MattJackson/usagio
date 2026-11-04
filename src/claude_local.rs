//! Claude Code's local login: the keychain entry and the `~/.claude.json` identity.

use anyhow::{anyhow, Context, Result};

use crate::{providers, watchdog};

// ---------------------------------------------------------------------------
// Keychain helpers — delegate to the platform SecretStore
// ---------------------------------------------------------------------------

// Only the keychain-account unit test uses this now; the live slot logic is
// `providers::claude::active_slot_{read,write}`.
#[cfg(test)]
pub(crate) fn keychain_account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "claude".to_string())
}

// NOTE: on macOS the SecretStore impl uses the `security` CLI rather than the
// Security.framework (`security-framework`) API on purpose. SecItem access
// from this unsigned, brew-installed binary makes macOS prompt on every
// launch (an unsigned binary has no stable identity for "Always Allow" to pin
// to). The CLI path doesn't prompt. The only downside is the write blob
// appears in `security`'s argv, which is LOW risk under this tool's
// single-user threat model. TODO: switch back to security-framework once we
// ship a code-signed (Developer ID) build, where "Always Allow" persists.
pub(crate) fn keychain_read() -> Option<String> {
    providers::claude::active_slot_read().ok().flatten()
}

pub(crate) fn keychain_write(blob: &str) -> Result<()> {
    let result = providers::claude::active_slot_write(blob).map_err(|e| anyhow!("{e}"));
    // Feed the keychain-write health signal the watchdog watches: a run of
    // consecutive failures is the fingerprint of the fd-exhaustion EMFILE that
    // silently broke account switching (see `src/watchdog.rs`). File writes
    // hit EMFILE the same way. A success resets the counter.
    watchdog::record_keychain_result(result.is_ok());
    result
}

pub(crate) fn claude_json_path() -> Result<std::path::PathBuf> {
    // Same resolution as the Claude provider (honours CLAUDE_CONFIG_DIR).
    crate::providers::claude::claude_json_path().context("HOME/USERPROFILE is not set")
}

#[cfg(test)]
thread_local! {
    /// Calls to [`read_claude_identity`] on this thread (each one parses the
    /// whole of `~/.claude.json`), so tests can pin "parse once".
    pub(crate) static CLAUDE_IDENTITY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read the current `oauthAccount` + `userID` from `~/.claude.json`.
pub(crate) fn read_claude_identity() -> (Option<serde_json::Value>, Option<String>) {
    #[cfg(test)]
    CLAUDE_IDENTITY_READS.with(|n| n.set(n.get() + 1));
    let Ok(path) = claude_json_path() else {
        return (None, None);
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };
    let oauth = v.get("oauthAccount").cloned();
    let uid = v.get("userID").and_then(|u| u.as_str()).map(String::from);
    (oauth, uid)
}

/// Raw bytes of ~/.claude.json plus its permission mode, for rollback.
pub(crate) fn read_claude_json_raw() -> Option<(Vec<u8>, u32)> {
    let path = claude_json_path().ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let mode = claude_json_mode(&path);
    Some((bytes, mode))
}

/// The file's permission bits (0o600 fallback so a rollback never loosens perms).
#[cfg(unix)]
pub(crate) fn claude_json_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o600)
}

#[cfg(not(unix))]
pub(crate) fn claude_json_mode(_path: &std::path::Path) -> u32 {
    0o600
}

/// Patch `~/.claude.json` so its active identity is this account: set
/// `oauthAccount`, set or (when unknown) REMOVE `userID`, and drop the stale
/// `cachedUsageUtilization`. Atomic, preserves the file mode, cleans the tmp on
/// failure.
pub(crate) fn write_claude_identity(
    oauth_account: &serde_json::Value,
    user_id: Option<&str>,
) -> Result<()> {
    write_claude_identity_with(oauth_account, user_id, || {})
}

/// Read-modify-write `~/.claude.json`. The Claude CLI rewrites this file
/// continually, so re-read just before committing and redo the patch if the
/// bytes moved (bounded). `before_commit` is a test seam.
pub(crate) fn write_claude_identity_with(
    oauth_account: &serde_json::Value,
    user_id: Option<&str>,
    before_commit: impl FnMut(),
) -> Result<()> {
    // One implementation (torn-read retry, missing-file message) shared with
    // the Claude provider.
    crate::providers::claude::write_claude_identity_with(oauth_account, user_id, before_commit)
        .map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
#[path = "claude_local_tests.rs"]
mod tests;
