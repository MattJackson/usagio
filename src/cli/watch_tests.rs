use super::*;

#[test]
fn watch_flags_reject_bad_or_missing_values() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(parse_watch_opts(&args(&["--trigger", "8O"])).is_err());
    assert!(parse_watch_opts(&args(&["--ceiling"])).is_err());
    assert!(parse_watch_opts(&args(&["--interval", "x"])).is_err());
    let ok = parse_watch_opts(&args(&["--trigger", "80", "--interval", "30"])).unwrap();
    assert_eq!(ok.trigger, Some(80.0));
    assert_eq!(ok.base, 30);
}
