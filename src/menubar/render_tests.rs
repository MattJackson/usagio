use super::*;
use crate::menubar::binding::binding_span;
use crate::menubar::cross_platform;
use crate::menubar::model::{now_utc, summary_pcts, ProviderSection, Severity, WindowView};
use crate::menubar::test_support::{
    acct, acct_with_resets, bands, claude_section, locked_for, next_mtime_tag, one_section_snap,
    rated, recovery_with, reset_fixture, save_state_with_mtime, two_section_snap, with_now,
    SNAPSHOT_LOCK,
};
use crate::providers::trait_def::{
    Credits, Money, ReportedUsage, ResetGrant, UsageShare, UsageWindow,
};
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use chrono::{TimeZone, Utc};

// For ASCII rows, UTF-16 offsets equal byte offsets, so we can slice the
// plain title to prove each colored span lands exactly on its `NN%`.
fn span_text(plain: &str, off: usize, len: usize) -> String {
    plain.chars().skip(off).take(len).collect()
}

#[test]
fn main_row_colors_land_on_percentages() {
    let a = acct("you@work.com", Some(82.0), Some(96.0), true);
    let r = main_row("Claude", &a, bands());
    assert_eq!(r.plain, "Claude    you@work.com\t82% / 96%");
    assert!(r.bold, "active account is bold");
    assert_eq!(
        r.tab_x_kind,
        Some(TabX::MenuRight),
        "trailing run is right-aligned"
    );
    assert_eq!(r.colors.len(), 2);
    let (so, sl, ss) = r.colors[0];
    assert_eq!(span_text(&r.plain, so, sl), "82%");
    assert_eq!(ss, Severity::Amber);
    let (wo, wl, ws) = r.colors[1];
    assert_eq!(span_text(&r.plain, wo, wl), "96%");
    assert_eq!(ws, Severity::Red);
}

#[test]
fn main_row_low_usage_has_no_colors_and_no_bold_when_inactive() {
    let a = acct("dev@side.com", Some(3.0), Some(9.0), false);
    let r = main_row("Claude", &a, bands());
    assert!(!r.bold);
    assert!(r.colors.is_empty());
}

#[test]
fn main_row_offsets_hold_for_unicode_email() {
    let a = acct("café@x.com", Some(99.0), None, false);
    let r = main_row("Claude", &a, bands());
    let (off, len, _) = r.colors[0];
    let utf16: Vec<u16> = r.plain.encode_utf16().collect();
    let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
    assert_eq!(picked, "99%");
}

#[test]
fn main_row_includes_provider_name_padded() {
    // Item 2 of the redesign: the provider name gets its own column-like
    // padding, then the email, then the tab-stopped `n% / n%` run.
    let claude = main_row(
        "Claude",
        &acct("a@x.com", Some(1.0), Some(2.0), false),
        bands(),
    );
    let codex = main_row(
        "Codex",
        &acct("b@x.com", Some(1.0), Some(2.0), false),
        bands(),
    );
    let (claude_label, _) = claude.plain.split_once('\t').unwrap();
    let (codex_label, _) = codex.plain.split_once('\t').unwrap();
    assert!(claude_label.starts_with("Claude"));
    assert!(codex_label.starts_with("Codex"));
    // Both providers' emails start at the same column regardless of the
    // provider-name length ("Claude" vs "Codex").
    assert_eq!(claude_label.find("a@x.com"), codex_label.find("b@x.com"));
}

#[test]
fn submenu_info_rows_never_include_a_percentage() {
    // Item 3 of the redesign: window percentages move to the main-list
    // row; submenu window rows are reset text only. (Provider-reported
    // rows — scoped limits, the weekly breakdown — are the exception:
    // they have no main-list slot. This fixture reports none.)
    let a = acct("a@x.com", Some(97.0), Some(88.0), true);
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
    for row in submenu_info_rows(&sec, &a) {
        assert!(!row.contains('%'), "submenu row leaked a percentage: {row}");
    }
}

#[test]
fn needs_relogin_trailing_beats_usage_and_lock() {
    // A dead login must never render a usage pct or a stale countdown — its
    // numbers are frozen, so the row surfaces the re-login state in red
    // regardless of what the cached percentages / reset instants say.
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let reset = now + chrono::Duration::minutes(90);
    with_now(now, || {
        let mut a = acct_with_resets(
            "stale@x.com",
            Some(100.0),
            Some(100.0),
            false,
            Some(reset),
            Some(reset),
        );
        a.needs_relogin = true;
        let (trailing, colors) = trailing_for_account(&a, bands(), now);
        assert_eq!(trailing, "⚠ re-login");
        assert!(!trailing.contains('%'), "no percentage on a dead login");
        assert_eq!(colors.len(), 1, "single red span over the whole run");
        assert_eq!(colors[0].2, Severity::Red);
    });
}

#[test]
fn locked_account_with_credits_shows_money_left_not_a_countdown() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let reset = now + chrono::Duration::days(4);
    with_now(now, || {
        let mut a = acct_with_resets(
            "paid@x.com",
            Some(0.0),
            Some(100.0),
            true,
            Some(reset),
            Some(reset),
        );
        a.reported.credits = Some(Credits {
            enabled: true,
            used: usd(37_225),
            limit: usd(100_000),
            balance: usd(62_706),
            ..Credits::default()
        });
        let (trailing, colors) = trailing_for_account(&a, bands(), now);
        assert_eq!(trailing, "$627 credits");
        assert!(colors.is_empty(), "a working account isn't painted red");
        // Cap reached: back to the lock countdown.
        a.reported.credits.as_mut().unwrap().limit_reached = true;
        assert_eq!(trailing_for_account(&a, bands(), now).0, "4d 0h");
    });
}

#[test]
fn refresh_error_trailing_when_not_needing_relogin() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut a = acct("err@x.com", Some(10.0), Some(20.0), false);
    a.error = Some("HTTP 500".into());
    let (trailing, colors) = trailing_for_account(&a, bands(), now);
    assert_eq!(trailing, "⚠ error");
    assert_eq!(colors.len(), 1);
    assert_eq!(colors[0].2, Severity::Red);
}

#[test]
fn lapsed_subscription_trailing_is_red_dash_not_countdown() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let reset = now + chrono::Duration::days(4);
    with_now(now, || {
        let mut a = acct_with_resets(
            "lapsed@x.com",
            Some(0.0),
            Some(100.0),
            false,
            None,
            Some(reset),
        );
        a.no_subscription = true;
        let (trailing, colors) = trailing_for_account(&a, bands(), now);
        assert_eq!(trailing, "Free", "no plan name known → Free");
        assert_eq!(colors, vec![(0, 4, Severity::Red)]);
        a.plan = Some("Pro".into());
        assert_eq!(trailing_for_account(&a, bands(), now).0, "Pro");
    });
}

