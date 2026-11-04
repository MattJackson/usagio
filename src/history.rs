//! The JSONL history log behind `usagio report`.

use anyhow::Result;
use chrono::Utc;

use crate::rows::Row;
use crate::{logging, store};

// ---------------------------------------------------------------------------
// History logging + reporting
// ---------------------------------------------------------------------------

pub(crate) fn history_path() -> Result<std::path::PathBuf> {
    Ok(store::config_dir()?.join("history.jsonl"))
}

pub(crate) fn log_event(v: &serde_json::Value) {
    use std::io::Write;
    let path = match history_path() {
        Ok(p) => p,
        Err(e) => {
            log_history_failure(&format!("history path unavailable: {e:#}"));
            return;
        }
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log_history_failure(&format!("create {} failed: {e}", dir.display()));
        }
    }
    // Rotate if it has grown too large (keep one previous generation), using the
    // same threshold as the debug log so the policy can't drift.
    logging::rotate_if_large(&path, logging::MAX_BYTES);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{v}") {
                log_history_failure(&format!("write {} failed: {e}", path.display()));
            }
        }
        Err(e) => log_history_failure(&format!("open {} failed: {e}", path.display())),
    }
}

/// Log a history.jsonl write failure, at most once per process so a
/// persistent fault doesn't flood the debug log every poll.
fn log_history_failure(msg: &str) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        logging::log(&format!(
            "history: {msg} (further history errors suppressed)"
        ));
    }
}

pub(crate) fn append_history(rows: &[Row], active: Option<&str>) {
    let ts = Utc::now().timestamp();
    for r in rows {
        if !r.has_data() {
            continue;
        }
        log_event(&serde_json::json!({
            "ts": ts,
            "account": r.email,
            "active": active == Some(r.email.as_str()),
            "session": r.session.pct,
            "weekly": r.weekly.pct,
        }));
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
