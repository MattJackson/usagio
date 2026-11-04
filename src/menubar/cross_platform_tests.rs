#[cfg(not(target_os = "macos"))]
use super::{initial_icon_bytes, Snapshot};
use crate::menubar::cross_platform;
use crate::menubar::test_support::{acct, one_section_snap, reset_fixture, two_section_snap};
use crate::providers;

#[test]
fn reset_ready_row_is_clickable_only_for_claude() {
    let _g = crate::store::ScopedConfigDir::new();
    use crate::platform::MenuItem;
    for provider in ["claude", "codex"] {
        for ready in [false, true] {
            let mut a = acct("gift@example.com", Some(10.0), Some(20.0), true);
            a.provider_id = provider;
            a.reported.reset_grants = vec![reset_fixture(ready)];
            let mut snap = one_section_snap(a);
            snap.sections[0].provider_id = provider;
            let tree = cross_platform::menu_tree_from_snapshot(&snap);
            let MenuItem::Submenu { items, .. } = &tree.items[1] else {
                panic!("account submenu missing")
            };
            let claim_rows: Vec<_> = items
                .iter()
                .filter_map(|item| match item {
                    MenuItem::Action {
                        id, label, enabled, ..
                    } if id.starts_with("claim:") => Some((id, label, enabled)),
                    _ => None,
                })
                .collect();
            assert_eq!(claim_rows.len(), usize::from(provider == "claude" && ready));
            if let Some((id, label, enabled)) = claim_rows.first() {
                assert_eq!(id.as_str(), "claim:claude:gift@example.com");
                assert_eq!(label.as_str(), "🎁 1 reset ready");
                assert!(**enabled);
            }
            assert!(!items.iter().any(
                |item| matches!(item, MenuItem::Action { label, .. } if label == "Claim reset…" || label == "✓ Active")
            ));
        }
    }
}

// --- cross-platform menu tree ------------------------------------------

fn find_submenu<'a>(
    items: &'a [crate::platform::MenuItem],
    wanted: &str,
) -> &'a [crate::platform::MenuItem] {
    items
        .iter()
        .find_map(|i| match i {
            crate::platform::MenuItem::Submenu { label, items, .. } if label == wanted => {
                Some(items.as_slice())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no submenu {wanted:?}"))
}

fn checked_of(items: &[crate::platform::MenuItem], wanted_id: &str) -> bool {
    items
        .iter()
        .find_map(|i| match i {
            crate::platform::MenuItem::Action { id, checked, .. } if id == wanted_id => {
                Some(*checked)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no action {wanted_id:?}"))
}

#[test]
fn menu_tree_separates_provider_groups_but_does_not_lead_with_one() {
    // The menu builder reads the usage log for burn-rate rows.
    let _g = crate::store::ScopedConfigDir::new();
    use crate::platform::MenuItem;
    let snap = two_section_snap(acct("c@x", Some(40.0), Some(30.0), true), Some(90.0), None);
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    assert!(
        matches!(tree.items[0], MenuItem::Static { .. }),
        "group header first"
    );
    assert!(matches!(tree.items[1], MenuItem::Submenu { .. }));
    assert!(
        matches!(tree.items[2], MenuItem::Separator),
        "between groups"
    );
    assert!(matches!(tree.items[3], MenuItem::Static { .. }));
}

#[test]
fn signin_is_one_provider_submenu_and_classic_is_absent() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let snap = one_section_snap(acct("a@x", Some(10.0), Some(10.0), true));
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    let signin = find_submenu(&tree.items, "Sign in to a new account");
    for provider in providers::all().iter().filter(|p| p.login_flow().is_some()) {
        assert!(signin.iter().any(|item| matches!(item, crate::platform::MenuItem::Action { id, label, .. } if id == &format!("signin:{}", provider.provider_id()) && label == provider.display_name())));
    }
    let auto = find_submenu(find_submenu(&tree.items, "Settings"), "Auto-swap");
    assert!(!auto.iter().any(|item| matches!(item, crate::platform::MenuItem::Action { id, .. } if id == "autoswap:engine")));
}

#[test]
fn menu_tree_capture_submenu_lists_providers_when_either_bucket_has_one() {
    // The menu builder reads the usage log for burn-rate rows.
    let _g = crate::store::ScopedConfigDir::new();
    use crate::platform::MenuItem;
    // One_section_snap has a creds-on-disk provider and no API-key ones.
    let snap = one_section_snap(acct("a@x.com", Some(1.0), Some(1.0), true));
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    let capture = find_submenu(&tree.items, "Capture current login");
    assert_eq!(capture.len(), 1);
    match &capture[0] {
        MenuItem::Action {
            id, label, enabled, ..
        } => {
            assert_eq!(id, "capture:claude");
            assert_eq!(label, "Claude");
            assert!(*enabled);
        }
        other => panic!("expected a capture action, got {other:?}"),
    }
    // Nothing registered at all → an explanatory disabled row.
    let mut empty = one_section_snap(acct("a@x.com", Some(1.0), Some(1.0), true));
    empty.capture_creds.clear();
    let tree = cross_platform::menu_tree_from_snapshot(&empty);
    let capture = find_submenu(&tree.items, "Capture current login");
    assert!(matches!(
        &capture[0],
        MenuItem::Action { label, enabled: false, .. } if label == "(no providers registered)"
    ));
}

#[test]
fn menu_tree_checks_the_current_autoswap_threshold_and_tray_mode() {
    // The menu builder reads the usage log for burn-rate rows.
    let _g = crate::store::ScopedConfigDir::new();
    let mut snap = two_section_snap(
        acct("c@x", Some(40.0), Some(30.0), true),
        Some(90.0),
        Some("codex".to_string()),
    );
    snap.autoswap = true;
    snap.threshold = 85.0;
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    let settings = find_submenu(&tree.items, "Settings");
    let auto = find_submenu(settings, "Auto-swap");
    assert!(checked_of(auto, "autoswap:85"));
    for id in ["autoswap:off", "autoswap:70", "autoswap:95", "autoswap:98"] {
        assert!(!checked_of(auto, id), "{id} must not be checked");
    }
    let tray = find_submenu(settings, "Tray Icon");
    assert!(checked_of(tray, "trayicon:codex"));
    assert!(!checked_of(tray, "trayicon:claude"));
    assert!(!checked_of(tray, "trayicon:at-risk"));

    // Auto-swap off, tray following the at-risk agent.
    snap.autoswap = false;
    snap.tray_icon_mode = None;
    let tree = cross_platform::menu_tree_from_snapshot(&snap);
    let settings = find_submenu(&tree.items, "Settings");
    assert!(checked_of(
        find_submenu(settings, "Auto-swap"),
        "autoswap:off"
    ));
    assert!(!checked_of(
        find_submenu(settings, "Auto-swap"),
        "autoswap:85"
    ));
    assert!(checked_of(
        find_submenu(settings, "Tray Icon"),
        "trayicon:at-risk"
    ));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn initial_icon_falls_back_to_the_usagio_mark_without_a_target() {
    let mark = crate::icons::usagio_tray_icon();
    assert!(!mark.is_empty());
    assert_eq!(initial_icon_bytes(&Snapshot::default()), mark);
}