#[test]
fn an_expiring_login_asks_for_a_renewal() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    with_now(now, || {
        let mut a = acct("dev@example.com", Some(10.0), Some(20.0), false);
        a.login_expires_at = Some(now + chrono::Duration::hours(50));
        // Not renewable from usagio → nothing to offer; usage stays.
        assert_eq!(trailing_for_account(&a, bands(), now).0, "10% / 20%");
        a.renewable = true;
        let (t, colors) = trailing_for_account(&a, bands(), now);
        assert_eq!(t, "renew · 2d");
        assert_eq!(colors[0].2, Severity::Amber);
        let rows = submenu_info_rows(&claude_section(), &a);
        assert!(rows[0].starts_with("Login expires in 2d"), "{rows:?}");
        assert!(account_submenu_rows(&claude_section(), &a).renew_row);
        // Far from expiry: back to usage.
        a.login_expires_at = Some(now + chrono::Duration::days(20));
        assert_eq!(trailing_for_account(&a, bands(), now).0, "10% / 20%");
    });
}

#[test]
fn a_dead_renewable_login_points_at_renew_not_the_cli() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut a = acct("dev@example.com", Some(10.0), Some(20.0), false);
    a.needs_relogin = true;
    a.renewable = true;
    assert_eq!(trailing_for_account(&a, bands(), now).0, "⚠ renew");
    let rows = submenu_info_rows(&claude_section(), &a);
    assert_eq!(rows, vec!["⚠ Login expired — Renew login… below"]);
}

#[test]
fn a_lapsed_renewable_login_reads_expired_not_under_an_hour() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    with_now(now, || {
        let mut a = acct("dev@example.com", Some(10.0), Some(20.0), false);
        a.renewable = true;
        a.login_expires_at = Some(now - chrono::Duration::hours(3));
        let (t, colors) = trailing_for_account(&a, bands(), now);
        assert_eq!(t, "⚠ renew");
        assert_eq!(colors[0].2, Severity::Red);
        let rows = submenu_info_rows(&claude_section(), &a);
        assert_eq!(rows[0], "⚠ Login expired — Renew login… below", "{rows:?}");
    });
}

#[test]
fn login_left_label_is_coarse() {
    assert_eq!(login_left_label(chrono::Duration::hours(71)), "2d");
    assert_eq!(login_left_label(chrono::Duration::minutes(150)), "2h");
    assert_eq!(login_left_label(chrono::Duration::minutes(20)), "<1h");
}

#[test]
fn lapsed_subscription_outranks_needs_relogin() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut a = acct("lapsed@example.com", Some(0.0), Some(100.0), false);
    a.no_subscription = true;
    a.needs_relogin = true;
    a.plan = Some("Free".into());
    assert_eq!(trailing_for_account(&a, bands(), now).0, "Free");
    assert_eq!(
        submenu_info_rows(&claude_section(), &a),
        vec!["No subscription · Free plan"]
    );
}

#[test]
fn lapsed_subscription_submenu_explains_instead_of_window_rows() {
    let mut a = acct("lapsed@x.com", Some(0.0), Some(100.0), false);
    a.no_subscription = true;
    a.plan = Some("Free".into());
    let rows = submenu_info_rows(&claude_section(), &a);
    assert_eq!(rows, vec!["No subscription · Free plan"], "one row only");
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("renews in") || r.contains("unlocks in")),
        "no stale window rows: {rows:?}"
    );
    assert!(account_extra_info_rows(&claude_section(), &a).is_empty());
}

fn usd(minor: i64) -> Option<Money> {
    Money::new(minor, 2, "USD")
}

fn share(label: &str, percent: f64) -> UsageShare {
    UsageShare {
        key: label.to_ascii_lowercase(),
        label: label.into(),
        percent,
    }
}

fn scoped(label: &str, pct: Option<f64>) -> UsageWindow {
    UsageWindow {
        id: format!("scoped:{label}"),
        label: format!("{label} 7d"),
        utilization: pct,
        resets_at: None,
    }
}

fn with_reported(r: ReportedUsage) -> Vec<String> {
    let mut a = acct("a@x.com", Some(10.0), Some(20.0), true);
    a.reported = r;
    submenu_info_rows(&claude_section(), &a)
}

#[test]
fn no_reported_rows_when_nothing_reported() {
    assert!(reported_rows(&ReportedUsage::default()).is_empty());
    let rows = with_reported(ReportedUsage::default());
    assert!(
        !rows
            .iter()
            .any(|r| r.starts_with("Credits") || r.starts_with("This week")),
        "{rows:?}"
    );
}

#[test]
fn credits_row_shows_available_amount_under_the_spending_cap() {
    let mut c = Credits {
        enabled: true,
        used: usd(5463),
        limit: usd(10000),
        balance: usd(90335),
        ..Credits::default()
    };
    assert_eq!(credits_row(&c).as_deref(), Some("Credits · $45.37"));
    c.balance = usd(1200);
    assert_eq!(credits_row(&c).as_deref(), Some("Credits · $12.00"));
    c.used = usd(10000);
    assert_eq!(credits_row(&c).as_deref(), Some("Credits · 0"));
    c.balance = Money::new(63649, 2, Money::CREDITS);
    c.limit = None;
    c.used = None;
    assert_eq!(credits_row(&c).as_deref(), Some("Credits · 636.49"));
}

#[test]
fn credits_row_reports_none_for_disabled_empty_or_capped_accounts() {
    for c in [
        Credits::default(),
        Credits {
            used: usd(100286),
            limit: usd(0),
            ..Credits::default()
        },
        Credits {
            enabled: true,
            limit_reached: true,
            balance: usd(90335),
            ..Credits::default()
        },
        Credits {
            enabled: true,
            balance: usd(0),
            ..Credits::default()
        },
    ] {
        assert_eq!(credits_row(&c).as_deref(), Some("Credits · 0"));
    }
}

#[test]
fn credits_row_does_not_invent_an_unknown_balance() {
    assert_eq!(
        credits_row(&Credits {
            enabled: true,
            ..Credits::default()
        })
        .as_deref(),
        Some("Credits · —")
    );
    assert_eq!(
        credits_row(&Credits {
            unlimited: true,
            ..Credits::default()
        })
        .as_deref(),
        Some("Credits · unlimited")
    );
}

#[test]
fn breakdown_row_needs_two_non_zero_shares() {
    let lone = [
        share("Claude Code", 100.0),
        share("Chats", 0.0),
        share("Cowork", 0.0),
    ];
    assert_eq!(breakdown_row(&lone), None);
    assert_eq!(breakdown_row(&[]), None);
    let split = [
        share("Chats", 18.0),
        share("Other", 0.0),
        share("Claude Code", 82.0),
    ];
    assert_eq!(
        breakdown_row(&split).as_deref(),
        Some("This week · Claude Code 82% · Chats 18%")
    );
}

