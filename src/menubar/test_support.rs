//! Shared menu-bar test fixtures.

use crate::menubar::model::{AcctView, ProviderSection, RegisteredProvider, Snapshot, WindowView};
use crate::menubar::render::recovery_status;
use crate::providers::trait_def::{ReportedUsage, ResetGrant};
use crate::providers::{CaptureMode, SeverityBands, CLAUDE_SLUG};
use crate::store::State;
use crate::watch::cadence::TRIGGER_PCT;
use chrono::{DateTime, TimeZone, Utc};
use std::cell::Cell as StdCell;
use std::time::Duration;

pub(super) fn bands() -> SeverityBands {
    SeverityBands {
        amber: 80.0,
        red: 95.0,
    }
}

thread_local! {
    /// Wall-clock hook honored by `menubar::now_utc` under `#[cfg(test)]`.
    /// Fixed to the epoch by default so a test that doesn't care about
    /// countdown transitions gets a stable "not locked" reading (both
    /// `session_reset_at` and `weekly_reset_at` are None on the default
    /// fixture, and `is_blocking` short-circuits on `None`).
    pub(super) static TEST_NOW: StdCell<DateTime<Utc>> =
        StdCell::new(Utc.timestamp_opt(0, 0).unwrap());
}

/// Read the current test-frozen wall clock. Called from `now_utc()` under
/// `#[cfg(test)]` — see `now_utc`.
pub(super) fn test_now() -> DateTime<Utc> {
    TEST_NOW.with(|c| c.get())
}

/// Run `f` with the test wall clock pinned to `t`, restoring it after.
pub(super) fn with_now<R>(t: DateTime<Utc>, f: impl FnOnce() -> R) -> R {
    let prev = TEST_NOW.with(|c| c.replace(t));
    let out = f();
    TEST_NOW.with(|c| c.set(prev));
    out
}

pub(super) fn acct(
    email: &str,
    session: Option<f64>,
    weekly: Option<f64>,
    active: bool,
) -> AcctView {
    AcctView {
        provider_id: CLAUDE_SLUG,
        key: email.to_string(),
        display: email.to_string(),
        windows: vec![
            WindowView {
                id: "session".into(),
                label: "5h".into(),
                pct: session,
                reset: "3h".into(),
            },
            WindowView {
                id: "weekly".into(),
                label: "7d".into(),
                pct: weekly,
                reset: "2d".into(),
            },
        ],
        updated: "1m ago".into(),
        active,
        has_data: true,
        // Default fixture: no reset instants → `compute_display` returns
        // Usage regardless of pct. Tests that exercise lock transitions
        // set these explicitly.
        session_reset_at: None,
        weekly_reset_at: None,
        // `None` → `is_stale_after_reset` short-circuits on `fetched_at?`,
        // so the default fixture never accidentally lands in
        // `StaleAfterReset`. Tests exercising that state set it directly.
        fetched_at: None,
        // Healthy fixture: no auth trouble. Tests exercising the re-login /
        // error trailing set these directly.
        needs_relogin: false,
        error: None,
        no_subscription: false,
        plan: None,
        reported: ReportedUsage::default(),
        login_expires_at: None,
        renewable: false,
        rate: Default::default(),
        trigger: TRIGGER_PCT,
    }
}

pub(super) fn one_section_snap(a: AcctView) -> Snapshot {
    Snapshot {
        sections: vec![ProviderSection {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            supports_switching: true,
            supports_usage: true,
            supports_remove: true,
            severity_bands: bands(),
            env_override_active: false,
            accounts: vec![a],
        }],
        account_order: vec![(0, 0)],
        capture_creds: vec![RegisteredProvider {
            provider_id: CLAUDE_SLUG,
            display_name: "Claude",
            capture_mode: CaptureMode::CredsOnDisk,
        }],
        capture_api_key: Vec::new(),
        autoswap: false,
        use_credits: false,
        threshold: 95.0,
        notification_config: crate::notifications::NotificationConfig::default(),
        tray_icon_mode: None,
    }
}

/// Build a two-provider snapshot for tray-target tests: a Claude section
/// and a Codex section, each with a single active account at the given
/// max %. `provider_id`/`display_name` are leaked `&'static` fixtures.
pub(super) fn two_section_snap(
    claude: AcctView,
    codex_pct: Option<f64>,
    tray_icon_mode: Option<String>,
) -> Snapshot {
    let mut codex = acct("cx@x", codex_pct, None, true);
    codex.provider_id = "codex";
    let mk = |id: &'static str, name: &'static str, a: AcctView| ProviderSection {
        provider_id: id,
        display_name: name,
        supports_switching: true,
        supports_usage: true,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![a],
    };
    Snapshot {
        sections: vec![
            mk(CLAUDE_SLUG, "Claude", claude),
            mk("codex", "Codex", codex),
        ],
        account_order: vec![(0, 0), (1, 0)],
        capture_creds: Vec::new(),
        capture_api_key: Vec::new(),
        autoswap: false,
        use_credits: false,
        threshold: 95.0,
        notification_config: crate::notifications::NotificationConfig::default(),
        tray_icon_mode,
    }
}

