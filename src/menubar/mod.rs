//! macOS menu-bar app: shows the active account's usage in the status bar and
//! lets you switch / capture / remove accounts and set the auto-swap threshold
//! from a dropdown. The same `watch_cycle` that powers `usagio watch` runs
//! on a background thread here, so the daemon behaviour is identical.
//!
//! Usage numbers come from the cache (written by the scheduler poll); the UI
//! never fetches on its own, so menu interactions can't trigger HTTP 429s.
//!
//! Menu wiring iterates `providers::all()` and emits one `ProviderSection` per
//! provider — but only if that provider has at least one captured account in
//! state (see `build_snapshot`). The "Capture current login ▸" submenu always
//! shows, with one row per registered provider that can capture a login
//! (`supports_usage == false` stubs are hidden; see
//! `partition_capture_providers`).

use anyhow::Result;
#[cfg(target_os = "macos")]
use std::cell::RefCell;

// macOS run-loop deps. `objc2`/`objc2-app-kit`/`objc2-foundation`/`block2` are
// `[target.'cfg(target_os = "macos")'.dependencies]` in Cargo.toml — they don't
// exist in the dependency graph on Linux/Windows, so this group needs real
// `#[cfg(target_os = "macos")]`, not just an unused-import allow. macOS drives a
// native `NSApplication` run loop + `NSTimer` tick (poll/relaunch/drain menu
// clicks) while muri's tray runs passively (`TrayIconBuilder::build()` spawns a
// live OS tray). The tray menu itself is now rendered through the SAME muri
// `muda-compat` facade (`muri::compat::tray_icon`) Windows/Linux already use —
// macOS renders the tray menu through muri's compat facade too, not a native
// `NSMenu` — the only macOS-specific machinery left is the run loop below.
#[cfg(target_os = "macos")]
use {
    crate::providers,
    actions::{handle_click, run_ui_tasks},
    block2::RcBlock,
    model::build_snapshot,
    muri::{MenuEvent, Tray},
    objc2::MainThreadMarker,
    objc2_app_kit::{NSApplication, NSApplicationActivationPolicy},
    objc2_foundation::NSTimer,
    render::{menu_signature, title_for},
    signin::sweep_sign_in_stores_later,
    supervisor::{maybe_relaunch_after_upgrade, poll_loop_supervisor},
    theme::{forced_theme, tray_options},
    tray::{tooltip_for, tray_tooltip_update},
};

pub(crate) mod actions;
pub(crate) mod backup;
mod binding;
mod cross_platform;
pub(crate) mod model;
pub(crate) mod poller;
#[cfg(all(target_os = "macos", feature = "custom-popup"))]
mod popover;
pub(crate) mod render;
pub(crate) mod signin;
pub(crate) mod supervisor;
pub(crate) mod theme;
pub(crate) mod tray;

/// Entry point dispatched by `usagio menubar` (see `main.rs`). Every platform
/// renders the tray menu through the same pipeline
/// (`cross_platform::menu_tree_from_snapshot` then `platform::render::render_menu`).
/// macOS drives its own `NSApplication`/`NSTimer` run loop (below) to pump
/// events; Linux/Windows drive the menu event loop via the
/// `platform::MenuBackend` trait instead (`cross_platform::run`, near the end
/// of this file).
#[cfg(not(target_os = "macos"))]
pub(crate) fn run() -> Result<()> {
    cross_platform::run()
}