#[test]
fn scoped_limit_rows_skip_zero_and_unreported() {
    assert_eq!(scoped_limit_row(&scoped("Fable", Some(0.0))), None);
    assert_eq!(scoped_limit_row(&scoped("Fable", None)), None);
    assert_eq!(
        scoped_limit_row(&scoped("Fable", Some(42.4))).as_deref(),
        Some("Fable 7d · 42%")
    );
    let mut w = scoped("Fable", Some(42.0));
    w.resets_at = Some(Utc::now() + chrono::Duration::days(3));
    let row = scoped_limit_row(&w).unwrap();
    assert!(row.starts_with("Fable 7d · 42% · resets in "), "{row}");
}

#[test]
fn reset_gift_tracks_inactive_accounts_across_providers() {
    let now = Utc::now();
    let mut snap = two_section_snap(
        acct("claude@example.com", Some(10.0), Some(20.0), true),
        Some(43.0),
        Some("claude".into()),
    );
    let base = title_for(&snap);
    snap.sections[1].accounts[0].active = false;
    snap.sections[1].accounts[0].reported.reset_grants = vec![reset_fixture(false)];
    assert_eq!(title_for(&snap), format!("{base} 🎁"));
    let a = &snap.sections[1].accounts[0];
    assert!(account_header_row(&snap.sections[1], a)
        .plain
        .ends_with(" 🎁"));
    let (text, colors) = trailing_for_account(a, bands(), now);
    let (base_text, base_colors) = trailing_usage_for_account(a, bands(), now);
    assert_eq!(text, format!("{base_text} 🎁"));
    assert_eq!(colors, base_colors);
    assert_eq!(
        binding_span(a, &text, now),
        binding_span(a, &base_text, now)
    );
    snap.sections[1].accounts[0].reported.reset_grants[0].resets_left = 0;
    assert_eq!(title_for(&snap), base);
    assert!(
        !account_header_row(&snap.sections[1], &snap.sections[1].accounts[0])
            .plain
            .contains('🎁')
    );
}

#[test]
fn reset_grant_row_text_by_state() {
    let mk = |resets_left: u32, usable_now: bool| ResetGrant {
        id: "g1".into(),
        label: "Opus launch".into(),
        resets_left,
        usable_now,
        expires_at: None,
    };
    // Spent → no row.
    assert_eq!(reset_grant_row(&mk(0, true)), None);
    // Usable now → ready.
    assert_eq!(
        reset_grant_row(&mk(1, true)).as_deref(),
        Some("🎁 1 reset ready")
    );
    // Banked but not usable → banked.
    assert_eq!(
        reset_grant_row(&mk(1, false)).as_deref(),
        Some("🎁 1 reset banked")
    );
    // Expiry is appended when known.
    let mut g = mk(1, true);
    g.expires_at = Some(Utc::now() + chrono::Duration::days(3));
    assert!(reset_grant_row(&g)
        .unwrap()
        .starts_with("🎁 1 reset ready · expires in "));
    let mut multiple = mk(2, false);
    assert_eq!(
        reset_grant_row(&multiple).as_deref(),
        Some("🎁 2 resets banked")
    );
    multiple.usable_now = true;
    assert_eq!(
        reset_grant_row(&multiple).as_deref(),
        Some("🎁 2 resets ready")
    );
}

#[test]
fn reset_grant_rows_lead_reported_rows() {
    let rows = reported_rows(&ReportedUsage {
        scoped_limits: vec![scoped("Fable", Some(42.0))],
        reset_grants: vec![ResetGrant {
            id: "g1".into(),
            label: "Opus launch".into(),
            resets_left: 1,
            usable_now: true,
            expires_at: None,
        }],
        ..ReportedUsage::default()
    });
    assert_eq!(rows[0], "🎁 1 reset ready", "{rows:?}");
}

#[test]
fn submenu_renders_reported_rows_after_the_windows() {
    let rows = with_reported(ReportedUsage {
        credits: Some(Credits {
            enabled: true,
            balance: usd(310),
            ..Credits::default()
        }),
        breakdown: vec![share("Claude Code", 82.0), share("Chats", 18.0)],
        scoped_limits: vec![scoped("Fable", Some(42.0)), scoped("Haiku", Some(0.0))],
        ..ReportedUsage::default()
    });
    let tail: Vec<&str> = rows
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "Fable 7d · 42%",
            "This week · Claude Code 82% · Chats 18%",
            "Credits · $3.10",
        ],
        "{rows:?}"
    );
    assert_eq!(rows[0], "Session renews in 3h", "windows lead: {rows:?}");
}

#[test]
fn lapsed_account_shows_no_reported_rows() {
    let mut a = acct("lapsed@x.com", Some(0.0), Some(0.0), false);
    a.no_subscription = true;
    a.reported = ReportedUsage {
        credits: Some(Credits {
            enabled: true,
            used: usd(310),
            ..Credits::default()
        }),
        breakdown: vec![share("Claude Code", 50.0), share("Chats", 50.0)],
        scoped_limits: vec![scoped("Fable", Some(42.0))],
        ..ReportedUsage::default()
    };
    let rows = submenu_info_rows(&claude_section(), &a);
    assert!(rows[0].contains("No subscription"), "{rows:?}");
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("Credits") || r.contains("This week") || r.contains("Fable")),
        "{rows:?}"
    );
}

#[test]
fn menu_signature_changes_when_reported_rows_change() {
    let base = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    let sig1 = menu_signature(&base);
    let mut changed = base;
    changed.sections[0].accounts[0].reported.credits = Some(Credits {
        enabled: true,
        balance: usd(310),
        ..Credits::default()
    });
    let sig2 = menu_signature(&changed);
    assert_ne!(sig1, sig2);
    changed.sections[0].accounts[0].reported.credits = Some(Credits {
        enabled: true,
        balance: usd(311),
        ..Credits::default()
    });
    assert_ne!(sig2, menu_signature(&changed));
}

#[test]
fn lapsed_subscription_hides_switch() {
    let mut sec = claude_section();
    sec.supports_switching = true;
    let mut a = acct("lapsed@x.com", Some(0.0), Some(0.0), false);
    a.no_subscription = true;
    let rows = account_submenu_rows(&sec, &a);
    assert!(!rows.switch_row);
    // An active account also has no redundant status or switch row.
    a.active = true;
    assert!(!account_submenu_rows(&sec, &a).switch_row);
}

