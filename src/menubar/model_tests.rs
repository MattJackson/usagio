use super::*;
use crate::menubar::render::{menu_signature, submenu_info_rows, title_for};
use crate::menubar::test_support::{
    acct, acct_with_resets, bands, codex_state_acct, next_mtime_tag, one_section_snap,
    recovery_with, restore_test_acct, save_state_with_mtime, two_section_snap, SNAPSHOT_LOCK,
};
use crate::menubar::tray::tray_target;
use crate::providers;
use crate::providers::LoginCapture;
use crate::providers::PResult;
use crate::providers::ProviderError;
use crate::providers::TokenGrant;
use crate::providers::TokenLifecycle;
use crate::providers::{CaptureMode, Provider, SeverityBands, CLAUDE_SLUG};
use crate::store::State;
use chrono::{TimeZone, Utc};
use std::time::Duration;

#[test]
fn at_risk_excludes_locked_claude_and_keeps_usable_codex() {
    let mut claude = acct("c@x", Some(100.0), Some(100.0), true);
    claude.session_reset_at = Some(now_utc() + chrono::Duration::hours(2));
    claude.weekly_reset_at = claude.session_reset_at;
    let snap = two_section_snap(claude, Some(16.0), None);
    assert_eq!(tray_target(&snap, None).unwrap().0.provider_id, "codex");
}

#[test]
fn acct_max_pct_is_the_window_max() {
    assert_eq!(
        acct_max_pct(&acct("a", Some(40.0), Some(75.0), true)),
        Some(75.0)
    );
    assert_eq!(
        acct_max_pct(&acct("a", Some(80.0), Some(12.0), true)),
        Some(80.0)
    );
    assert_eq!(acct_max_pct(&acct("a", None, None, true)), None);
}

// The Restore drop-detection path (State::load failure must not silently
// become "0 accounts" and skip the drop-warning prompt) is tested via
// `crate::store::accounts_dropped_by` in store_tests.rs and menubar's
// Restore… test block above, not here.

#[test]
fn severity_bands_defaults() {
    let b = bands();
    assert!(severity_with(None, b).is_none());
    assert!(severity_with(Some(0.0), b).is_none());
    assert!(severity_with(Some(79.9), b).is_none());
    assert_eq!(severity_with(Some(80.0), b), Some(Severity::Amber));
    assert_eq!(severity_with(Some(94.9), b), Some(Severity::Amber));
    assert_eq!(severity_with(Some(95.0), b), Some(Severity::Red));
    assert_eq!(severity_with(Some(100.0), b), Some(Severity::Red));
}

#[test]
fn severity_bands_are_provider_driven() {
    // A provider with different thresholds (50/75) colors the same pct
    // differently — proving the hardcoded 80/95 are gone.
    let b = SeverityBands {
        amber: 50.0,
        red: 75.0,
    };
    assert_eq!(severity_with(Some(60.0), b), Some(Severity::Amber));
    assert_eq!(severity_with(Some(80.0), b), Some(Severity::Red));
}

#[test]
fn priority_order_sinks_lapsed_accounts_alphabetically() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut zed = acct("zed@x.com", Some(0.0), Some(0.0), true);
    zed.no_subscription = true;
    let mut abe = acct("abe@x.com", Some(0.0), Some(0.0), false);
    abe.no_subscription = true;
    let live = acct("live@x.com", Some(90.0), Some(90.0), false);
    let mut v = [zed, live, abe];
    v.sort_by(|a, b| account_priority_cmp(a, b, now));
    let keys: Vec<&str> = v.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["live@x.com", "abe@x.com", "zed@x.com"]);
}

#[test]
fn weekly_locked_drops_session_reset_row() {
    // Image #13: when the WEEKLY window is locked (100%), the account is fully
    // blocked, so the "5h resets in X" session row is meaningless noise — the
    // flyout should show only the "7d limit reached · resets in X" weekly row.
    let sec = ProviderSection {
        provider_id: CLAUDE_SLUG,
        display_name: "Claude",
        supports_switching: true,
        supports_usage: true,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    };
    let locked = acct("a@x.com", Some(40.0), Some(100.0), true);
    let rows = submenu_info_rows(&sec, &locked);
    assert!(
        rows.iter()
            .any(|r| r.contains("Weekly") && r.contains("locked · unlocks in")),
        "weekly-locked flyout must keep the weekly lock row: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("Session")),
        "weekly-locked flyout must drop the session reset row: {rows:?}"
    );
    // Control: an unlocked account still shows the session row.
    let healthy = acct("b@x.com", Some(40.0), Some(60.0), true);
    assert!(
        submenu_info_rows(&sec, &healthy)
            .iter()
            .any(|r| r.contains("Session")),
        "unlocked flyout must keep the session reset row"
    );
}

