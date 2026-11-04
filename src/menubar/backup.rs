//! Advanced ▸ Backups: save and restore of state.json.

use super::actions::confirm;
use crate::notifications::notify;
use crate::state_ops::with_state_lock;
use crate::store::State;
use anyhow::Result;

// ---------------------------------------------------------------------------
// Settings ▸ Advanced ▸ Backups ▸ handlers (native Save…/Restore… panels)
// ---------------------------------------------------------------------------

/// "Save…" click: opens a native SAVE panel (via
/// `Platform::file_dialog().save_file(...)`) defaulting to
/// `~/Downloads/usagio-state-{timestamp}.json`, then writes a
/// REDACTED (token-free) dump of the current state to the chosen path with
/// mode 0600. This is a portable diagnostics/config snapshot — the same
/// shape `redact_state_for_dump` already produces for the "REFUSED save"
/// diagnostic dump — NOT a working credential backup. The full-fidelity
/// automatic rolling backups under `config_dir()/backups/` (written on every
/// real `state.json` save; see `write_rolling_backup`) are what "Restore…"
/// below defaults its file panel to.
pub(super) fn handle_backup_save() {
    handle_backup_save_with(crate::platform().file_dialog());
}

/// Testable core of the Save… click: takes the `FileDialog` as a parameter
/// so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead of
/// popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_save_with(dialog: &dyn crate::platform::FileDialog) {
    let default_name = format!(
        "usagio-state-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let default_dir = dirs::download_dir();
    let Some(path) = dialog.save_file(&default_name, default_dir.as_deref()) else {
        return; // user cancelled
    };
    let st = match State::load() {
        Ok(s) => s,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    let redacted = crate::store::redact_state_for_dump(&st);
    let bytes = match serde_json::to_vec_pretty(&redacted) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Save failed: {e}"));
            return;
        }
    };
    if let Err(e) = crate::store::write_private(&path, &bytes) {
        notify(&format!("Save failed: {e}"));
        return;
    }
    notify(&format!("Saved to {}", path.display()));
}

/// "Restore…" click: opens a native OPEN panel (via
/// `Platform::file_dialog().pick_file(...)`) defaulting to the automatic
/// rolling-backups directory, validates the chosen file is state-shaped,
/// warns — by NAME, not just count — before a restore would drop accounts,
/// then replaces `state.json` through the same `save_state_safe`
/// drop-protection every other write goes through. The rolling backups
/// already snapshot the pre-restore state on every ordinary save, but we
/// ALSO stash the live file to `<config_dir>/backups/` right before
/// overwriting it, so the restore itself is reversible even if the user
/// picked a very old backup.
///
/// This used to (1) stash the live state — full OAuth
/// tokens, non-redacted — to a `/tmp` sidecar, a world-writable shared
/// directory, and (2) bypass `save_state_safe`'s drop-protection guard and
/// the cross-process state lock entirely, gating the write on nothing but an
/// account *count* comparison (same-count-different-accounts sailed through
/// with zero confirmation). Both are fixed here: the stash moves under
/// `config_dir()/backups/` (owner-only permissions), and the whole
/// read-confirm-write sequence runs under `with_state_lock` with the actual
/// write routed through `store::save_state_restore` (a thin, explicitly-
/// authorized wrapper around `save_state_safe`). Note: the M2 concern
/// (`State::load()` failure silently becoming "0 accounts") is subsumed by
/// this restructure — `accounts_dropped_by(&new_state)?` bubbles the load
/// error up out of `with_state_lock`, and the `Err(e) => notify(...)` arm
/// below surfaces it, so the drop-warning path can't be silently skipped.
pub(super) fn handle_backup_restore_dialog() {
    handle_backup_restore_dialog_with(crate::platform().file_dialog());
}

/// True if any string anywhere in `v` is the `"<redacted>"` placeholder that
/// `store::redact_state_for_dump` writes (such a file holds no usable logins).
fn json_contains_redacted(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s == "<redacted>",
        serde_json::Value::Array(a) => a.iter().any(json_contains_redacted),
        serde_json::Value::Object(o) => o.values().any(json_contains_redacted),
        _ => false,
    }
}

/// Testable core of the Restore… click: takes the `FileDialog` as a
/// parameter so `#[cfg(test)]` can pass a `platform::MockFileDialog` instead
/// of popping a real native panel. See `platform::Platform::file_dialog`.
fn handle_backup_restore_dialog_with(dialog: &dyn crate::platform::FileDialog) {
    let mut default_dir = None;
    if let Ok(p) = crate::store::state_json_path() {
        let backups_dir = p.parent().map(|d| d.join("backups"));
        if let Some(dir) = backups_dir {
            if dir.is_dir() {
                default_dir = Some(dir);
            }
        }
    }
    let Some(path) = dialog.pick_file(default_dir.as_deref()) else {
        return; // user cancelled
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            notify(&format!("Restore failed reading {}: {e}", path.display()));
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            notify(&format!("Restore failed: not valid JSON ({e})"));
            return;
        }
    };
    if value.get("accounts").is_none() {
        notify("Restore failed: not a usagio state file (missing 'accounts')");
        return;
    }
    if json_contains_redacted(&value) {
        notify(
            "Restore failed: this is a redacted diagnostic dump (tokens are \"<redacted>\"), \
             not a working backup",
        );
        return;
    }
    let new_state = State::from_value(&value);

    // Ask OUTSIDE the cross-process state lock: the modal can stay open
    // indefinitely and would otherwise block every poller/CLI state write.
    let asked_about = match crate::store::accounts_dropped_by(&new_state) {
        Ok(d) => d,
        Err(e) => {
            notify(&format!("Restore failed: {e}"));
            return;
        }
    };
    if !asked_about.is_empty() && !confirm(&restore_drop_confirmation(&asked_about)) {
        return; // user declined; not an error
    }

    let outcome = with_state_lock(|lock| -> Result<bool> {
        // Recompute under the lock: if the set of dropped accounts changed
        // while the dialog was open, the confirmation no longer applies.
        let dropped = crate::store::accounts_dropped_by(&new_state)?;
        if dropped != asked_about {
            anyhow::bail!("accounts changed while the confirmation was open; try Restore again");
        }
        let dir = crate::store::config_dir()?;
        // The stash is a copy and `save_state_restore` writes atomically, so
        // a failed restore leaves the live state.json untouched; the stash is
        // kept as an extra pre-restore copy.
        crate::store::stash_pre_restore(&dir)?;
        crate::store::save_state_restore(lock, new_state.clone())?;
        Ok(true)
    });

    match outcome {
        Ok(true) => notify(&format!("Restored state from {}", path.display())),
        Ok(false) => {} // user declined the drop confirmation; no-op
        Err(e) => notify(&format!("Restore failed: {e}")),
    }
}

/// Build the confirmation question shown before a restore that would drop
/// accounts. Names the specific dropped emails (the prior wording was
/// "will drop 2 account(s)", which told the user nothing
/// about *which* accounts they were about to lose).
fn restore_drop_confirmation(dropped: &[String]) -> String {
    format!("Restoring will drop {}. Continue?", dropped.join(", "))
}

#[cfg(test)]
#[path = "backup_tests.rs"]
mod tests;