#[test]
fn submenu_leads_with_plan_when_known() {
    let mut a = acct("max@x.com", Some(10.0), Some(20.0), false);
    a.plan = Some("Max 20x".into());
    let rows = submenu_info_rows(&claude_section(), &a);
    assert_eq!(rows[0], "Plan · Max 20x");
}

#[test]
fn needs_relogin_submenu_leads_with_reason_and_fix() {
    let mut a = acct("stale@x.com", Some(100.0), Some(100.0), false);
    a.needs_relogin = true;
    let rows = submenu_info_rows(&claude_section(), &a);
    assert!(
        rows[0].contains("Re-login required"),
        "first row states the problem: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("usagio capture")),
        "a row names the fix: {rows:?}"
    );
    // No usage/window rows once the login is dead.
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("renews in") || r.contains("unlocks in")),
        "dead login shows no window rows: {rows:?}"
    );
}

#[test]
fn tray_countdown_requires_resets_for_every_exhausted_window() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut a = acct("blocked@x.com", Some(100.0), Some(100.0), true);
    a.weekly_reset_at = Some(now + chrono::Duration::hours(2));
    let mut snap = one_section_snap(a);
    snap.tray_icon_mode = Some("claude".into());
    with_now(now, || assert_eq!(title_for(&snap), "🔒"));
}

#[test]
fn tray_recovery_excludes_relogin_accounts_and_other_providers() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut snap = two_section_snap(
        acct("active@x.com", Some(0.0), Some(100.0), true),
        Some(0.0),
        Some("claude".into()),
    );
    snap.tray_icon_mode = Some("claude".into());
    snap.sections[0].accounts[0].weekly_reset_at = Some(now + chrono::Duration::hours(3));
    let mut unusable = acct("relogin@x.com", Some(0.0), Some(100.0), false);
    unusable.weekly_reset_at = Some(now + chrono::Duration::hours(1));
    unusable.needs_relogin = true;
    snap.sections[0].accounts.push(unusable);
    with_now(now, || assert_eq!(title_for(&snap), "🔒 3h 0m"));
}

#[test]
fn codex_weekly_only_reports_missing_session_and_weekly_title() {
    let mut a = acct("codex@example.com", None, Some(40.0), true);
    a.provider_id = "codex";
    a.windows[0].reset.clear();
    let mut sec = claude_section();
    sec.provider_id = "codex";
    let rows = submenu_info_rows(&sec, &a);
    // No session window: nothing to say about it, not "Not reported".
    assert_eq!(rows, ["Weekly renews in 2d"]);
    assert_eq!(summary_pcts(&a), (None, Some(40.0)));
    // Codex has no session window, so the binding window is weekly:
    // `40%w` (0.12.2 binding-window display, design §4).
    assert_eq!(title_for(&one_section_snap(a)), "40%w");
}

#[test]
fn account_extra_info_rows_empty_without_usage_support() {
    // A provider with no usage endpoint has no burn-rate / cost / "updated"
    // rows — and crucially the extractor must take the no-disk path so it
    // stays callable off the main thread without a ScopedConfigDir.
    let a = acct("a@x.com", Some(20.0), Some(30.0), false);
    let sec = ProviderSection {
        provider_id: CLAUDE_SLUG,
        display_name: "Claude",
        supports_switching: true,
        supports_usage: false,
        supports_remove: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    };
    assert!(account_extra_info_rows(&sec, &a).is_empty());
}

#[test]
fn u16len_counts_surrogate_pairs() {
    assert_eq!(u16len("abc"), 3);
    assert_eq!(u16len("café"), 4); // é is BMP → 1 code unit
    assert_eq!(u16len("a😀b"), 4); // 😀 is astral → 2 code units
}

#[test]
fn main_row_offsets_hold_for_astral_email() {
    let a = acct("😀@x.com", Some(99.0), None, false);
    let r = main_row("Claude", &a, bands());
    let (off, len, _) = r.colors[0];
    let utf16: Vec<u16> = r.plain.encode_utf16().collect();
    let picked = String::from_utf16(&utf16[off..off + len]).unwrap();
    assert_eq!(picked, "99%");
}

#[test]
fn menu_signature_changes_when_a_window_reset_changes() {
    // A window's reset countdown is folded into the signature so a
    // changing reset triggers a redraw (would otherwise leave stale text).
    let mut a = acct("you@work.com", Some(50.0), Some(60.0), true);
    a.windows.push(WindowView {
        id: "opus".into(),
        label: "Opus 7d".into(),
        pct: Some(30.0),
        reset: "3h".into(),
    });
    let base = one_section_snap(a);
    let sig1 = menu_signature(&base);
    let mut changed = base;
    changed.sections[0].accounts[0].windows[2].reset = "2h".into();
    assert_ne!(sig1, menu_signature(&changed));
}

#[test]
fn menu_signature_folds_settings_and_account_error() {
    let base = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    let sig = menu_signature(&base);
    let mut m = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    m.tray_icon_mode = Some("codex".into());
    assert_ne!(sig, menu_signature(&m), "tray_icon_mode");
    let mut m = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    m.use_credits = !m.use_credits;
    assert_ne!(sig, menu_signature(&m), "use_credits");
    for i in 0..3 {
        let mut m = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
        let c = &mut m.notification_config;
        match i {
            0 => c.threshold_enabled = !c.threshold_enabled,
            1 => c.reset_back_enabled = !c.reset_back_enabled,
            _ => c.pace_enabled = !c.pace_enabled,
        }
        assert_ne!(sig, menu_signature(&m), "notification flag {i}");
    }
    let mut m = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    m.sections[0].accounts[0].error = Some("boom".into());
    assert_ne!(sig, menu_signature(&m), "account error");
}

#[test]
fn window_status_row_locks_at_the_countdown_threshold() {
    let w = WindowView {
        id: "weekly".into(),
        label: "7d".into(),
        pct: Some(99.7),
        reset: "3d".into(),
    };
    let (text, locked) = window_status_row_at(&w, false).unwrap();
    assert!(locked, "99.7% is locked per countdown threshold: {text}");
    assert!(text.contains("unlocks in"), "{text}");
}

#[test]
fn menu_signature_folds_provider_id() {
    // Same account, same numbers, different provider slug → different
    // signature. Guards against a cross-provider menu change being
    // swallowed by a stale Claude-shaped fingerprint.
    let mut base = one_section_snap(acct("you@work.com", Some(50.0), Some(60.0), true));
    let sig1 = menu_signature(&base);
    // Simulate the same-shaped row landing under a different provider.
    base.sections[0].provider_id = "codex";
    base.sections[0].accounts[0].provider_id = "codex";
    assert_ne!(sig1, menu_signature(&base));
}