#[test]
fn submenu_reset_rows_are_plain_text_no_emoji() {
    // The details panel deliberately carries NO leading emoji glyphs (the
    // maintainer found them out of place). Reset rows are plain "<Label>
    // renews in X" text — assert they read that way and never lead with a
    // non-ASCII decoration.
    let a = acct("a@x.com", Some(20.0), Some(30.0), false);
    let sec = ProviderSection {
        provider_id: CLAUDE_SLUG,
        display_name: "Claude",
        supports_switching: true,
        supports_usage: true,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    };
    let rows = submenu_info_rows(&sec, &a);
    assert!(!rows.is_empty());
    for row in &rows {
        assert!(
            row.is_ascii(),
            "reset row should be plain ASCII text, no emoji: {row}"
        );
        assert!(
            row.contains("renews in") || row.contains("unlocks in"),
            "unexpected reset row text: {row}"
        );
    }
}

// (Context Ledger menu-item wiring tests removed in 0.4.2 alongside the
// submenu itself. The CLI-level ledger tests still cover the audit path.)

// --- env-override guard end-to-end -----------------------------------

#[test]
fn env_override_for_reads_shared_hook_and_row_flows_into_menu_signature() {
    // (a) With no override, the Claude section's `env_override_active`
    // stays false and the menu signature reflects "env=false".
    assert!(!env_override_for(CLAUDE_SLUG));

    let mut snap = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    assert!(!snap.sections[0].env_override_active);
    let sig_off = menu_signature(&snap);

    // (b) Toggle the env override on for the Claude slug via the shared
    // hook. `env_override_for` — the same function `build_snapshot`
    // consults to populate each section's flag — must now report true,
    // and a section rebuilt with that flag must produce a different
    // `menu_signature` (so the tray redraws and shows the disabled row).
    crate::env_override::with_env_override_hook(&[CLAUDE_SLUG], || {
        assert!(env_override_for(CLAUDE_SLUG));
        assert!(!env_override_for("codex"));
        snap.sections[0].env_override_active = env_override_for(CLAUDE_SLUG);
    });

    assert!(snap.sections[0].env_override_active);
    assert_ne!(
        sig_off,
        menu_signature(&snap),
        "env-override flag must fold into the signature so the menu redraws with the disabled row",
    );
    // The redraw signal includes the exact override state so a change
    // from active→inactive is not swallowed either.
    assert!(menu_signature(&snap).contains("env=true"));
}

#[test]
fn active_account_looks_across_sections() {
    // Two sections, active row lives in the second — active_account must
    // still find it (it's the anchor for the top header + title).
    let mut a = acct("dev@x.com", Some(10.0), Some(20.0), false);
    a.active = false;
    let mut b = acct("work@x.com", Some(50.0), Some(60.0), true);
    b.provider_id = "codex";
    let snap = Snapshot {
        sections: vec![
            ProviderSection {
                provider_id: CLAUDE_SLUG,
                display_name: "Claude",
                supports_switching: true,
                supports_usage: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![a],
            },
            ProviderSection {
                provider_id: "codex",
                display_name: "Codex",
                supports_switching: true,
                supports_usage: true,
                supports_remove: true,
                severity_bands: bands(),
                env_override_active: false,
                accounts: vec![b],
            },
        ],
        account_order: vec![(0, 0), (1, 0)],
        capture_creds: Vec::new(),
        capture_api_key: Vec::new(),
        autoswap: false,
        use_credits: false,
        threshold: 95.0,
        notification_config: crate::notifications::NotificationConfig::default(),
        tray_icon_mode: None,
    };
    let (sec, acc) = active_account(&snap).expect("active row found");
    assert_eq!(sec.provider_id, "codex");
    assert_eq!(acc.key, "work@x.com");
}

// --- capture-menu filter --------------------------------------------