#[cfg(target_os = "macos")]
pub(crate) fn run() -> Result<()> {
    // Register providers on the main thread before anything else — the poll
    // thread and menu build both dispatch through `providers::get`.
    providers::init();

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the menu bar must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // Background (menu-bar-only) app: no Dock icon, even as the bare binary.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Poll + auto-swap on a background thread, supervised against panics
    // (see `poll_loop_supervisor`'s doc). It writes cached usage to
    // state.json; the main-thread timer reads it back to render.
    std::thread::spawn(poll_loop_supervisor);

    // Remember the binary we launched from so the timer can notice a `brew
    // upgrade` replacing it and relaunch into the new version.
    let start_exe = std::fs::canonicalize(crate::cli::install::stable_exe_path()).ok();

    // Build the tray on the main thread and keep it alive for the app's lifetime.
    // muri 0.11: the native `Tray` API (customization lives here, not in the
    // frozen muda-compat facade). `spawn` installs the `NSStatusItem` and
    // returns a `Clone` `TrayHandle` without blocking, so usagio keeps driving
    // its own `NSApplication`/`NSTimer` loop below; the status item shows the
    // fixed usagio mark + the `%` title side-by-side (muri #58); the title and
    // tooltip, not the icon, say which agent the percentage is for.
    let initial = build_snapshot();
    let mut tray = Tray::new(muri::Icon::from_png(
        crate::icons::usagio_tray_icon().to_vec(),
    ))
    .title(title_for(&initial))
    .tooltip(tooltip_for(&initial))
    .menu(crate::platform::render::render_menu(
        &cross_platform::menu_tree_from_snapshot(&initial),
    ))
    .options(tray_options());
    if let Some(theme) = forced_theme() {
        tray = tray.theme(theme);
    }
    let muri_mtm = muri::MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the tray must spawn on the main thread"))?;
    let handle = tray
        .spawn(muri_mtm)
        .map_err(|e| anyhow::anyhow!("failed to create tray icon: {e}"))?;

    // custom-popup: build the NSPopover host anchored to the status-item button
    // and listen for tray left-clicks to toggle it. `handle_click` is reused
    // verbatim for row actions (same click-id scheme as the native menu).
    #[cfg(feature = "custom-popup")]
    let popover = popover::build_popover_host(&handle, mtm);
    #[cfg(feature = "custom-popup")]
    let tray_rx = muri::compat::tray_icon::TrayIconEvent::receiver().clone();
    #[cfg(feature = "custom-popup")]
    let shown_on_launch = RefCell::new(false);
    // A retained clone for the tick closure (which takes `app` by move); the
    // outer `app` is still needed for `app.run()` below.
    #[cfg(feature = "custom-popup")]
    let app_popover = app.clone();

    // All UI updates happen in this timer, scheduled in the DEFAULT run-loop mode.
    // A status menu opens its own nested tracking loop (NSEventTrackingRunLoopMode);
    // a default-mode timer never fires there, so an open menu is never dismissed.
    // (This is exactly the tao bug we sidestepped by dropping tao: it added its
    // run-loop observer to kCFRunLoopCommonModes, which *does* fire during
    // tracking and collapsed the menu.) Each tick rebuilds the display from cached
    // state — so "updated Xm ago" ticks and a CLI switch shows up within ~1s — and
    // only re-installs the menu/title when the rendered content actually changes.
    let menu_rx = MenuEvent::receiver().clone();
    let last_sig = RefCell::new(menu_signature(&initial));
    let last_title = RefCell::new(title_for(&initial));
    let last_tooltip = RefCell::new(tooltip_for(&initial));
    let tick = RcBlock::new(move |_t: core::ptr::NonNull<NSTimer>| {
        // If `brew upgrade` replaced our binary, relaunch into the new version.
        if let Some(start) = &start_exe {
            maybe_relaunch_after_upgrade(start);
        }
        // Work a worker handed back to the main thread (a sign-in window).
        run_ui_tasks();
        // A menu closes before its click is delivered, so handling it here won't
        // fight menu tracking.
        while let Ok(ev) = menu_rx.try_recv() {
            handle_click(&ev.id.0);
        }
        let snap = build_snapshot();
        let sig = menu_signature(&snap);
        if *last_sig.borrow() != sig {
            // Re-render the muri tray menu on change. With custom-popup ON this
            // is the right-click fallback (the popover rebuilds its own content
            // from a fresh snapshot each time it's shown); with it OFF this is
            // the sole UI.
            handle.set_menu(crate::platform::render::render_menu(
                &cross_platform::menu_tree_from_snapshot(&snap),
            ));
            *last_sig.borrow_mut() = sig;
        }
        if let Some(tip) = tray_tooltip_update(&mut last_tooltip.borrow_mut(), &snap) {
            handle.set_tooltip(Some(tip));
        }
        let title = title_for(&snap);
        if *last_title.borrow() != title {
            handle.set_title(Some(title.clone()));
            *last_title.borrow_mut() = title;
        }
        // custom-popup: toggle the NSPopover on a tray left-click, and (for the
        // screenshot path) force-show it once on launch.
        #[cfg(feature = "custom-popup")]
        if let Some(p) = &popover {
            while let Ok(ev) = tray_rx.try_recv() {
                if let muri::compat::tray_icon::TrayIconEvent::Click {
                    button: muri::compat::tray_icon::MouseButton::Left,
                    button_state: muri::compat::tray_icon::MouseButtonState::Down,
                    ..
                } = ev
                {
                    p.toggle(&popover::popover_model(&snap));
                }
            }
            if !*shown_on_launch.borrow() {
                *shown_on_launch.borrow_mut() = true;
                if std::env::var("USAGIO_POPOVER_SHOW_ON_LAUNCH").is_ok() {
                    eprintln!("[popover] show-on-launch: activating + showing");
                    app_popover.activate();
                    p.show(&popover::popover_model(&snap));
                    eprintln!("[popover] shown={}", p.is_shown());
                }
            }
        }
    });
    // Startup sweep of sign-in stores left by accounts removed elsewhere
    // (`usagio rm`) or by a run that quit mid sign-in.
    sweep_sign_in_stores_later();
    // The run loop retains the timer; scheduled timers fire in the default mode.
    let _timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.75, true, &tick) };

    app.run();
    Ok(())
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
