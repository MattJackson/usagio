//! The single menu builder, shared by every platform. It emits the generic,
//! Send-able `platform::MenuTree` (content only — rows, submenus, ids, active /
//! value-color directives); `platform::render::render_menu` turns that into a
//! live muri menu on each platform's UI thread. muri renders identically on
//! macOS/Windows/Linux, so there is exactly ONE description of the menu and ONE
//! IR→muri translator — no per-OS menu builder. The `run()` + redraw pump at the
//! end of this module are the only Linux/Windows-specific pieces (the
//! `MenuBackend`/channel plumbing); macOS drives the same `menu_tree_from_snapshot`
//! from its own main-thread loop in `menubar/mod.rs`.

use super::model::{
    at_risk_checked, provider_grouped_order, AcctView, ProviderSection, Severity, Snapshot,
};
use super::render::{
    account_extra_info_rows, account_header_row, account_submenu_rows, provider_group_header_row,
    quit_row_plain, reset_grant_row, submenu_info_items, u16len, RowStyle,
};
use crate::platform::{MenuItem as PMenuItem, MenuTree, ValueColor};
use crate::providers;
use crate::providers::CaptureMode;
#[cfg(not(target_os = "macos"))]
use {
    super::actions::handle_click,
    super::model::build_snapshot,
    super::render::{menu_signature, title_for},
    super::signin::sweep_sign_in_stores_later,
    super::supervisor::{maybe_relaunch_after_upgrade, poll_loop_supervisor},
    super::tray::{tray_icon_bytes, tray_icon_update},
    crate::platform::MenuHandle,
    anyhow::Result,
    std::time::Duration,
};

/// A `RowStyle`'s plain title. The `\t` is passed THROUGH so muri's
/// muda-compat two-column layout right-aligns the trailing value column.
fn plain_text(style: &RowStyle) -> String {
    style.plain.clone()
}

/// Map usagio's severity band to the platform-agnostic `ValueColor` the
/// menu tree carries (backends translate it to muri's `Color`).
fn value_color_of(sev: Severity) -> crate::platform::ValueColor {
    match sev {
        Severity::Red => crate::platform::ValueColor::Red,
        Severity::Amber => crate::platform::ValueColor::Amber,
    }
}

/// Per-span value tints for an account header row, RELATIVE to the value
/// segment (after the `\t`). Native muri colors each span independently, so
/// session and weekly can carry DIFFERENT bands (e.g. `85%` amber next to
/// `95%` red) instead of the whole value collapsing to the most-severe one
/// (which `row_value_severity` did for the old single-color compat API).
/// Empty for a healthy/uncolored row.
fn value_spans_of(header: &RowStyle) -> Vec<crate::platform::ValueSpan> {
    let base = header
        .plain
        .split_once('\t')
        .map(|(l, _)| u16len(l) + 1)
        .unwrap_or(0);
    header
        .colors
        .iter()
        .filter_map(|&(off, len, sev)| {
            off.checked_sub(base)
                .map(|start| crate::platform::ValueSpan {
                    start,
                    len,
                    color: value_color_of(sev),
                })
        })
        .collect()
}

/// A plain (non-account) submenu: never the active account, no value tint.
/// Only account header rows carry `active`/`value_color`;
/// the Settings / Capture / provider-group submenus are structural.
fn plain_submenu(label: impl Into<String>, items: Vec<PMenuItem>) -> PMenuItem {
    PMenuItem::Submenu {
        label: label.into(),
        icon_png: None,
        items,
        active: false,
        value_spans: Vec::new(),
    }
}

/// An informational submenu row (reset windows, burn-rate, "updated Xm
/// ago"). Rendered `enabled: true` with a no-op click id: a disabled row
/// draws greyed/dimmed, which the maintainer flagged as unreadable, and
/// muri has no rich-text to re-tint a disabled row back to full contrast —
/// so full-contrast + a harmless no-op click wins over a dimmed row.
/// A non-clickable (never highlighted) detail line, optionally tinted.
fn info_row(label: impl Into<String>, color: Option<ValueColor>) -> PMenuItem {
    PMenuItem::Info {
        label: label.into(),
        color,
    }
}