/// Tiny fixture provider — supports_usage + capture_mode are the only
/// knobs `partition_capture_providers` reads, so the rest returns
/// `Unsupported` and the id/name are what the assertions look at.
struct FakeProvider {
    id: &'static str,
    name: &'static str,
    supports_usage: bool,
    capture_mode: CaptureMode,
}

impl Provider for FakeProvider {
    fn provider_id(&self) -> &'static str {
        self.id
    }
    fn display_name(&self) -> &'static str {
        self.name
    }
    fn capabilities(&self) -> providers::Capabilities {
        providers::Capabilities {
            supports_usage: self.supports_usage,
            supports_switching: false,
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: false,
            secret_backend: providers::SecretBackend::File,
            capture_mode: self.capture_mode,
        }
    }

    fn login_capture(&self) -> Option<&dyn LoginCapture> {
        Some(self)
    }

    fn token_lifecycle(&self) -> Option<&dyn TokenLifecycle> {
        Some(self)
    }
}

impl LoginCapture for FakeProvider {
    fn capture_current_login(&self) -> providers::PResult<Option<providers::CapturedAccount>> {
        Ok(None)
    }
}

impl TokenLifecycle for FakeProvider {
    fn parse_stored_blob(&self, _blob: &str) -> providers::PResult<providers::TokenGrant> {
        Err(providers::ProviderError::Unsupported)
    }
    fn patch_stored_blob(
        &self,
        _blob: &str,
        _grant: &providers::TokenGrant,
    ) -> providers::PResult<String> {
        Err(providers::ProviderError::Unsupported)
    }

    fn refresh_token(&self, refresh: &str) -> PResult<TokenGrant> {
        let _ = refresh;
        Err(ProviderError::Unsupported)
    }

    fn supports_active_refresh(&self) -> bool {
        false
    }
}

#[test]
fn partition_capture_providers_filters_stubs_and_buckets_by_capture_mode() {
    // A mixed registry: one full creds provider (Claude-shaped), one
    // full API-key provider (OpenRouter-shaped), one creds-on-disk stub
    // whose usage isn't wired yet (Cline-shaped), and one API-key-shaped
    // row that also has `supports_usage == false` (hypothetical stub —
    // still filtered because the capture filter only cares about
    // `supports_usage`).
    let claude_like = FakeProvider {
        id: "claude-like",
        name: "Claude-like",
        supports_usage: true,
        capture_mode: CaptureMode::CredsOnDisk,
    };
    let openrouter_like = FakeProvider {
        id: "openrouter-like",
        name: "OpenRouter-like",
        supports_usage: true,
        capture_mode: CaptureMode::ApiKey,
    };
    let cline_stub = FakeProvider {
        id: "cline-stub",
        name: "Cline-stub",
        supports_usage: false,
        capture_mode: CaptureMode::CredsOnDisk,
    };
    let apikey_stub = FakeProvider {
        id: "apikey-stub",
        name: "APIKey-stub",
        supports_usage: false,
        capture_mode: CaptureMode::ApiKey,
    };

    let regs: Vec<Box<dyn Provider>> = vec![
        Box::new(claude_like),
        Box::new(openrouter_like),
        Box::new(cline_stub),
        Box::new(apikey_stub),
    ];

    let (creds, api_key) = partition_capture_providers(&regs, &[]);

    let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
    let api_key_ids: Vec<&str> = api_key.iter().map(|p| p.provider_id()).collect();

    // Full creds provider is in the creds bucket.
    assert_eq!(creds_ids, vec!["claude-like"]);
    // Full API-key provider is in the api-key bucket.
    assert_eq!(api_key_ids, vec!["openrouter-like"]);

    // The stub with capture_mode == CredsOnDisk but supports_usage == false
    // is excluded from the creds list and included in NEITHER bucket.
    assert!(
        !creds_ids.contains(&"cline-stub"),
        "creds-on-disk stub must be excluded from the creds list",
    );
    assert!(
        !api_key_ids.contains(&"cline-stub"),
        "creds-on-disk stub must not leak into the api-key list either",
    );
    // Same guarantee for a stub with capture_mode == ApiKey: no usage → not shown.
    assert!(!creds_ids.contains(&"apikey-stub"));
    assert!(!api_key_ids.contains(&"apikey-stub"));
}

