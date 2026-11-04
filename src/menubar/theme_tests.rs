use super::*;

#[test]
fn theme_from_name_maps_os_aliases_and_rejects_unknown() {
    use muri::ThemeSource;
    assert!(matches!(
        theme_from_name("windows"),
        Some(ThemeSource::Windows(_))
    ));
    assert!(matches!(
        theme_from_name("WIN"),
        Some(ThemeSource::Windows(_))
    ));
    assert!(matches!(
        theme_from_name("mac"),
        Some(ThemeSource::MacOs(_))
    ));
    assert!(matches!(
        theme_from_name(" macos "),
        Some(ThemeSource::MacOs(_))
    ));
    assert!(matches!(
        theme_from_name("gnome"),
        Some(ThemeSource::Gnome(_))
    ));
    assert!(matches!(
        theme_from_name("linux"),
        Some(ThemeSource::Gnome(_))
    ));
    assert!(matches!(
        theme_from_name("system"),
        Some(ThemeSource::System(_))
    ));
    assert!(theme_from_name("beos").is_none());
    assert!(theme_from_name("").is_none());
}

#[test]
fn theme_override_args_set_the_forced_theme_and_tray_options_pin_min_width() {
    use muri::ThemeSource;
    // `FORCED_THEME` is a process-wide OnceLock, so this single test owns
    // the whole before/after sequence.
    assert!(forced_theme().is_none(), "no override before any args");
    set_theme_override_from_args(&["--theme".into(), "beos".into()]);
    assert!(forced_theme().is_none(), "an unknown name is ignored");
    set_theme_override_from_args(&["menubar".into(), "--theme=windows".into()]);
    assert!(matches!(forced_theme(), Some(ThemeSource::Windows(_))));
    // First valid override wins; later ones can't change a running tray.
    set_theme_override_from_args(&["--theme".into(), "gnome".into()]);
    assert!(matches!(forced_theme(), Some(ThemeSource::Windows(_))));

    assert_eq!(tray_options().min_width, Some(300.0));
}
