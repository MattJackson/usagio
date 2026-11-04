//! custom-popup (macOS, off by default): the `NSPopover` host and the
//! snapshot-to-`ui::PopoverModel` fold the AppKit renderer draws.

use objc2::MainThreadMarker;

use super::model::{now_utc, provider_grouped_order, Severity, Snapshot};
use super::render::trailing_for_account;

/// custom-popup: build the `NSPopover` host anchored to the tray's status-item
/// button, wiring row clicks straight into `handle_click` (same click-id scheme
/// as the native menu, so no action logic is duplicated). Returns `None` if the
/// status item / button isn't available (no anchor → no popover; the tray icon
/// still shows, it just won't open a popup).
pub(super) fn build_popover_host(
    handle: &muri::TrayHandle,
    mtm: MainThreadMarker,
) -> Option<crate::ui::popover::PopoverHost> {
    // muri's native tray does not (yet) expose the underlying `NSStatusItem`
    // button as an NSPopover anchor, so there is no button to attach to. The
    // experimental, off-by-default custom-popup is therefore inert until muri
    // exposes a status-item anchor — return `None` so the tray still shows (it
    // just opens muri's own styled popup rather than this NSPopover).
    let _ = (handle, mtm);
    None
}

/// custom-popup: fold a `Snapshot` into the toolkit-neutral `ui::PopoverModel`
/// the AppKit renderer draws. Reuses the SAME data derivations as the native
/// menu — `provider_grouped_order`, `trailing_for_account` (locked countdown /
/// severity spans), the active flag, and the `switch:<provider>:<key>` click
/// ids — so the popover and the native menu can never disagree about content.
///
/// Phase 1: an account row's click switches to that account (or is a no-op when
/// it's already active / the provider can't switch). Detail rows (reset / burn /
/// cost) are Phase 2. "Capture current login" and "Settings" are rendered as
/// rows but route to Phase-2 stub ids (the native submenu trees aren't rebuilt
/// yet); "Refresh usage now" and "Quit" are fully wired.
pub(super) fn popover_model(snap: &Snapshot) -> crate::ui::PopoverModel {
    use crate::ui::{PctSpan, PopoverModel, PopoverRow, Sev};

    let sev_of = |s: Severity| match s {
        Severity::Amber => Sev::Amber,
        Severity::Red => Sev::Red,
    };

    let mut rows: Vec<PopoverRow> = Vec::new();
    let groups = provider_grouped_order(snap);
    let mut first = true;
    for (si, account_idxs) in &groups {
        let sec = &snap.sections[*si];
        if !first {
            rows.push(PopoverRow::Separator);
        }
        first = false;
        rows.push(PopoverRow::GroupHeader {
            title: sec.display_name.to_string(),
            icon_slug: Some(sec.provider_id),
        });
        for ai in account_idxs {
            let a = &sec.accounts[*ai];
            let (trailing, rel_colors) = trailing_for_account(a, sec.severity_bands, now_utc());
            let colors = rel_colors
                .into_iter()
                .map(|(start, len, sev)| PctSpan {
                    start,
                    len,
                    sev: sev_of(sev),
                })
                .collect();
            // Same routing as the native menu's account submenu: active → no
            // switch; switching-capable & inactive → `switch:<provider>:<key>`.
            let click_id = if a.active || !sec.supports_switching {
                "noop".to_string()
            } else {
                format!("switch:{}:{}", sec.provider_id, a.key)
            };
            rows.push(PopoverRow::Account {
                email: a.display.clone(),
                active: a.active,
                trailing,
                colors,
                click_id,
            });
        }
    }

    if snap.sections.is_empty() {
        rows.push(PopoverRow::Action {
            label: "Capture a login below to begin".to_string(),
            click_id: "noop".to_string(),
        });
    }

    rows.push(PopoverRow::Separator);
    // Phase 1 bottom actions. Capture/Settings are submenu trees in the native
    // menu; Phase 1 surfaces them as rows routing to inert ids (Phase 2 ports
    // the trees / opens a secondary popover). Refresh + Quit are fully wired to
    // the same ids `handle_click` already routes.
    rows.push(PopoverRow::Action {
        label: "Capture current login…".to_string(),
        click_id: "popup:capture".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Settings…".to_string(),
        click_id: "popup:settings".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Refresh usage now".to_string(),
        click_id: "refresh:now".to_string(),
    });
    rows.push(PopoverRow::Action {
        label: "Quit".to_string(),
        click_id: "quit".to_string(),
    });

    PopoverModel { rows }
}