#[test]
fn account_priority_orders_by_soonest_weekly_reset() {
    // The canonical comparator ranks by soonest WEEKLY reset first (use
    // credits top-down so no weekly is left behind). Session reset is
    // deliberately ignored. Accounts here tie on sink/headroom, so weekly
    // reset alone decides. Regression for the user-reported bug: a
    // newly-added account (upsert appends to the tail) must still float to
    // the top when its weekly window resets sooner.
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut soonest = acct("soon@x.com", Some(50.0), Some(60.0), false);
    soonest.weekly_reset_at = Some(now + chrono::Duration::hours(6));
    let mut middle = acct("mid@x.com", Some(50.0), Some(60.0), false);
    middle.weekly_reset_at = Some(now + chrono::Duration::hours(48));
    let mut latest = acct("late@x.com", Some(50.0), Some(60.0), false);
    latest.weekly_reset_at = Some(now + chrono::Duration::hours(120));

    // Insert in REVERSE-expiration order (mirrors what upsert would
    // produce if the user added them latest-first).
    let mut accounts = [latest, middle, soonest];
    accounts.sort_by(|a, b| account_priority_cmp(a, b, now));

    let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, vec!["soon@x.com", "mid@x.com", "late@x.com"]);
}

#[test]
fn account_priority_places_no_data_accounts_last() {
    // An account without cached usage yet has `weekly_reset_at == None`.
    // The comparator treats None as "furthest in the future" so a freshly-
    // captured account (no data) sinks below one with a real, soon reset —
    // even though the no-data account has more nominal headroom.
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut with_data = acct("data@x.com", Some(10.0), Some(20.0), false);
    with_data.weekly_reset_at = Some(now + chrono::Duration::hours(6));
    let no_data = acct("nodata@x.com", None, None, false);
    assert!(no_data.weekly_reset_at.is_none(), "precondition");

    let mut accounts = [no_data, with_data];
    accounts.sort_by(|a, b| account_priority_cmp(a, b, now));
    let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, vec!["data@x.com", "nodata@x.com"]);
}

#[test]
fn account_priority_does_not_pin_active_account() {
    // "1 order only" (v0.5.18): the active account is NOT floated to the
    // top — it ranks on its own priority like everyone else. Here the
    // active account resets LATEST, so it must sort LAST despite being
    // active.
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut early = acct("early@x.com", Some(10.0), Some(20.0), false);
    early.weekly_reset_at = Some(now + chrono::Duration::hours(6));
    let mut active_late = acct("active@x.com", Some(10.0), Some(20.0), true);
    active_late.weekly_reset_at = Some(now + chrono::Duration::hours(200));
    assert!(active_late.active, "precondition: active");

    let mut accounts = [active_late, early];
    accounts.sort_by(|a, b| account_priority_cmp(a, b, now));
    let keys: Vec<&str> = accounts.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, vec!["early@x.com", "active@x.com"]);
}

// -----------------------------------------------------------------------
// flat_account_order (v0.5.2: cross-provider account order regression)
// -----------------------------------------------------------------------

fn section_with(
    provider_id: &'static str,
    display_name: &'static str,
    accounts: Vec<AcctView>,
) -> ProviderSection {
    ProviderSection {
        provider_id,
        display_name,
        supports_switching: true,
        supports_usage: true,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts,
    }
}

#[test]
fn flat_account_order_sorts_by_soonest_reset_across_providers() {
    // Given 3 accounts with resets in [+3d, +6h, +2d]: order should be
    // [+6h_first, +2d, +3d] — a single global comparator spanning ALL
    // providers, not a per-provider re-sort.
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let a_3d = acct_with_resets(
        "three-day@x.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(3)),
    );
    let a_6h = acct_with_resets(
        "six-hour@x.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::hours(6)),
    );
    let mut a_2d = acct_with_resets(
        "two-day@x.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(2)),
    );
    a_2d.provider_id = "codex";
    let sections = vec![
        section_with(CLAUDE_SLUG, "Claude", vec![a_3d, a_6h]),
        section_with("codex", "Codex", vec![a_2d]),
    ];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["six-hour@x.com", "two-day@x.com", "three-day@x.com"]
    );
}

#[test]
fn flat_account_order_sinks_locked_and_inactive_accounts() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    // Locked (session at 100%, reset still future) and NOT active — must
    // sink below a healthy account even though its own reset is sooner.
    let locked = acct_with_resets(
        "locked@x.com",
        Some(100.0),
        Some(10.0),
        false,
        Some(now + chrono::Duration::minutes(30)),
        None,
    );
    let healthy = acct_with_resets(
        "healthy@x.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(5)),
    );
    let sections = vec![section_with(CLAUDE_SLUG, "Claude", vec![locked, healthy])];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(keys, vec!["healthy@x.com", "locked@x.com"]);
}