#[test]
fn locked_window_rows_are_flagged_and_say_unlocks_healthy_say_renews() {
    // dev6's real shape: session maxed, weekly just reset to 0%.
    let a = acct("dev6@x.com", Some(100.0), Some(0.0), false);
    let items = submenu_info_items(&claude_section(), &a);
    assert!(
        items.contains(&("Session locked · unlocks in 3h".to_string(), true)),
        "{items:?}"
    );
    assert!(
        items.contains(&("Weekly renews in 2d".to_string(), false)),
        "{items:?}"
    );
    assert!(
        !items.iter().any(|(r, _)| r.contains("resets in")),
        "{items:?}"
    );
}

#[test]
fn provider_header_is_always_a_static_label_row() {
    // Every provider header is the same static row with its icon — no
    // submenu whatever the state (env override, blocked active account).
    let mut blocked = acct("a@x.com", Some(100.0), Some(100.0), true);
    blocked.weekly_reset_at = Some(Utc::now() + chrono::Duration::days(2));
    let mut snap = one_section_snap(blocked);
    for env in [false, true] {
        snap.sections[0].env_override_active = env;
        match cross_platform::build_provider_group_item(&snap.sections[0]) {
            crate::platform::MenuItem::Static { label, icon_png } => {
                assert!(icon_png.is_some(), "header keeps its icon");
                assert_eq!(label.contains(ENV_OVERRIDE_ROW_TITLE), env, "{label}");
            }
            _ => panic!("provider header must be a static label row"),
        }
    }
}

#[test]
fn main_row_flat_shape_matches_spec_when_not_locked() {
    // "{provider}    {email}\t{n}% / {n}%" per the v0.5.1 UX pass — the
    // crate's variant uses a TAB between the label and trailing run so
    // AppKit right-aligns it. The important structural invariants are:
    // provider, then email, one TAB, then "n% / n%". Any change to that
    // layout will fail this assertion — a wall against silent drift.
    let a = acct("you@work.com", Some(42.0), Some(61.0), false);
    let r = main_row("Claude", &a, bands());
    assert_eq!(r.plain, "Claude    you@work.com\t42% / 61%");
    assert!(!r.checkmark, "inactive row: no leading checkmark");
}

#[test]
fn main_row_switches_to_locked_countdown_when_over_threshold() {
    // Session at 100% with a reset ~90 minutes out → row must swap the
    // percentages for "locked · 1h 30m" and paint it red. This is the
    // "usage → locked" transition the redesign spec calls out.
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let reset = now + chrono::Duration::minutes(90);
    with_now(now, || {
        let a = acct_with_resets(
            "matt@example.com",
            Some(100.0),
            Some(20.0),
            true,
            Some(reset),
            None,
        );
        let r = main_row("Claude", &a, bands());
        // v0.5.0 UX: no "locked · " prefix — red color + a time (not a
        // percent) is the affordance. See main_row's locked-branch comment.
        assert_eq!(r.plain, "Claude    matt@example.com\t1h 30m");
        assert!(r.bold, "active locked row is still bold");
        assert!(r.checkmark, "active row gets a leading checkmark");
        // Exactly one colored span, red-tinted, covering the countdown run.
        assert_eq!(r.colors.len(), 1);
        let (off, len, sev) = r.colors[0];
        assert_eq!(sev, Severity::Red);
        let picked: String = r.plain.chars().skip(off).take(len).collect();
        assert_eq!(picked, "1h 30m");
    });
}

#[test]
fn main_row_stays_usage_when_reset_is_stale() {
    // pct at 100 but reset already in the past — countdown treats it as
    // stale (next refresh will fix the pct), so the row stays "S / W".
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let past = now - chrono::Duration::minutes(5);
    with_now(now, || {
        let a = acct_with_resets("dev@x.com", Some(100.0), None, false, Some(past), None);
        let r = main_row("Claude", &a, bands());
        assert_eq!(r.plain, "Claude    dev@x.com\t100% / —");
        assert!(!r.plain.contains("locked"));
    });
}

/// Minimal `ProviderSection` for `account_header_row` tests — only
/// `display_name`/`severity_bands` matter to that function; the rest are
/// filled with harmless defaults.
fn header_test_section(display_name: &'static str) -> ProviderSection {
    ProviderSection {
        provider_id: "claude",
        display_name,
        supports_switching: true,
        supports_remove: true,
        supports_usage: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    }
}

#[test]
fn account_header_row_flat_shape_matches_spec_when_not_locked() {
    // v0.5.2 per-account BLOCK header: "{provider} · {email}\t{n}% / {n}%"
    // — replaces `main_row`'s padded flat-list format as the string every
    // production render path (`menu_tree_from_snapshot`) now
    // uses for a block's first row.
    let sec = header_test_section("Claude");
    let a = acct("you@work.com", Some(42.0), Some(61.0), false);
    let r = account_header_row(&sec, &a);
    assert_eq!(r.plain, "you@work.com\t42% / 61%");
    assert!(!r.checkmark, "inactive row: no leading checkmark");
    assert_eq!(r.tab_x_kind, Some(TabX::MenuRight));
}

#[test]
fn account_header_row_switches_to_locked_countdown_when_over_threshold() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let reset = now + chrono::Duration::minutes(90);
    with_now(now, || {
        let sec = header_test_section("Claude");
        let a = acct_with_resets(
            "matt@example.com",
            Some(100.0),
            Some(20.0),
            true,
            Some(reset),
            None,
        );
        let r = account_header_row(&sec, &a);
        // Session-locked + weekly-healthy nuance (v0.5.2 addendum #7):
        // trailing = "<countdown> / <weekly%>" so the user can still see
        // the healthy weekly headroom while the session countdown ticks.
        assert_eq!(r.plain, "matt@example.com\t1h 30m / 20%");
        assert!(r.bold, "active locked row is still bold");
        assert!(r.checkmark, "active row gets a leading checkmark");
        // countdown is red; weekly=20 sits below the severity bands so it
        // contributes no additional color entry.
        assert_eq!(r.colors.len(), 1);
        let (_, _, sev) = r.colors[0];
        assert_eq!(sev, Severity::Red);
    });
}

#[test]
fn account_header_row_colors_high_percentages_per_provider_bands() {
    // Same severity-band contract `main_row` has always had, just on the
    // new label format — a regression guard against the block-redesign
    // refactor accidentally dropping the color computation.
    let sec = header_test_section("Codex");
    let a = acct("hot@x.com", Some(96.0), Some(10.0), false);
    let r = account_header_row(&sec, &a);
    assert_eq!(r.plain, "hot@x.com\t96% / 10%");
    assert_eq!(r.colors.len(), 1, "only the 96% session run is colored");
    let (off, len, sev) = r.colors[0];
    assert_eq!(sev, Severity::Red);
    let picked: String = r.plain.chars().skip(off).take(len).collect();
    assert_eq!(picked, "96%");
}

