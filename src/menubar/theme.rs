//! Forced tray theme (`usagio menubar --theme`) for UI audits.

/// The forced tray theme for the debug/QA switcher, set once by
/// [`set_theme_override_from_args`] before any tray thread spawns and read at
/// every `TrayIconBuilder` site. Holds the validated `--theme` name (not a
/// `ThemeSource`, which isn't `Clone`), so `forced_theme` re-maps it per call.
/// A process-global `OnceLock` rather than an env var: the repo lint disallows
/// `std::env::set_var`, and there is no need to leak this into the environment.
pub(super) static FORCED_THEME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Map a `--theme <name>` token to muri's [`ThemeSource`](muri::ThemeSource).
/// `windows`/`macos`/`gnome` force that OEM look on any host; `system`
/// follows the host (the default). Returns `None` for an unknown name.
pub(super) fn theme_from_name(name: &str) -> Option<muri::ThemeSource> {
    use muri::{ThemeMode, ThemeSource};
    match name.trim().to_ascii_lowercase().as_str() {
        "windows" | "win" => Some(ThemeSource::Windows(ThemeMode::Auto)),
        "macos" | "mac" => Some(ThemeSource::MacOs(ThemeMode::Auto)),
        "gnome" | "linux" | "adwaita" => Some(ThemeSource::Gnome(ThemeMode::Auto)),
        "system" | "host" | "auto" => Some(ThemeSource::System(ThemeMode::Auto)),
        _ => None,
    }
}

/// The forced tray theme, if the process was launched with a valid
/// `--theme <os>` override. Applied at each platform's `TrayIconBuilder` via
/// `with_theme`. muri 0.10.5 forced themes render the *target* OS font (#54),
/// so this is the switcher for auditing all three OEM looks from one host —
/// windows-on-mac is meant to match windows-on-windows. `None` = follow host.
pub(crate) fn forced_theme() -> Option<muri::ThemeSource> {
    FORCED_THEME.get()?.as_deref().and_then(theme_from_name)
}

/// Shared popup layout for both the live tray and the headless capture, so a
/// `__render_shot` PNG matches what the menu bar actually draws. usagio's menu
/// is text-dense (email addresses + `S% / W%` values); the auto-fit width came
/// out cramped next to a native OEM menu (measured ~244pt vs the Time Machine
/// menu's ~310pt), so we pin a wider minimum. `min_width` is in logical points;
/// muri still grows past it for longer rows and clamps the theme/gutter defaults
/// otherwise. The live tray layers its host/forced `theme` on top via
/// `Tray::theme` (which only overwrites the theme field, not this width).
pub(crate) fn tray_options() -> muri::MenuOptions {
    muri::MenuOptions::default().min_width(300.0)
}

/// Parse a `menubar --theme <name>` flag (if present) and record it for the
/// tray builders. Called once from `main`, before any tray thread spawns. An
/// unknown name is reported and ignored (falls through to the host theme).
pub(crate) fn set_theme_override_from_args(args: &[String]) {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = match a.as_str() {
            "--theme" => it.next().map(String::as_str),
            other => other.strip_prefix("--theme="),
        };
        if let Some(name) = name {
            if theme_from_name(name).is_some() {
                let _ = FORCED_THEME.set(Some(name.to_string()));
            } else {
                eprintln!(
                    "usagio: unknown --theme '{name}' (use windows|macos|gnome|system); \
                     falling back to the host theme"
                );
            }
            return;
        }
    }
}

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;