#[test]
fn flat_account_order_sorts_locked_accounts_by_unlock_time() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    // Weekly-locked, unlocks in ~1.5d (its weekly reset is the soonest).
    let weekly_locked = acct_with_resets(
        "weekly-locked@x.com",
        Some(10.0),
        Some(100.0),
        false,
        None,
        Some(now + chrono::Duration::hours(38)),
    );
    // Session-locked, unlocks in 2h, but its weekly reset is 5d out. It is
    // usable first, so it must sort above the weekly-locked account.
    let session_locked = acct_with_resets(
        "session-locked@x.com",
        Some(100.0),
        Some(53.0),
        false,
        Some(now + chrono::Duration::hours(2)),
        Some(now + chrono::Duration::days(5)),
    );
    let sections = vec![section_with(
        CLAUDE_SLUG,
        "Claude",
        vec![weekly_locked, session_locked],
    )];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(keys, vec!["session-locked@x.com", "weekly-locked@x.com"]);
}

#[test]
fn flat_account_order_sinks_active_locked_account() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    // An active locked account must still rank below usable accounts.
    let locked_active = acct_with_resets(
        "locked-active@x.com",
        Some(10.0),
        Some(100.0),
        true,
        None,
        Some(now + chrono::Duration::minutes(30)),
    );
    let healthy = acct_with_resets(
        "healthy@x.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(5)),
    );
    let sections = vec![section_with(
        CLAUDE_SLUG,
        "Claude",
        vec![healthy, locked_active],
    )];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(keys, vec!["healthy@x.com", "locked-active@x.com"]);
}

#[test]
fn flat_account_order_ranks_active_lock_by_unlock_time() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let later = acct_with_resets(
        "dev8",
        Some(100.0),
        Some(27.0),
        true,
        Some(now + chrono::Duration::hours(4)),
        Some(now + chrono::Duration::days(6)),
    );
    let earlier = acct_with_resets(
        "dev4",
        Some(100.0),
        Some(54.0),
        false,
        Some(now + chrono::Duration::hours(2)),
        Some(now + chrono::Duration::days(7)),
    );
    let sections = vec![section_with(CLAUDE_SLUG, "Claude", vec![later, earlier])];
    assert_eq!(flat_account_order(&sections, now), vec![(0, 1), (0, 0)]);
}

#[test]
fn flat_account_order_tie_breaks_by_headroom() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    // Same reset instant: more headroom (lower max_pct) sorts first.
    let reset = Some(now + chrono::Duration::days(1));
    let more_headroom = acct_with_resets("b@x.com", Some(10.0), Some(10.0), false, None, reset);
    let less_headroom = acct_with_resets("a@x.com", Some(50.0), Some(50.0), false, None, reset);
    let sections = vec![section_with(
        CLAUDE_SLUG,
        "Claude",
        vec![less_headroom, more_headroom],
    )];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(keys, vec!["b@x.com", "a@x.com"]);
}

#[test]
fn flat_account_order_ties_break_by_key_not_input_order() {
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let reset = Some(now + chrono::Duration::days(1));
    let mk = |k: &str| acct_with_resets(k, Some(10.0), Some(10.0), false, None, reset);
    // Identical usage and reset, stored in reverse key order.
    let sections = vec![section_with(
        CLAUDE_SLUG,
        "Claude",
        vec![mk("c@x.com"), mk("a@x.com"), mk("b@x.com")],
    )];
    let order = flat_account_order(&sections, now);
    let keys: Vec<&str> = order
        .iter()
        .map(|&(si, ai)| sections[si].accounts[ai].key.as_str())
        .collect();
    assert_eq!(keys, vec!["a@x.com", "b@x.com", "c@x.com"]);
}

