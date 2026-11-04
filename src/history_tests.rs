use super::*;
use crate::test_support::{history_events, row_full};
use chrono::{Duration, Utc};

#[test]
fn history_events_are_appended_one_json_line_each() {
    let _g = crate::store::ScopedConfigDir::new();
    assert!(history_path().unwrap().ends_with("history.jsonl"));
    log_event(&serde_json::json!({"event": "first", "n": 1}));
    log_event(&serde_json::json!({"event": "second", "n": 2}));
    let events = history_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "first");
    assert_eq!(events[1]["n"], 2);
}

#[test]
fn append_history_records_only_accounts_with_a_reading() {
    let _g = crate::store::ScopedConfigDir::new();
    let reset = Utc::now() + Duration::hours(5);
    let mut blank = row_full("blank@e.com", 1.0, 1.0, reset);
    blank.fetched_at = None;
    let rows = vec![
        row_full("active@e.com", 30.0, 40.0, reset),
        row_full("other@e.com", 5.0, 6.0, reset),
        blank,
    ];
    append_history(&rows, Some("active@e.com"));
    let events = history_events();
    assert_eq!(events.len(), 2);
    let active = events
        .iter()
        .find(|e| e["account"] == "active@e.com")
        .unwrap();
    assert_eq!(active["active"], true);
    assert_eq!(active["session"], 30.0);
    assert_eq!(active["weekly"], 40.0);
    let other = events
        .iter()
        .find(|e| e["account"] == "other@e.com")
        .unwrap();
    assert_eq!(other["active"], false);
}
