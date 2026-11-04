use super::*;

#[test]
fn parse_context_args_reads_both_flags_and_rejects_the_rest() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(parse_context_args(&[]).unwrap(), (None, None));
    assert_eq!(
        parse_context_args(&args(&["--provider", "claude"])).unwrap(),
        (Some("claude".to_string()), None)
    );
    assert_eq!(
        parse_context_args(&args(&["--project", "/tmp/p", "--provider", "codex"])).unwrap(),
        (
            Some("codex".to_string()),
            Some(std::path::PathBuf::from("/tmp/p"))
        )
    );
    assert!(parse_context_args(&args(&["--provider"])).is_err());
    assert!(parse_context_args(&args(&["--project"])).is_err());
    let err = parse_context_args(&args(&["--bogus"])).unwrap_err();
    assert!(format!("{err}").contains("unknown context option: --bogus"));
}
