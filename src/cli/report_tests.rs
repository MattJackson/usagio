use super::*;
use crate::history::history_path;
use crate::store::State;
use crate::test_support::codex_account;

// --- consumption_deltas (report same-account guard) ---

fn sample(account: &str, session: f64, ts: i64) -> Sample {
    Sample {
        ts,
        account: Some(account.to_string()),
        active: Some(true),
        session: Some(session),
        weekly: None,
        event: None,
        provider: None,
    }
}

#[test]
fn consumption_deltas_only_counts_same_account_increases() {
    let a1 = sample("a@e.com", 10.0, 100);
    let a2 = sample("a@e.com", 40.0, 200); // +30, same account -> counted
    let b1 = sample("b@e.com", 5.0, 300); // a->b switch -> NOT counted
    let b2 = sample("b@e.com", 20.0, 400); // +15, same account -> counted
    let a3 = sample("a@e.com", 45.0, 500); // b->a switch -> NOT counted
    let list: Vec<&Sample> = vec![&a1, &a2, &b1, &b2, &a3];
    assert_eq!(consumption_deltas(&list), vec![(200, 30.0), (400, 15.0)]);
}

#[test]
fn consumption_deltas_ignores_negative_deltas() {
    // A reset (session drops) is not consumption.
    let a1 = sample("a@e.com", 90.0, 100);
    let a2 = sample("a@e.com", 5.0, 200); // reset, negative -> skipped
    let list: Vec<&Sample> = vec![&a1, &a2];
    assert!(consumption_deltas(&list).is_empty());
}

#[test]
fn consumption_deltas_ignore_unchanged_readings() {
    let a = sample("a@e.com", 10.0, 100);
    let same = sample("a@e.com", 10.0, 200);
    let up = sample("a@e.com", 12.0, 300);
    let got = consumption_deltas(&[&a, &same, &up]);
    assert_eq!(got, vec![(300, 2.0)]);
}

#[test]
fn pace_report_covers_non_claude_accounts() {
    let mut state = State::default();
    state
        .provider_accounts_mut("codex")
        .accounts
        .push(codex_account("dev@example.com", None));
    let targets = pace_targets(&state);
    assert_eq!(
        targets,
        vec![("codex".to_string(), "dev@example.com".to_string())]
    );
}

#[test]
fn report_distinguishes_missing_history_and_tolerates_bad_utf8() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let err = cmd_report(&[]).unwrap_err().to_string();
    assert!(err.contains("no history yet"), "{err}");
    let path = history_path().unwrap();
    std::fs::create_dir_all(&path).unwrap(); // EISDIR, not "missing"
    let err = format!("{:#}", cmd_report(&[]).unwrap_err());
    assert!(!err.contains("no history yet"), "{err}");
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"\xff\xfe not json\n").unwrap();
    assert!(
        cmd_report(&[]).is_ok(),
        "one bad byte must not hide history"
    );
}

#[test]
fn report_counts_only_claude_swaps() {
    let swap = |provider: Option<&str>| Sample {
        event: Some("swap".into()),
        provider: provider.map(str::to_string),
        ..sample("a@e.com", 0.0, 1)
    };
    let samples = [swap(Some("codex")), swap(Some("claude")), swap(None)];
    assert_eq!(
        report_swap_count(&samples),
        2,
        "codex swaps entered the Claude report"
    );
}