fn action(id: impl Into<String>, label: impl Into<String>, enabled: bool) -> PMenuItem {
    PMenuItem::Action {
        id: id.into(),
        label: label.into(),
        icon_png: None,
        enabled,
        checked: false,
        checkable: false,
    }
}

fn checkbox(
    id: impl Into<String>,
    label: impl Into<String>,
    enabled: bool,
    checked: bool,
) -> PMenuItem {
    PMenuItem::Action {
        id: id.into(),
        label: label.into(),
        icon_png: None,
        enabled,
        checked,
        checkable: true,
    }
}

/// One account's block: a `PMenuItem::Submenu` whose label is the compact
/// account row and whose children are the detail + action rows. Each
/// provider group holds one such submenu per account.
fn build_account_submenu_item(sec: &ProviderSection, a: &AcctView) -> PMenuItem {
    let mut items = Vec::new();
    let rows = account_submenu_rows(sec, a);
    for (row, locked) in submenu_info_items(sec, a) {
        let ready_reset = rows.claim_reset_row
            && a.reported.reset_grants.iter().any(|g| {
                g.is_available()
                    && g.usable_now
                    && reset_grant_row(g).as_deref() == Some(row.as_str())
            });
        if ready_reset {
            items.push(action(
                format!("claim:{}:{}", sec.provider_id, a.key),
                row,
                true,
            ));
        } else {
            items.push(info_row(row, locked.then_some(ValueColor::Red)));
        }
    }
    for row in account_extra_info_rows(sec, a) {
        items.push(info_row(row, None));
    }
    // Account actions.
    items.push(PMenuItem::Separator);
    if rows.switch_row {
        items.push(action(
            format!("switch:{}:{}", sec.provider_id, a.key),
            "Switch to this account",
            true,
        ));
    }
    if rows.renew_row {
        items.push(action(
            format!("renew:{}:{}", sec.provider_id, a.key),
            "Renew login…",
            true,
        ));
    }
    if rows.remove_row {
        items.push(action(
            format!("remove:{}:{}", sec.provider_id, a.key),
            "Remove…",
            true,
        ));
    }
    let header = account_header_row(sec, a);
    PMenuItem::Submenu {
        label: plain_text(&header),
        icon_png: None,
        items,
        // The active account renders bold + leading checkmark.
        // `account_header_row` already folds `a.active` into `bold`.
        active: header.bold,
        // Tint the trailing `S% / W%` value segment per severity.
        value_spans: value_spans_of(&header),
    }
}

/// The provider-group header row: the provider's display name plus its
/// 16px icon, as a static section header — identical for every provider
/// and every state (no submenu, no hover highlight). Label comes from
/// `provider_group_header_row` so the header text has a single source.
pub(super) fn build_provider_group_item(sec: &ProviderSection) -> PMenuItem {
    PMenuItem::Static {
        label: provider_group_header_row(sec).plain,
        icon_png: crate::icons::png16_for(sec.provider_id).map(|b| b.to_vec()),
    }
}