#[test]
fn provider_grouped_order_partitions_and_sorts_alphabetically() {
    // v0.5.3 menu redesign: two providers with two accounts each, given
    // in Codex → Claude order — grouping must partition by provider AND
    // sort provider groups alphabetically by display name so a two-
    // provider menu always reads Claude → Codex. Within-provider order
    // is preserved from `snap.account_order`.
    let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let a1 = acct_with_resets(
        "a@claude.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::hours(6)),
    );
    let a2 = acct_with_resets(
        "b@claude.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(2)),
    );
    let mut c1 = acct_with_resets(
        "a@codex.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::hours(12)),
    );
    c1.provider_id = "codex";
    let mut c2 = acct_with_resets(
        "b@codex.com",
        Some(10.0),
        Some(10.0),
        false,
        None,
        Some(now + chrono::Duration::days(1)),
    );
    c2.provider_id = "codex";
    // Sections registered in reverse-alpha order — grouped_order must
    // still return Claude before Codex.
    let sections = vec![
        // Accounts are stored latest-reset first so the expected
        // soonest-first order differs from storage order.
        section_with("codex", "Codex", vec![c2, c1]),
        section_with(CLAUDE_SLUG, "Claude", vec![a2, a1]),
    ];
    let account_order = flat_account_order(&sections, now);
    let snap = Snapshot {
        sections,
        account_order,
        capture_creds: Vec::new(),
        capture_api_key: Vec::new(),
        autoswap: false,
        use_credits: false,
        threshold: 95.0,
        notification_config: crate::notifications::NotificationConfig::default(),
        tray_icon_mode: None,
    };
    let groups = provider_grouped_order(&snap);
    assert_eq!(groups.len(), 2, "one entry per provider");
    // Claude first (alphabetical by display name), then Codex.
    assert_eq!(snap.sections[groups[0].0].display_name, "Claude");
    assert_eq!(snap.sections[groups[1].0].display_name, "Codex");
    // Within-provider account order preserved.
    assert_eq!(groups[0].1, vec![1, 0], "Claude: soonest reset first");
    assert_eq!(groups[1].1, vec![1, 0], "Codex: soonest reset first");
}

#[test]
fn capture_menu_filter_hides_zero_usage_stubs_from_creds_list() {
    // Integration-level check on top of `partition_capture_providers`:
    // the live registry produced by `providers::init()` must hide any
    // stub provider (`supports_usage == false`) from the "Capture
    // current login ▸" onboarding surface. Claude + Codex (the two full
    // providers) must be present in the creds bucket.
    providers::init();
    let (creds, _api_key) = capture_menu_providers(&[]);
    let creds_ids: Vec<&str> = creds.iter().map(|p| p.provider_id()).collect();
    assert!(creds_ids.contains(&"claude"), "claude in creds bucket");
    // Every registered stub with supports_usage == false must be filtered.
    for stub in [
        "opencode",
        "gemini-cli",
        "qwen-code",
        "copilot-cli",
        "cursor-agent",
        "amazon-q",
        "cline",
        "grok",
        "kimi",
    ] {
        assert!(
            !creds_ids.contains(&stub),
            "stub `{stub}` must be filtered out of the creds capture list",
        );
    }
}

#[test]
fn partition_capture_providers_excludes_an_already_captured_api_key_provider() {
    // Item 9 of the redesign: once an API-key provider has a captured
    // account (surfaced to `partition_capture_providers` as its id being
    // in `captured_provider_ids`), it must disappear from the "Paste API
    // key ▸" bucket — the account itself now renders in the main list.
    let openrouter_like = FakeProvider {
        id: "openrouter-like",
        name: "OpenRouter-like",
        supports_usage: true,
        capture_mode: CaptureMode::ApiKey,
    };
    let regs: Vec<Box<dyn Provider>> = vec![Box::new(openrouter_like)];

    let (_, api_key_before) = partition_capture_providers(&regs, &[]);
    assert_eq!(
        api_key_before
            .iter()
            .map(|p| p.provider_id())
            .collect::<Vec<_>>(),
        vec!["openrouter-like"],
        "not yet captured → still offered under Paste API key ▸",
    );

    let (_, api_key_after) = partition_capture_providers(&regs, &["openrouter-like"]);
    assert!(
        api_key_after.is_empty(),
        "already captured → must NOT be offered under Paste API key ▸ anymore",
    );
}