#[test]
fn provider_group_header_row_is_bold_plain_display_name_with_icon() {
    // v0.5.3: the bold provider-group header row that precedes a
    // provider's account rows carries the display name verbatim, bold,
    // and the provider's 16px icon slug. No tab stop, no checkmark, no
    // color spans — a header, not a data row.
    let sec = header_test_section("Claude");
    let row = provider_group_header_row(&sec);
    assert_eq!(row.plain, "Claude");
    assert!(row.bold, "group header is bold");
    assert!(row.section_header, "marked as a section header");
    assert_eq!(row.icon_slug, Some("claude"));
    assert!(row.tab_x_kind.is_none(), "no tab stop");
    assert!(!row.checkmark);
    assert!(row.colors.is_empty());
}

// -----------------------------------------------------------------------
// Capability-gated menu rows. `supports_switching`
// gates the "Switch to this account" row and `supports_remove` the
// "Remove…" row, so a usage-only provider like Codex shows neither
// action it can't perform.
// -----------------------------------------------------------------------

fn section_with_caps(supports_switching: bool, supports_remove: bool) -> ProviderSection {
    ProviderSection {
        provider_id: "codex",
        display_name: "Codex",
        supports_switching,
        supports_remove,
        supports_usage: true,
        severity_bands: bands(),
        env_override_active: false,
        accounts: vec![],
    }
}

#[test]
fn submenu_rows_omit_switch_when_unsupported() {
    // A provider without switching support has no Switch row.
    let sec = section_with_caps(false, true);
    let a = acct("user@example.com", Some(10.0), Some(20.0), false);
    let rows = account_submenu_rows(&sec, &a);
    assert!(!rows.switch_row, "no switch row: {rows:?}");
}

#[test]
fn submenu_rows_omit_switch_for_active_accounts() {
    let sec = section_with_caps(true, true);
    let a = acct("user@example.com", Some(10.0), Some(20.0), true);
    assert!(!account_submenu_rows(&sec, &a).switch_row);
}

#[test]
fn submenu_rows_omit_remove_when_supports_remove_is_false() {
    let sec = section_with_caps(false, false);
    let a = acct("user@example.com", Some(10.0), Some(20.0), false);
    assert!(!account_submenu_rows(&sec, &a).remove_row);
}

#[test]
fn submenu_rows_include_remove_when_supports_remove_is_true() {
    let sec = section_with_caps(false, true);
    let a = acct("user@example.com", Some(10.0), Some(20.0), false);
    assert!(account_submenu_rows(&sec, &a).remove_row);
}

#[test]
fn menu_signature_folds_lock_state_and_reset_at() {
    // Same pcts + updated + windows — but toggling only the reset instant
    // (usage → locked transition) must yield a different signature so the
    // menu redraws with the "locked · Xh Ym" trailing text.
    let now = Utc.timestamp_opt(3_000_000, 0).unwrap();
    with_now(now, || {
        let a_usage = acct_with_resets("m@x.com", Some(100.0), Some(50.0), true, None, None);
        let usage_snap = one_section_snap(a_usage);
        let sig_usage = menu_signature(&usage_snap);
        assert!(sig_usage.contains("L=0"), "usage state marker present");

        let a_locked = acct_with_resets(
            "m@x.com",
            Some(100.0),
            Some(50.0),
            true,
            Some(now + chrono::Duration::hours(2)),
            None,
        );
        let locked_snap = one_section_snap(a_locked);
        let sig_locked = menu_signature(&locked_snap);
        assert!(sig_locked.contains("L=S"), "session-locked marker present");
        assert_ne!(sig_usage, sig_locked, "lock transition must trigger redraw");
    });
}

#[test]
fn main_row_locked_shape_swaps_only_the_trailing_run() {
    // The locked row keeps the "{provider label}\t…" tab structure so
    // right-alignment still works — only the trailing "n% / n%" run
    // becomes "locked · <countdown>". A test that pins the structure so
    // a future refactor can't accidentally lose the tab.
    let now = Utc.timestamp_opt(4_000_000, 0).unwrap();
    with_now(now, || {
        let a = acct_with_resets(
            "matt@example.com",
            Some(100.0),
            None,
            false,
            Some(now + chrono::Duration::hours(23) + chrono::Duration::minutes(52)),
            None,
        );
        let r = main_row("Claude", &a, bands());
        let (label, trailing) = r.plain.split_once('\t').expect("tab preserved");
        assert_eq!(label, "Claude    matt@example.com");
        // No "locked · " prefix — red color + a time (not a percent) is
        // the affordance now. See main_row's locked-branch comment.
        assert_eq!(trailing, "23h 52m");
        assert_eq!(
            r.tab_x_kind,
            Some(TabX::MenuRight),
            "right-align tab-stop preserved"
        );
    });
}

// -----------------------------------------------------------------------
// Manual-switch availability, API-key-once-captured, and the
// "white but not clickable" submenu style.
// -----------------------------------------------------------------------

#[test]
fn locked_accounts_keep_manual_switch_action() {
    let now = Utc.timestamp_opt(5_000_000, 0).unwrap();
    let a = acct_with_resets(
        "locked@example.com",
        Some(100.0),
        Some(100.0),
        false,
        Some(now + chrono::Duration::hours(1)),
        Some(now + chrono::Duration::days(2)),
    );
    let rows = account_submenu_rows(&claude_section(), &a);
    assert!(rows.switch_row);
}

#[test]
fn stat_display_label_names_claude_windows_and_falls_back_to_the_short_label() {
    let w = |id: &str| WindowView {
        id: id.into(),
        label: "short".into(),
        pct: None,
        reset: String::new(),
    };
    assert_eq!(stat_display_label(&w("session")), "Session");
    assert_eq!(stat_display_label(&w("weekly")), "Weekly");
    assert_eq!(stat_display_label(&w("opus")), "Opus");
    assert_eq!(stat_display_label(&w("something-new")), "short");
}

#[test]
fn main_row_checkmarks_only_the_active_account() {
    let active = main_row(
        "Claude",
        &acct("a@x.com", Some(10.0), Some(20.0), true),
        bands(),
    );
    assert!(active.checkmark, "active unlocked row gets the checkmark");
    let idle = main_row(
        "Claude",
        &acct("a@x.com", Some(10.0), Some(20.0), false),
        bands(),
    );
    assert!(!idle.checkmark);
}

