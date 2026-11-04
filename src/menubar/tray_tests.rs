use super::*;
use crate::menubar::model::Snapshot;
use crate::menubar::render::title_for;
use crate::menubar::test_support::{acct, one_section_snap, rated, two_section_snap, with_now};
use crate::providers::CLAUDE_SLUG;
use chrono::{TimeZone, Utc};

#[test]
fn tray_target_at_risk_picks_highest_pct_agent() {
    // Claude at 40%, Codex at 90% → at-risk picks Codex.
    let snap = two_section_snap(acct("c@x", Some(40.0), Some(30.0), true), Some(90.0), None);
    let (sec, _) = tray_target(&snap, None).expect("a target");
    assert_eq!(sec.provider_id, "codex");
    // Flip it: Claude 99% beats Codex 10%.
    let snap = two_section_snap(acct("c@x", Some(99.0), Some(20.0), true), Some(10.0), None);
    let (sec, _) = tray_target(&snap, None).expect("a target");
    assert_eq!(sec.provider_id, CLAUDE_SLUG);
}

#[test]
fn tray_target_pinned_provider_wins_over_at_risk() {
    // Codex is far more at-risk, but the tray is pinned to Claude.
    let snap = two_section_snap(
        acct("c@x", Some(10.0), Some(10.0), true),
        Some(99.0),
        Some("claude".to_string()),
    );
    let (sec, _) = tray_target(&snap, snap.tray_icon_mode.as_deref()).expect("a target");
    assert_eq!(sec.provider_id, CLAUDE_SLUG);
}

#[test]
fn tray_target_pinned_missing_provider_falls_back_to_at_risk() {
    // Pinned to a provider that isn't captured → fall back to at-risk (Codex).
    let snap = two_section_snap(
        acct("c@x", Some(10.0), Some(10.0), true),
        Some(88.0),
        Some("gemini-cli".to_string()),
    );
    let (sec, _) = tray_target(&snap, snap.tray_icon_mode.as_deref()).expect("a target");
    assert_eq!(sec.provider_id, "codex");
}

#[test]
fn tray_countdown_uses_earliest_usable_account_then_waits_for_refresh() {
    let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
    let mut active = acct("active@x.com", Some(0.0), Some(100.0), true);
    active.weekly_reset_at = Some(now + chrono::Duration::days(5));
    active.fetched_at = Some(now);
    let mut next = acct("next@x.com", Some(100.0), Some(100.0), false);
    next.session_reset_at = Some(now + chrono::Duration::minutes(30));
    next.weekly_reset_at = Some(now + chrono::Duration::hours(2));
    next.fetched_at = Some(now);
    let mut snap = one_section_snap(active);
    snap.sections[0].accounts.push(next);
    snap.tray_icon_mode = Some("claude".into());
    with_now(now, || {
        assert_eq!(title_for(&snap), "🔒 2h 0m");
        assert!(tooltip_for(&snap).contains("next@x.com"));
    });
    with_now(now + chrono::Duration::hours(2), || {
        assert_eq!(title_for(&snap), "Refreshing…");
    });
    // Once fresh usage arrives a usable alternative prompts a switch;
    // the normal percentage returns after the switch is applied.
    let next = &mut snap.sections[0].accounts[1];
    next.windows[0].pct = Some(0.0);
    next.windows[1].pct = Some(0.0);
    next.fetched_at = Some(now + chrono::Duration::hours(2));
    with_now(now + chrono::Duration::hours(2), || {
        assert_eq!(title_for(&snap), "Switch")
    });
    snap.sections[0].accounts[0].active = false;
    snap.sections[0].accounts[1].active = true;
    with_now(now + chrono::Duration::hours(2), || {
        assert_eq!(title_for(&snap), "0%")
    });
}

#[test]
fn tray_target_ranks_a_missing_reading_below_zero_percent() {
    // Claude has no reading yet, Codex sits at exactly 0% → Codex is the
    // better "at risk" pick, whichever order the sections come in.
    let no_data = acct("c@x", None, None, true);
    let snap = two_section_snap(no_data, Some(0.0), None);
    let (sec, _) = tray_target(&snap, None).expect("a target");
    assert_eq!(sec.provider_id, "codex");

    let snap = two_section_snap(acct("c@x", Some(0.0), None, true), None, None);
    let (sec, _) = tray_target(&snap, None).expect("a target");
    assert_eq!(sec.provider_id, CLAUDE_SLUG);
}

#[test]
fn tray_icon_bytes_follow_the_target_provider_and_fall_back_to_the_mark() {
    let claude_icon = crate::icons::png16_for(CLAUDE_SLUG).expect("claude icon bundled");
    let codex_icon = crate::icons::png16_for("codex").expect("codex icon bundled");
    assert!(!claude_icon.is_empty() && claude_icon != codex_icon);

    // Pinned to each provider in turn.
    let snap = two_section_snap(
        acct("c@x", Some(10.0), None, true),
        Some(90.0),
        Some("claude".to_string()),
    );
    assert_eq!(tray_icon_bytes(&snap), claude_icon);
    let snap = two_section_snap(acct("c@x", Some(10.0), None, true), Some(90.0), None);
    assert_eq!(
        tray_icon_bytes(&snap),
        codex_icon,
        "at-risk target is codex"
    );

    // No target at all → the usagio mark.
    let mark = crate::icons::usagio_tray_icon();
    assert!(!mark.is_empty());
    assert_eq!(tray_icon_bytes(&Snapshot::default()), mark);
}

#[test]
fn tray_target_at_risk_picks_the_soonest_block() {
    // Claude 80% weekly at a fast pace blocks before Codex at 90% idle-ish.
    let claude = rated(
        acct("c@x", Some(10.0), Some(80.0), true),
        Some(0.5),
        Some(1.0),
    );
    let mut snap = two_section_snap(claude, Some(90.0), None);
    snap.sections[1].accounts[0].rate = crate::autoswap::AccountRate {
        session: Some(0.05),
        weekly: None,
        ..Default::default()
    };
    let (sec, _) = tray_target(&snap, None).unwrap();
    assert_eq!(sec.provider_id, CLAUDE_SLUG, "20 min vs 200 min to block");
}

#[test]
fn tray_icon_and_tooltip_follow_the_target_without_a_menu_change() {
    let mut snap = two_section_snap(acct("c@x", Some(10.0), Some(10.0), true), Some(99.0), None);
    let mut icon = None;
    let mut tip = String::new();
    assert!(tray_icon_update(&mut icon, &snap).is_some(), "first push");
    assert!(tray_tooltip_update(&mut tip, &snap).is_some(), "first push");
    assert!(tray_icon_update(&mut icon, &snap).is_none(), "unchanged");
    assert!(tray_tooltip_update(&mut tip, &snap).is_none(), "unchanged");
    snap.tray_icon_mode = Some(CLAUDE_SLUG.to_string());
    assert!(tray_icon_update(&mut icon, &snap).is_some(), "target moved");
    assert!(
        tray_tooltip_update(&mut tip, &snap).is_some(),
        "target moved"
    );
}
