use super::*;
use chrono::{DateTime, Duration, Utc};

// --- bar ---

#[test]
fn bar_none_is_dash() {
    assert_eq!(bar(None), "-");
}

#[test]
fn bar_zero_percent() {
    assert_eq!(bar(Some(0.0)), "[----------]   0%");
}

#[test]
fn bar_fifty_percent() {
    assert_eq!(bar(Some(50.0)), "[#####-----]  50%");
}

#[test]
fn bar_hundred_percent() {
    assert_eq!(bar(Some(100.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_over_hundred() {
    assert_eq!(bar(Some(250.0)), "[##########] 100%");
}

#[test]
fn bar_clamps_negative() {
    assert_eq!(bar(Some(-5.0)), "[----------]   0%");
}

// --- truncate ---

#[test]
fn truncate_short_string_unchanged() {
    assert_eq!(truncate("hello", 10), "hello");
}

#[test]
fn truncate_exact_length_unchanged() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn truncate_long_string_adds_ellipsis() {
    let out = truncate("abcdefghij", 5);
    assert_eq!(out.chars().count(), 5);
    assert!(out.ends_with('…'));
    assert!(out.starts_with("abcd"));
}

// --- humanize_until ---

#[test]
fn humanize_until_past_is_now() {
    // Sub-minute / already-past reads "<1m", never "now"/"0m" — a countdown
    // flooring to 0 looked like it had already reset when it hadn't.
    assert_eq!(humanize_until(Utc::now() - Duration::hours(1)), "<1m");
    assert_eq!(humanize_until(Utc::now() + Duration::seconds(30)), "<1m");
}

#[test]
fn humanize_until_hours_and_minutes() {
    // 2h30m out (+ a few seconds of slack): shows hours and minutes.
    let s = humanize_until(Utc::now() + Duration::minutes(150) + Duration::seconds(5));
    assert_eq!(s, "2h 30m");
}

#[test]
fn humanize_until_days_and_hours() {
    let s = humanize_until(Utc::now() + Duration::hours(50) + Duration::seconds(5));
    assert_eq!(s, "2d 2h");
}

#[test]
fn humanize_until_minutes_only() {
    let s = humanize_until(Utc::now() + Duration::minutes(30) + Duration::seconds(5));
    assert_eq!(s, "30m");
}

// --- age_str ---

#[test]
fn age_str_never_without_timestamp() {
    assert_eq!(age_str(None), "never");
}

#[test]
fn age_str_minutes_and_hours() {
    let now = Utc::now().timestamp();
    assert_eq!(age_str(Some(now - 120)), "2m ago");
    assert_eq!(age_str(Some(now - 7200)), "2h ago");
}

// --- mutation-hardening: small formatting / path / CLI helpers ---

/// Retry `f` until the wall-clock second did not change while it ran, so a
/// boundary-exact age or countdown is measured exactly.
fn within_one_second<R>(mut f: impl FnMut(i64) -> R) -> R {
    for _ in 0..20 {
        let t0 = Utc::now().timestamp();
        let r = f(t0);
        if Utc::now().timestamp() == t0 {
            return r;
        }
    }
    panic!("wall clock never stable");
}

#[test]
fn age_str_switches_unit_exactly_at_each_boundary() {
    let at = |age: i64| within_one_second(|t0| age_str(Some(t0 - age)));
    assert_eq!(at(-100), "just now");
    assert_eq!(at(0), "0s ago");
    assert_eq!(at(59), "59s ago");
    assert_eq!(at(60), "1m ago");
    assert_eq!(at(3599), "59m ago");
    assert_eq!(at(3600), "1h ago");
    assert_eq!(at(86_399), "23h ago");
    assert_eq!(at(86_400), "1d ago");
    assert_eq!(at(3 * 86_400 + 5), "3d ago");
}

#[test]
fn humanize_until_reads_a_minute_out_as_one_minute() {
    let at = |ahead: i64| {
        within_one_second(|t0| humanize_until(DateTime::from_timestamp(t0 + ahead, 0).unwrap()))
    };
    assert_eq!(at(59), "<1m");
    assert_eq!(at(60), "1m");
}