#[test]
fn trailing_color_spans_land_on_the_weekly_percentage() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    // Usage: the weekly span starts after "<session> / ".
    let a = acct("hot@x.com", Some(10.0), Some(96.0), false);
    let (text, colors) = trailing_for_account(&a, bands(), now);
    assert_eq!(text, "10% / 96%");
    assert_eq!(colors, vec![(6, 3, Severity::Red)]);

    // Session-locked: the red countdown, then the weekly percentage.
    let locked = acct_with_resets(
        "hot@x.com",
        Some(100.0),
        Some(97.0),
        false,
        Some(now + chrono::Duration::minutes(90)),
        None,
    );
    let (text, colors) = trailing_for_account(&locked, bands(), now);
    assert_eq!(text, "1h 30m / 97%");
    assert_eq!(
        colors,
        vec![(0, 6, Severity::Red), (9, 3, Severity::Red)],
        "countdown span, then weekly span after ' / '"
    );
}

#[test]
fn account_header_row_offsets_colors_past_the_label() {
    let sec = header_test_section("Claude");
    let a = acct("hot@x.com", Some(10.0), Some(96.0), false);
    let r = account_header_row(&sec, &a);
    assert_eq!(r.plain, "hot@x.com\t10% / 96%");
    assert_eq!(r.colors.len(), 1);
    let (off, len, sev) = r.colors[0];
    assert_eq!(sev, Severity::Red);
    assert_eq!(span_text(&r.plain, off, len), "96%");
}

#[test]
fn quit_row_is_quit_tab_version() {
    let q = quit_row_plain();
    assert!(
        q.starts_with(&format!("Quit\tusagio v{}", env!("CARGO_PKG_VERSION"))),
        "{q}"
    );
    // A build tag is appended only when one was baked in at compile time.
    match option_env!("USAGIO_BUILD_TAG") {
        Some(tag) if !tag.is_empty() => assert!(q.contains(&format!("[{tag}]")), "{q}"),
        _ => assert!(!q.contains('['), "{q}"),
    }
}

#[test]
fn submenu_info_rows_edge_cases_for_missing_data() {
    let sec = claude_section();

    // Windows present but no data fetched yet → just "no data yet".
    let mut a = acct("a@x.com", Some(10.0), Some(20.0), true);
    a.has_data = false;
    assert_eq!(submenu_info_rows(&sec, &a), ["no data yet"]);

    // A locked window leads, ahead of window order.
    let mut a = acct("a@x.com", Some(50.0), Some(60.0), true);
    a.windows.push(WindowView {
        id: "opus".into(),
        label: "Opus 7d".into(),
        pct: Some(100.0),
        reset: "1d".into(),
    });
    assert_eq!(
        submenu_info_items(&sec, &a),
        [
            ("Opus locked · unlocks in 1d".to_string(), true),
            ("Session renews in 3h".to_string(), false),
            ("Weekly renews in 2d".to_string(), false),
        ]
    );

    // Only Codex announces a window it doesn't report; for Claude an
    // unreported window with nothing to say is simply omitted.
    let mut a = acct("a@x.com", None, Some(40.0), true);
    a.windows[0].reset.clear();
    assert_eq!(submenu_info_rows(&sec, &a), ["Weekly renews in 2d"]);

    // Usage known but no reset times reported: the header shows the
    // percentages, so the submenu must not claim there is no data.
    let mut a = acct("a@x.com", Some(40.0), Some(20.0), true);
    for w in &mut a.windows {
        w.reset.clear();
    }
    a.plan = Some("Max".into());
    assert_eq!(
        submenu_info_rows(&sec, &a),
        ["Plan · Max", "reset times not reported"]
    );
}

#[test]
fn an_expiring_login_row_does_not_hide_the_usage_fallback() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    with_now(now, || {
        let mut a = acct("a@x.com", Some(40.0), Some(20.0), true);
        for w in &mut a.windows {
            w.reset.clear();
        }
        a.renewable = true;
        a.login_expires_at = Some(now + chrono::Duration::days(2));
        let rows = submenu_info_rows(&claude_section(), &a);
        assert_eq!(
            rows.last().map(String::as_str),
            Some("reset times not reported"),
            "{rows:?}"
        );
    });
}

#[test]
fn account_extra_info_rows_footer_and_burn_rate() {
    let _g = crate::store::ScopedConfigDir::new();
    let sec = claude_section();
    let mk = || {
        let mut a = acct("burn@x.com", Some(30.0), Some(60.0), true);
        a.updated = "1m ago".into();
        a
    };
    let a = mk();

    // Keep the same layout without inventing a pace when history is absent.
    assert_eq!(
        account_extra_info_rows(&sec, &a),
        ["Weekly · 60% · —", "updated 1m ago"]
    );
    let mut inactive = mk();
    inactive.active = false;
    assert_eq!(
        account_extra_info_rows(&sec, &inactive),
        account_extra_info_rows(&sec, &a)
    );

    // A steady climb over the last hour → a confident burn-rate row first.
    let now = Utc::now();
    for i in 0..8 {
        crate::usage_log::append(&crate::usage_log::Snapshot {
            ts: now - chrono::Duration::minutes(70 - i * 10),
            provider: CLAUDE_SLUG.to_string(),
            account: "burn@x.com".to_string(),
            session_pct: Some(10.0),
            weekly_pct: Some(20.0 + i as f32 * 3.0),
            active_model: None,
            ..Default::default()
        })
        .unwrap();
    }
    // The usage-log reader is mtime-cached process-wide; drop entries other
    // tests (or the empty read above) left behind so ours is parsed fresh.
    crate::usage_log::evict_recent_months();
    let rows = with_now(now, || account_extra_info_rows(&sec, &a));
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].starts_with("Weekly · "), "{rows:?}");
    assert_ne!(
        rows[0], "Weekly · 60% · —",
        "a real pace, not the no-history fallback"
    );
    assert_eq!(rows[1], "updated 1m ago");

    // Weekly already locked → the burn forecast is redundant and skipped.
    let mut locked = mk();
    locked.windows[1].pct = Some(100.0);
    assert_eq!(account_extra_info_rows(&sec, &locked), ["updated 1m ago"]);

    // No fetched data / no windows / lapsed plan → nothing to add.
    let mut no_data = mk();
    no_data.has_data = false;
    assert!(account_extra_info_rows(&sec, &no_data).is_empty());
    let mut no_windows = mk();
    no_windows.windows.clear();
    assert!(account_extra_info_rows(&sec, &no_windows).is_empty());
    let mut lapsed = mk();
    lapsed.no_subscription = true;
    assert!(account_extra_info_rows(&sec, &lapsed).is_empty());
}

#[test]
fn recovery_ignores_a_maxed_opus_only_window() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mk = || {
        let mut a = acct("solo@x.com", Some(10.0), Some(30.0), true);
        a.windows.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: Some(100.0),
            reset: "2d".into(),
        });
        a
    };
    let a = mk();
    let mut sec = claude_section();
    sec.accounts = vec![mk()];
    assert_eq!(
        recovery_status(&sec, &a, now),
        None,
        "session/weekly have headroom: the account is usable"
    );
}

