use super::*;

#[test]
fn render_shot_writes_a_png_for_a_theme_and_needs_an_output_path() {
    let _g = crate::store::ScopedConfigDir::new();
    assert!(cmd_render_shot(&["system".to_string()]).is_err());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("shot.png");
    cmd_render_shot(&[
        "macos".to_string(),
        out.to_string_lossy().to_string(),
        "1.0".to_string(),
    ])
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "round-trips the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn secrets_selftest_round_trips_through_the_secret_store() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    cmd_secrets_selftest(&args(&["usagio-selftest-svc", "acct", "s3cret"])).unwrap();
    assert!(cmd_secrets_selftest(&args(&["only", "two"])).is_err());
    assert!(cmd_secrets_selftest(&[]).is_err());
}