#[test]
fn acctview_from_row_orders_windows_and_flags_the_active_account() {
    let cell = |p: f64| crate::rows::Cell {
        pct: Some(p),
        resets_at: None,
    };
    let row = crate::rows::Row {
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
        email: "a@x.com".to_string(),
        session: cell(1.0),
        weekly: cell(2.0),
        opus: Some(cell(3.0)),
        error: None,
        fetched_at: None,
        reported: Default::default(),
    };
    let v = acctview_from_row(
        &row,
        &Some("a@x.com".to_string()),
        CLAUDE_SLUG,
        &["weekly", "session"],
    );
    let ids: Vec<&str> = v.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        ["weekly", "session", "opus"],
        "declared order first, undeclared windows after"
    );
    assert!(v.active);
    let v = acctview_from_row(
        &row,
        &Some("other@x.com".to_string()),
        CLAUDE_SLUG,
        &["session", "weekly"],
    );
    assert!(!v.active);
    assert_eq!(v.windows[0].id, "session");
    assert!(!acctview_from_row(&row, &None, CLAUDE_SLUG, &[]).active);
}

#[test]
fn at_risk_tray_falls_back_to_locked_active_when_nothing_else_is_active() {
    let mut a = acct("solo@x.com", Some(10.0), Some(100.0), true);
    a.weekly_reset_at = Some(now_utc() + chrono::Duration::days(2));
    let snap = one_section_snap(a);
    assert!(
        tray_target(&snap, None).is_some(),
        "a locked sole active account must still be the tray target"
    );
    assert_ne!(title_for(&snap), "—");
}

#[test]
fn recovery_offers_a_healthy_alternative_and_skips_unusable_ones() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let healthy = || acct("alt@x.com", Some(0.0), Some(0.0), false);
    let (title, detail) = recovery_with(now, healthy()).expect("recovery info");
    assert_eq!(title, "Switch");
    assert_eq!(detail, "alt@x.com · Usage available");

    // Every kind of unusable alternative is passed over → the active
    // account's own lock countdown is what the tray shows.
    type Spoil = fn(&mut AcctView);
    let kinds: [(&str, Spoil); 4] = [
        ("needs re-login", |a| a.needs_relogin = true),
        ("refresh error", |a| a.error = Some("boom".into())),
        ("lapsed plan", |a| a.no_subscription = true),
        ("no data", |a| a.has_data = false),
    ];
    for (what, mutate) in kinds {
        let mut alt = healthy();
        mutate(&mut alt);
        let (title, _) = recovery_with(now, alt).expect("recovery info");
        assert_eq!(title, "🔒 1h 0m", "{what} alternative must be skipped");
    }
}

#[test]
fn recovery_does_not_offer_an_account_sitting_exactly_at_the_lock_threshold() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    // 99.5% with a reset that already passed and a fresh reading after it:
    // not stale, not locked, but still effectively exhausted.
    let mut edge = acct("edge@x.com", Some(99.5), Some(0.0), false);
    edge.session_reset_at = Some(now - chrono::Duration::minutes(10));
    edge.fetched_at = Some(now);
    let (title, _) = recovery_with(now, edge).expect("recovery info");
    assert_eq!(title, "🔒 1h 0m");
}

// --- cached state / snapshot pipeline ----------------------------------

#[test]
fn cached_state_rereads_only_when_the_file_changes() {
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let tag = next_mtime_tag();
    let mut st = State {
        trigger_pct: Some(90.0),
        ..State::default()
    };
    save_state_with_mtime(&st, tag);
    assert_eq!(
        cached_state().trigger_pct,
        Some(90.0),
        "first read loads the file"
    );

    // New content, new mtime → re-read.
    st.trigger_pct = Some(80.0);
    let tag2 = next_mtime_tag();
    save_state_with_mtime(&st, tag2);
    assert_eq!(
        cached_state().trigger_pct,
        Some(80.0),
        "changed mtime reloads"
    );

    // The same file rewritten in place (same inode, length and mtime) → the
    // cache is trusted (no re-parse every 0.75s tick).
    let path = crate::store::config_dir().unwrap().join("state.json");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("80.0", "70.0");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut &f, text.as_bytes()).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + tag2))
        .unwrap();
    drop(f);
    assert_eq!(
        cached_state().trigger_pct,
        Some(80.0),
        "unchanged mtime hits the cache"
    );
}