/// THE menu builder — one generic `platform::MenuTree` describing the
/// whole tray dropdown, used by every platform (macOS's main-thread loop
/// and the Linux/Windows redraw pump both call this, then hand the result
/// to `platform::render::render_menu`). Same click ids `handle_click`
/// parses regardless of renderer. v0.5.3 menu redesign: provider grouping
/// — each provider with captured accounts contributes a static
/// header row (an env override rides inline in its label), followed by one
/// `PMenuItem::Submenu` per account whose children are the details + action rows.
/// `PMenuItem::Separator` sits between provider groups, not between
/// individual accounts.
pub(super) fn menu_tree_from_snapshot(snap: &Snapshot) -> MenuTree {
    let mut items = Vec::new();
    if snap.sections.is_empty() {
        items.push(action("none", "Capture a login below to begin", false));
    }
    let groups = provider_grouped_order(snap);
    let mut first_group = true;
    for (si, account_idxs) in &groups {
        let sec = &snap.sections[*si];
        if !first_group {
            items.push(PMenuItem::Separator);
        }
        first_group = false;
        items.push(build_provider_group_item(sec));
        for ai in account_idxs {
            let a = &sec.accounts[*ai];
            items.push(build_account_submenu_item(sec, a));
        }
    }
    items.push(PMenuItem::Separator);

    // Capture current login ▸ — creds-on-disk providers first, then
    // API-key providers, both as DIRECT children (item 8 flattened the
    // "Paste API key ▸" sub-submenu).
    let mut capture_items = Vec::new();
    if snap.capture_creds.is_empty() && snap.capture_api_key.is_empty() {
        capture_items.push(action("noop", "(no providers registered)", false));
    } else {
        for reg in snap.capture_creds.iter().chain(snap.capture_api_key.iter()) {
            let title = reg.display_name.to_string();
            let prefix = match reg.capture_mode {
                CaptureMode::CredsOnDisk => "capture",
                CaptureMode::ApiKey => "apikey",
            };
            capture_items.push(action(format!("{prefix}:{}", reg.provider_id), title, true));
        }
    }
    // Sign in to a new account — every provider usagio can log in itself.
    let mut signin_items = Vec::new();
    for p in providers::all().iter().filter(|p| p.login_flow().is_some()) {
        signin_items.push(action(
            format!("signin:{}", p.provider_id()),
            p.display_name(),
            true,
        ));
    }
    items.push(plain_submenu("Sign in to a new account", signin_items));
    items.push(plain_submenu("Capture current login", capture_items));

    // Settings ▸: Refresh usage now, then a separator, then everything
    // else alphabetical (Auto-swap, Backups, Notifications) — item 9
    // flattened the old "Advanced ▸" grouping and item 5 folded the
    // top-level auto-swap checkbox into the Auto-swap ▸ submenu.
    let notifications = vec![
        checkbox(
            "notifications:threshold",
            crate::notifications::threshold_alert_label(),
            true,
            snap.notification_config.threshold_enabled,
        ),
        checkbox(
            "notifications:resetback",
            "Window reset alerts",
            true,
            snap.notification_config.reset_back_enabled,
        ),
        checkbox(
            "notifications:pace",
            "Weekly pace projection (experimental)",
            true,
            snap.notification_config.pace_enabled,
        ),
    ];

    let cur = if snap.autoswap {
        snap.threshold.round() as i32
    } else {
        0
    };
    let mut autoswap_items = vec![checkbox("autoswap:off", "Off", true, cur == 0)];
    for t in [70i32, 85, 95, 98] {
        autoswap_items.push(checkbox(
            format!("autoswap:{t}"),
            format!("{t}%"),
            true,
            cur == t,
        ));
    }
    autoswap_items.push(PMenuItem::Separator);
    autoswap_items.push(checkbox(
        "autoswap:credits",
        "Use credits when every plan is used up",
        true,
        snap.use_credits,
    ));
    autoswap_items.push(PMenuItem::Separator);
    autoswap_items.push(action("autoswap:now", "Switch to best account now", true));

    let backups = vec![
        action("backup:save", "Save…", true),
        action("backup:restore", "Restore…", true),
    ];

    // Tray Icon ▸: "At risk" (default) or pin to one captured provider.
    // `snap.sections` already holds only providers with a captured
    // account, so the pinned options list exactly the agents the user has.
    let cur_mode = snap.tray_icon_mode.as_deref();
    let mut trayicon_items = vec![checkbox(
        "trayicon:at-risk",
        "At risk (closest to limit)",
        true,
        at_risk_checked(snap),
    )];
    for sec in &snap.sections {
        trayicon_items.push(checkbox(
            format!("trayicon:{}", sec.provider_id),
            sec.display_name,
            true,
            cur_mode == Some(sec.provider_id),
        ));
    }

    let settings_items = vec![
        action("refresh:now", "Refresh usage now", true),
        PMenuItem::Separator,
        plain_submenu("Auto-swap", autoswap_items),
        plain_submenu("Tray Icon", trayicon_items),
        plain_submenu("Backups", backups),
        plain_submenu("Notifications", notifications),
    ];
    items.push(plain_submenu("Settings", settings_items));

    items.push(PMenuItem::Separator);
    items.push(action(
        "quit",
        plain_text(&RowStyle::plain_row(quit_row_plain())),
        true,
    ));

    MenuTree { items }
}

