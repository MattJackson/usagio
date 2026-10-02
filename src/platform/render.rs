//! The single IR→muri translator, shared by every platform.
//!
//! usagio builds the tray menu once as a generic, `Send`-able [`MenuTree`]
//! (`menubar::menu_tree_from_snapshot`) and this turns it into a live
//! **native** `muri::Menu` on the platform's UI thread. As of muri 0.11 the
//! `muda-compat` facade is a frozen, pure `muda`/`tray-icon` drop-in with NO
//! muri-only styling (bold, per-severity value colors, icons) — all of that
//! lives in the native API (`muri::menu::{Menu, Row, Item, Icon}` +
//! `Row::bold`/`Row::value_color`), which usagio now targets directly. The
//! resulting `Menu` is fed to a native `muri::Tray` and to the headless
//! offscreen renderer (`muri::render_menu_to_png`) alike.
//!
//! Threading: build on the platform's UI thread (macOS main thread, the GTK
//! thread on Linux, the tray UI thread on Windows). The `Send` [`MenuTree`] is
//! what crosses any channel; muri is only ever constructed here, on the far
//! side.

use super::{MenuItem, MenuTree, ValueColor, ValueSpan};
use muri::{Color, Icon, Item, Menu, MenuId, Row, Segment, StyleRun, Weight};

/// Map the platform-agnostic [`ValueColor`] (severity band) to muri's `Color`
/// for `Row::value_color`. Red = "about to hit the wall", Amber = "approaching
/// it".
fn muri_color(c: ValueColor) -> Color {
    match c {
        ValueColor::Red => Color::SystemRed,
        ValueColor::Amber => Color::SystemOrange,
    }
}

/// A bundled provider PNG → a native muri `Icon`. muri owns PNG decoding
/// (lazy, at paint time); this is just a wrapper.
fn menu_icon(bytes: &[u8]) -> Icon {
    Icon::from_png(bytes.to_vec())
}

/// Translate the generic [`MenuTree`] into a live **native** `muri::Menu`.
/// Call on the platform's UI thread (see the module doc).
pub(crate) fn render_menu(tree: &MenuTree) -> Menu {
    build_menu(&tree.items)
}

fn build_menu(items: &[MenuItem]) -> Menu {
    let mut menu = Menu::new();
    for item in items {
        menu = append(menu, item);
    }
    menu
}

fn append(menu: Menu, item: &MenuItem) -> Menu {
    match item {
        MenuItem::Action {
            id,
            label,
            icon_png,
            enabled,
            checked,
            checkable,
        } => {
            // A `\t` in the label splits it into a grow label + a right-aligned
            // value (e.g. `"Quit\tusagio v0.6.1"`) — the same flush-right value
            // the account rows use, so the version reads at the right edge
            // rather than mashed onto the label.
            let base = Row::new(id.as_str()).enabled(*enabled);
            let mut row = match label.split_once('\t') {
                Some((l, v)) => base.label_value(l.trim_end(), v.trim_start()),
                None => base.label(label),
            };
            // Only checkable rows reserve the check column; a plain action must
            // not (native `checked` marks the row as a checkbox).
            if *checkable {
                row = row.checked(*checked);
            }
            if let Some(png) = icon_png {
                row = row.leading(menu_icon(png));
            }
            menu.item(Item::Row(row))
        }
        MenuItem::Static { label, icon_png } => {
            // A non-interactive header row (the per-provider "Claude"/"Codex"
            // group header, drawn dimmed) — carries the provider mark when set.
            let mut row = Row::label_only(label);
            if let Some(png) = icon_png {
                row = row.leading(menu_icon(png));
            }
            menu.section_header(row)
        }
        MenuItem::Info { label, color } => {
            let mut seg = Segment::grow(label.as_str());
            if let Some(c) = color {
                let len = label.encode_utf16().count();
                seg = seg.runs(vec![StyleRun::new(0, len, muri_color(*c))]);
            }
            menu.item(Item::Row(Row::new(MenuId::none()).segment(seg)))
        }
        MenuItem::Separator => menu.separator(),
        MenuItem::Submenu {
            label,
            items,
            active,
            value_spans,
            ..
        } => {
            let label_row = submenu_label(label, *active, value_spans);
            menu.submenu(label_row, build_menu(items))
        }
    }
}