#[test]
fn recovery_blocked_without_any_reset_time_still_reports_the_block() {
    // Both limits maxed with no reset known anywhere: not "usage is fine".
    let a = acct("solo@x.com", Some(100.0), Some(10.0), true);
    assert_eq!(title_for(&one_section_snap(a)), "🔒");
}

#[test]
fn recovery_title_shows_credits_when_the_locked_account_can_spend_them() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let with_credits = |limit_reached: bool| {
        let mut a = locked_for("active@x.com", now, true);
        a.reported.credits = Some(Credits {
            enabled: true,
            used: usd(37_225),
            limit: usd(100_000),
            balance: usd(62_706),
            limit_reached,
            ..Credits::default()
        });
        a
    };
    let title_detail = |limit_reached: bool| {
        let mut sec = claude_section();
        sec.accounts = vec![with_credits(limit_reached)];
        recovery_status(&sec, &with_credits(limit_reached), now).expect("recovery info")
    };
    let (title, detail) = title_detail(false);
    assert_eq!(title, "$627 credits");
    assert_eq!(detail, "active@x.com · Plan used up · using credits");
    // Cap reached: back to the lock countdown.
    assert_eq!(title_detail(true).0, "🔒 1h 0m");
}

#[test]
fn recovery_only_offers_alternatives_the_provider_can_switch_to() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let alt = || acct("alt@x.com", Some(0.0), Some(0.0), false);
    let active = locked_for("active@x.com", now, true);

    // Provider can't switch: the healthy account is no way out, but the
    // active account's own lock is still reported.
    let mut sec = claude_section();
    sec.supports_switching = false;
    sec.accounts = vec![locked_for("active@x.com", now, true), alt()];
    let (title, _) = recovery_status(&sec, &active, now).expect("recovery info");
    assert_eq!(title, "🔒 1h 0m");

    // An env override defeats any switch we make.
    let mut sec = claude_section();
    sec.env_override_active = true;
    sec.accounts = vec![locked_for("active@x.com", now, true), alt()];
    let (title, _) = recovery_status(&sec, &active, now).expect("recovery info");
    assert_eq!(title, "🔒 1h 0m");
}

#[test]
fn recovery_prefers_the_first_of_two_equally_early_unlocks() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let (_, detail) =
        recovery_with(now, locked_for("tie@x.com", now, false)).expect("recovery info");
    assert!(
        detail.contains("Next available: active@x.com"),
        "a tie keeps the earlier entry: {detail}"
    );
}

#[test]
fn menu_tree_labels_spans_and_quit_row_come_from_the_row_builders() {
    // The menu builder reads the usage log for burn-rate rows.
    let _g = crate::store::ScopedConfigDir::new();
    use crate::platform::{MenuItem, ValueColor, ValueSpan};
    let snap = one_section_snap(acct("hot@x.com", Some(96.0), Some(97.0), true));
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    match &tree.items[1] {
        MenuItem::Submenu {
            label,
            active,
            value_spans,
            ..
        } => {
            assert_eq!(label, "hot@x.com\t96% / 97%");
            assert!(*active);
            assert_eq!(
                value_spans,
                &vec![
                    ValueSpan {
                        start: 0,
                        len: 3,
                        color: ValueColor::Red
                    },
                    ValueSpan {
                        start: 6,
                        len: 3,
                        color: ValueColor::Red
                    },
                ],
                "spans are relative to the text after the tab"
            );
        }
        other => panic!("expected the account submenu, got {other:?}"),
    }
    let quit = tree
        .items
        .iter()
        .find_map(|i| match i {
            MenuItem::Action { id, label, .. } if id == "quit" => Some(label.clone()),
            _ => None,
        })
        .expect("quit row");
    assert_eq!(quit, quit_row_plain());
}

#[test]
fn render_menu_png_produces_a_png() {
    let _l = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::store::ScopedConfigDir::new();
    save_state_with_mtime(&State::default(), next_mtime_tag());
    let png = render_menu_png_for_theme("gnome", 1.0);
    assert!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "not a PNG: {:?}",
        &png[..png.len().min(8)]
    );
    assert!(
        png.len() > 100,
        "suspiciously small image: {} bytes",
        png.len()
    );
}

#[test]
fn active_row_marks_the_binding_segment() {
    let sec = claude_section();
    let a = rated(
        acct("dev4", Some(29.0), Some(96.0), true),
        Some(1.8),
        Some(0.47),
    );
    let row = account_header_row(&sec, &a);
    let (label, trailing) = row.plain.split_once('\t').unwrap();
    let base = u16len(label) + 1;
    assert_eq!(trailing, "29% / 96%");
    assert_eq!(
        row.bold_spans,
        vec![(base + u16len("29% / "), u16len("96%"))]
    );
    // Inactive rows carry no binding mark.
    let b = rated(
        acct("x", Some(29.0), Some(96.0), false),
        Some(1.8),
        Some(0.47),
    );
    assert!(account_header_row(&sec, &b).bold_spans.is_empty());
}

#[test]
fn stale_after_reset_window_is_not_shown_as_locked() {
    let w = WindowView {
        id: "weekly".into(),
        label: "7d".into(),
        pct: Some(100.0),
        reset: "<1m".into(),
    };
    let (text, locked) = window_status_row_at(&w, true).unwrap();
    assert!(!locked && !text.contains("locked"), "{text}");
    assert!(text.contains("awaiting"), "{text}");
}

#[test]
fn menu_signature_tracks_the_rendered_percentage_on_ties() {
    let a = menu_signature(&one_section_snap(acct("a@x", Some(2.5), Some(1.0), true)));
    let b = menu_signature(&one_section_snap(acct("a@x", Some(3.0), Some(1.0), true)));
    assert_ne!(a, b, "2.5 renders 2% and 3.0 renders 3%");
}

#[test]
fn a_weekly_lock_whose_reset_passed_is_not_shown_locked_in_the_submenu() {
    let now = now_utc();
    let mut a = acct("stale@x.com", Some(20.0), Some(100.0), false);
    a.weekly_reset_at = Some(now - chrono::Duration::minutes(1));
    a.fetched_at = Some(now - chrono::Duration::minutes(10));
    let items = submenu_info_items(&claude_section(), &a);
    assert!(
        !items
            .iter()
            .any(|(r, locked)| *locked || r.contains("locked")),
        "a stale 100% weekly was shown locked: {items:?}"
    );
    assert!(
        items.iter().any(|(r, _)| r.contains("awaiting")),
        "{items:?}"
    );
    assert!(
        items.contains(&("Session renews in 3h".to_string(), false)),
        "the session row was dropped: {items:?}"
    );
    // The stale weekly does not lead: the session row keeps its place.
    assert!(items[0].0.starts_with("Session"), "{items:?}");
}