/// 16x16 PNG bytes for the initial tray icon. `MenuBackend::create_status_item`
/// requires real, decodable icon bytes on Linux/Windows (unlike macOS,
/// which is happy with a text-only title). This follows `tray_target`
/// (the at-risk or pinned provider's icon), falling back to the bundled
/// usagio mark when there is no target — see `tray_icon_bytes`.
#[cfg(not(target_os = "macos"))]
fn initial_icon_bytes(snap: &Snapshot) -> &'static [u8] {
    // The tray icon reflects the current `tray_target` agent (Settings ▸
    // Tray Icon: "at risk" or a pinned provider), so the menu bar shows
    // WHICH agent the percentage is for. This reverses the earlier
    // v0.5.22 "usagio mark only" choice now that multiple agents
    // (Claude + Codex + …) can be captured and the tray must disambiguate.
    // Falls back to the usagio mark when there's no target/icon.
    tray_icon_bytes(snap)
}

/// Background redraw ticker: rebuilds the tray from cached state and
/// pushes updates through the `Send`-safe `MenuHandle`, mirroring the
/// macOS `NSTimer` tick in `run` above but off the (blocked)
/// `run_event_loop` thread instead of on it — `create_status_item` and
/// `run_event_loop` must share a thread (see `MenuBackend`'s doc in
/// `platform/mod.rs`), so this can't run on the main thread here.
#[cfg(not(target_os = "macos"))]
fn redraw_loop(handle: Box<dyn MenuHandle>, initial: Snapshot) {
    let mut last_sig = menu_signature(&initial);
    let mut last_title = title_for(&initial);
    let mut last_icon = Some(initial_icon_bytes(&initial));
    let start_exe = std::fs::canonicalize(crate::cli::install::stable_exe_path()).ok();
    loop {
        std::thread::sleep(Duration::from_millis(750));
        if let Some(start) = &start_exe {
            maybe_relaunch_after_upgrade(start);
        }
        let snap = build_snapshot();
        let sig = menu_signature(&snap);
        if sig != last_sig {
            if let Err(e) = handle.set_menu(menu_tree_from_snapshot(&snap)) {
                crate::logging::log(&format!("menubar: set_menu failed: {e:#}"));
            }
            last_sig = sig;
        }
        if let Some(icon) = tray_icon_update(&mut last_icon, &snap) {
            if let Err(e) = handle.set_icon(icon) {
                crate::logging::log(&format!("menubar: set_icon failed: {e:#}"));
            }
        }
        let title = title_for(&snap);
        if title != last_title {
            match handle.set_title(&title) {
                Ok(()) => last_title = title,
                Err(e) => {
                    crate::logging::log(&format!("menubar: set_title failed: {e:#}"));
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn run() -> Result<()> {
    providers::init();
    let backend = crate::platform().menu();
    backend.on_click(Box::new(handle_click))?;

    let initial = build_snapshot();
    let handle = backend.create_status_item(&title_for(&initial), initial_icon_bytes(&initial))?;
    handle.set_menu(menu_tree_from_snapshot(&initial))?;
    let _ = handle.set_title(&title_for(&initial));

    // Poll + auto-swap on a background thread — same `poll_loop`
    // (supervised against panics; see `poll_loop_supervisor`'s doc) the
    // macOS `run` above uses (writes cached usage to state.json; nothing
    // here calls into the native tray, so it's safe off-thread).
    std::thread::spawn(poll_loop_supervisor);
    // Redraw ticker on its own background thread (see `redraw_loop`'s
    // doc for why it can't be the main thread here).
    std::thread::spawn(move || redraw_loop(handle, initial));
    // Startup sweep of orphaned sign-in stores (runs on the UI thread).
    sweep_sign_in_stores_later();

    // Blocks, pumping the GTK (Linux) / Win32 (Windows) message loop —
    // see `platform::linux::LinuxMenu` / `platform::windows::WindowsMenu`.
    backend.run_event_loop()
}

// `initial_icon_bytes` is private to this module, so its test lives here.

#[cfg(test)]
#[path = "cross_platform_tests.rs"]
mod tests;