pub(super) fn claude_section() -> ProviderSection {
    ProviderSection {
        provider_id: CLAUDE_SLUG,
        display_name: "Claude",
        supports_switching: true,
        supports_usage: true,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    }
}

pub(super) fn reset_fixture(usable_now: bool) -> ResetGrant {
    ResetGrant {
        id: "gift".into(),
        resets_left: 1,
        usable_now,
        ..ResetGrant::default()
    }
}

// -----------------------------------------------------------------------
// Menu-redesign / countdown / icons tests
// -----------------------------------------------------------------------

/// Build an account with explicit reset instants so the locked-vs-usage
/// transition can be pinned in tests.
pub(super) fn acct_with_resets(
    email: &str,
    session: Option<f64>,
    weekly: Option<f64>,
    active: bool,
    session_reset_at: Option<DateTime<Utc>>,
    weekly_reset_at: Option<DateTime<Utc>>,
) -> AcctView {
    let mut a = acct(email, session, weekly, active);
    a.session_reset_at = session_reset_at;
    a.weekly_reset_at = weekly_reset_at;
    a
}

/// Minimal restore-test account.
pub(super) fn restore_test_acct(email: &str) -> crate::store::Account {
    let blob = serde_json::json!({
        "claudeAiOauth": { "accessToken": "at", "refreshToken": "rt", "expiresAt": 0 }
    })
    .to_string();
    let mut a = crate::store::Account::from_keychain_blob(&blob).unwrap();
    a.email = Some(email.to_string());
    a
}

// -----------------------------------------------------------------------
// Behaviour pins for the tray theme override, labels, row spans, the
// cached-state snapshot pipeline, the poll-request wake-up, the recovery
// title/tooltip and the click-routing table.
// -----------------------------------------------------------------------

/// Serializes tests that go through `cached_state` (a process-wide mtime
/// cache): two tests with different config dirs would otherwise evict each
/// other's cached entry mid-assertion.
pub(super) static SNAPSHOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that touch the process-wide `REFRESH_IN_FLIGHT` flag.
pub(super) static REFRESH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that touch the process-wide `POLL_REQUEST` flag.
pub(super) static POLL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Write `st` to the scoped `state.json` and pin its mtime to a value unique
/// per `tag`, so the `cached_state` mtime cache can neither miss a rewrite
/// (coarse filesystem timestamps) nor mistake another test's file for ours.
pub(super) fn save_state_with_mtime(st: &State, tag: u64) {
    st.save().unwrap();
    let path = crate::store::config_dir().unwrap().join("state.json");
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + tag))
        .unwrap();
}

/// A process-unique mtime tag for [`save_state_with_mtime`].
pub(super) fn next_mtime_tag() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    N.fetch_add(10, std::sync::atomic::Ordering::SeqCst)
}

pub(super) fn codex_state_acct(key: &str) -> crate::store::ProviderAccount {
    serde_json::from_value(serde_json::json!({
        "key": key,
        "secret_blob": "{}",
        "access_token": "at",
    }))
    .unwrap()
}

// --- recovery status (tray title / tooltip) ----------------------------

pub(super) fn locked_for(email: &str, now: DateTime<Utc>, active: bool) -> AcctView {
    let mut a = acct(email, Some(100.0), Some(10.0), active);
    a.session_reset_at = Some(now + chrono::Duration::minutes(60));
    a.fetched_at = Some(now);
    a
}

pub(super) fn recovery_with(now: DateTime<Utc>, alt: AcctView) -> Option<(String, String)> {
    let active = locked_for("active@x.com", now, true);
    let mut sec = claude_section();
    sec.accounts = vec![locked_for("active@x.com", now, true), alt];
    recovery_status(&sec, &active, now)
}

// --- binding-window display (auto-swap v2, design §4) -------------------

pub(super) fn rated(mut a: AcctView, rs: Option<f64>, rw: Option<f64>) -> AcctView {
    a.rate = crate::autoswap::AccountRate {
        session: rs,
        weekly: rw,
        ..Default::default()
    };
    a
}
