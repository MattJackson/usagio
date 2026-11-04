//! Internal, undocumented subcommands (`__secrets_selftest`, `__render_shot`).

use anyhow::{anyhow, bail, Context, Result};

use crate::{menubar, platform, providers};

/// Internal, undocumented CLI hook used ONLY by
/// `tests/integration_linux_secrets.rs` (and any equivalent test on other
/// OSes) to round-trip `Platform::secrets()` against whatever the real
/// backend is on this machine — the real D-Bus Secret Service daemon on
/// Linux CI, Keychain on macOS, Credential Manager on Windows. This crate
/// has no `[lib]` target, so an integration test can't call `LinuxSecrets`
/// directly the way a unit test in `src/platform/linux.rs` can; this
/// subcommand is the black-box seam (same shape as every other
/// `tests/cli.rs` test driving the compiled binary as a subprocess).
///
/// Deliberately not listed in `print_help` — it exists purely as a test
/// fixture, not a user-facing feature. Round-trips a caller-supplied
/// `(service, account, secret)` through delete → get(None) → set →
/// get(Some) → delete → get(None), printing `OK` and exiting 0 on success,
/// or returning an `Err` (non-zero exit, message on stderr) describing
/// exactly which step diverged.
///
/// Usage: `usagio __secrets_selftest <service> <account> <secret>`
/// Headless website-screenshot renderer. Builds the menu from the state.json
/// this process's config dir points at (a fixture, in the capture pipeline)
/// and renders it to a PNG under a forced OS theme via muri's offscreen
/// renderer — no tray/window/display/TCC. Deliberately not in `print_help`;
/// it's a build-pipeline seam, not a user feature.
///
/// Usage: `usagio __render_shot <windows|macos|gnome|system> <out.png> [scale]`
/// (default scale 2.0). A forced theme renders that OEM look on any host, so
/// all three OS shots can be produced from a single runner.
pub(crate) fn cmd_render_shot(args: &[String]) -> Result<()> {
    let theme = args.first().map(String::as_str).unwrap_or("system");
    let out = args
        .get(1)
        .ok_or_else(|| anyhow!("usage: __render_shot <theme> <out.png> [scale]"))?;
    let scale: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2.0);
    providers::init();
    let png = menubar::render_menu_png_for_theme(theme, scale);
    std::fs::write(out, &png).with_context(|| format!("writing {out}"))?;
    println!(
        "wrote {} bytes to {out} (theme={theme}, scale={scale})",
        png.len()
    );
    Ok(())
}

pub(crate) fn cmd_secrets_selftest(args: &[String]) -> Result<()> {
    let (service, account, secret) = match args {
        [service, account, secret] => (service.as_str(), account.as_str(), secret.as_str()),
        _ => bail!("usage: usagio __secrets_selftest <service> <account> <secret>"),
    };
    let store = platform().secrets();

    // Start from a clean slate in case a previous run crashed mid-round-trip
    // and left a stale entry behind under this test-scoped service name.
    let _ = store.delete(service, account);
    if store.get(service, account)?.is_some() {
        bail!("secrets_selftest: secret unexpectedly present before set()");
    }

    store.set(service, account, secret)?;
    let got = store.get(service, account)?;
    if got.as_deref() != Some(secret) {
        bail!("secrets_selftest: get() after set() returned {got:?}, expected Some({secret:?})");
    }

    store.delete(service, account)?;
    if store.get(service, account)?.is_some() {
        bail!("secrets_selftest: secret still present after delete()");
    }

    println!("OK");
    Ok(())
}