// The inode in the cache key is unix-only; elsewhere two same-length
// saves at one mtime are indistinguishable.
#[cfg(unix)]
#[test]
fn cached_state_reloads_a_save_that_lands_on_the_same_mtime() {
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let tag = next_mtime_tag();
    let mut st = State {
        trigger_pct: Some(90.0),
        ..State::default()
    };
    save_state_with_mtime(&st, tag);
    assert_eq!(cached_state().trigger_pct, Some(90.0));
    // A second atomic save within one coarse timestamp tick.
    st.trigger_pct = Some(85.5);
    save_state_with_mtime(&st, tag);
    assert_eq!(
        cached_state().trigger_pct,
        Some(85.5),
        "a new save must not be hidden behind an equal mtime"
    );
}

#[test]
fn build_snapshot_omits_sections_for_providers_with_zero_accounts() {
    // Only Claude has accounts: every other registered provider (stubs
    // included) must not get a section, or Settings > Tray Icon lists them
    // all and the empty-state rows never show.
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    st.accounts.push(restore_test_acct("only@x.com"));
    st.active = Some("only@x.com".to_string());
    save_state_with_mtime(&st, next_mtime_tag());

    let snap = build_snapshot();
    let ids: Vec<String> = snap
        .sections
        .iter()
        .map(|s| s.provider_id.to_string())
        .collect();
    assert_eq!(ids, [CLAUDE_SLUG]);
}

#[test]
fn build_snapshot_buckets_accounts_by_provider_with_their_own_active() {
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    st.accounts.push(restore_test_acct("a@x.com"));
    st.accounts.push(restore_test_acct("b@x.com"));
    st.active = Some("a@x.com".to_string());
    st.upsert_provider_account("codex", codex_state_acct("cx@x.com"));
    st.upsert_provider_account("codex", codex_state_acct("cy@x.com"));
    st.provider_accounts_mut("codex").active = Some("cy@x.com".to_string());
    st.trigger_pct = Some(88.0);
    st.tray_icon_mode = Some("codex".to_string());
    save_state_with_mtime(&st, next_mtime_tag());

    let snap = build_snapshot();
    assert!(snap.autoswap);
    assert_eq!(snap.threshold, 88.0);
    assert_eq!(snap.tray_icon_mode.as_deref(), Some("codex"));
    let claude = snap
        .sections
        .iter()
        .find(|s| s.provider_id == CLAUDE_SLUG)
        .unwrap();
    let codex = snap
        .sections
        .iter()
        .find(|s| s.provider_id == "codex")
        .unwrap();
    let keys = |s: &ProviderSection| {
        let mut k: Vec<String> = s.accounts.iter().map(|a| a.key.clone()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(claude), ["a@x.com", "b@x.com"]);
    assert_eq!(keys(codex), ["cx@x.com", "cy@x.com"]);
    let active = |s: &ProviderSection| {
        s.accounts
            .iter()
            .filter(|a| a.active)
            .map(|a| a.key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        active(claude),
        ["a@x.com"],
        "Claude's active comes from state.active"
    );
    assert_eq!(
        active(codex),
        ["cy@x.com"],
        "Codex's active comes from its own slot"
    );
    assert_eq!(snap.account_order.len(), 4);

    // Auto-swap switched off in Settings is reflected.
    st.autoswap_disabled = true;
    save_state_with_mtime(&st, next_mtime_tag());
    assert!(!build_snapshot().autoswap);
}

#[test]
fn all_locked_fallback_skips_an_indefinitely_blocked_account() {
    let mut claude = acct("c@x", Some(100.0), Some(100.0), true);
    claude.session_reset_at = None; // maxed, reset unknown: indefinite
    claude.weekly_reset_at = Some(now_utc() + chrono::Duration::hours(1));
    let mut snap = two_section_snap(claude, Some(10.0), None);
    let codex = &mut snap.sections[1].accounts[0];
    codex.windows[1].pct = Some(100.0);
    codex.weekly_reset_at = Some(now_utc() + chrono::Duration::hours(3));
    let (sec, _) = tray_target(&snap, None).unwrap();
    assert_eq!(sec.provider_id, "codex", "Claude never really unlocks");
}

#[test]
fn at_risk_is_checked_when_the_pinned_provider_has_no_section() {
    let mut snap = two_section_snap(acct("c@x", Some(1.0), None, true), None, None);
    assert!(at_risk_checked(&snap));
    snap.tray_icon_mode = Some("codex".into());
    assert!(!at_risk_checked(&snap), "pinned and present");
    snap.tray_icon_mode = Some("gemini-cli".into());
    assert!(
        at_risk_checked(&snap),
        "pinned but gone: behaves as at-risk"
    );
}