/// Build a submenu's parent row. usagio's account label is `"name\tvalue"`; the
/// tab splits it into a grow label + a right-aligned value segment so the value
/// aligns and can be tinted per severity. The active account renders **bold +
/// accent forecolor** (bold alone is too subtle at 13pt through the glass; color
/// reads clearly) — no checkmark → no leading gutter — and its `S% / W%` value
/// carries the severity color.
fn submenu_label(label: &str, active: bool, value_spans: &[ValueSpan]) -> Row {
    // Non-interactive parent row (id = none): the submenu opens the flyout;
    // the actionable rows live inside it.
    let base = Row::new(MenuId::none());
    match label.split_once('\t') {
        Some((name, value)) => {
            // `value_spans` offsets are UTF-16, relative to `value` (the text
            // after the `\t`) — see `menubar::value_spans_of`. Color each span
            // as its own `StyleRun`, so session and weekly tint independently.
            let mut vseg = Segment::trailing_value(value);
            if !value_spans.is_empty() {
                let runs = value_spans
                    .iter()
                    .map(|s| StyleRun::new(s.start, s.len, muri_color(s.color)))
                    .collect();
                vseg = vseg.runs(runs);
            }
            let mut nseg = Segment::grow(name);
            if active {
                // Whole-name accent-colored bold run marks the active account.
                let len = name.encode_utf16().count();
                nseg = nseg.runs(vec![
                    StyleRun::new(0, len, Color::Accent).weight(Weight::Bold)
                ]);
            }
            base.segment(nseg).segment(vseg)
        }
        None => {
            let row = base.label(label);
            if active {
                row.bold()
            } else {
                row
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(items: Vec<MenuItem>) -> MenuTree {
        MenuTree { items }
    }

    #[test]
    fn render_menu_translates_every_item_in_order() {
        let t = tree(vec![
            MenuItem::Static {
                label: "Claude".into(),
                icon_png: None,
            },
            MenuItem::Action {
                id: "refresh:now".into(),
                label: "Refresh".into(),
                icon_png: None,
                enabled: true,
                checked: false,
                checkable: false,
            },
            MenuItem::Separator,
            MenuItem::Info {
                label: "note".into(),
                color: None,
            },
        ]);
        let menu = render_menu(&t);
        assert_eq!(menu.len(), 4);
        assert!(matches!(menu.items[0], Item::SectionHeader(_)));
        match &menu.items[1] {
            Item::Row(r) => {
                assert_eq!(r.id.as_str(), "refresh:now");
                assert_eq!(r.segments[0].text, "Refresh");
            }
            other => panic!("expected row, got {other:?}"),
        }
        assert!(matches!(menu.items[2], Item::Separator));
        assert!(matches!(menu.items[3], Item::Row(_)));
        assert!(render_menu(&tree(vec![])).is_empty());
    }

    #[test]
    fn submenu_children_are_built_recursively() {
        let t = tree(vec![MenuItem::Submenu {
            label: "a@b.c\t10%".into(),
            icon_png: None,
            items: vec![MenuItem::Separator, MenuItem::Separator],
            active: false,
            value_spans: vec![],
        }]);
        let menu = render_menu(&t);
        match &menu.items[0] {
            Item::Submenu { menu, .. } => assert_eq!(menu.len(), 2),
            other => panic!("expected submenu, got {other:?}"),
        }
    }

    #[test]
    fn submenu_label_splits_name_and_value_with_span_runs() {
        let spans = [
            ValueSpan {
                start: 0,
                len: 3,
                color: ValueColor::Amber,
            },
            ValueSpan {
                start: 6,
                len: 3,
                color: ValueColor::Red,
            },
        ];
        let row = submenu_label("me@x.io\t85% / 95%", false, &spans);
        assert!(row.id.is_none());
        assert_eq!(row.segments.len(), 2);
        assert_eq!(row.segments[0].text, "me@x.io");
        assert!(row.segments[0].runs.is_empty());
        assert_eq!(row.segments[1].text, "85% / 95%");
        assert_eq!(
            row.segments[1].runs,
            vec![
                StyleRun::new(0, 3, Color::SystemOrange),
                StyleRun::new(6, 3, Color::SystemRed),
            ]
        );
    }

    #[test]
    fn submenu_label_without_spans_has_no_value_runs() {
        let row = submenu_label("me@x.io\t10% / 20%", false, &[]);
        assert_eq!(row.segments[1].text, "10% / 20%");
        assert!(row.segments[1].runs.is_empty());
    }

    #[test]
    fn submenu_label_active_name_is_bold_accent() {
        let row = submenu_label("é@x.io\t1%", true, &[]);
        let len = "é@x.io".encode_utf16().count();
        assert_eq!(
            row.segments[0].runs,
            vec![StyleRun::new(0, len, Color::Accent).weight(Weight::Bold)]
        );
    }

    #[test]
    fn submenu_label_without_tab_is_a_plain_label() {
        let row = submenu_label("Settings", false, &[]);
        assert!(row.id.is_none());
        assert_eq!(row.segments.len(), 1);
        assert_eq!(row.segments[0].text, "Settings");
    }
}
