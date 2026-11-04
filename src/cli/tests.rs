use crate::cli::context::cmd_context;
use crate::cli::list::{cmd_list, render_table};
use crate::cli::report::{cmd_report, print_bars};
use crate::cli::token::cmd_token;
use crate::history::history_path;
use crate::providers::CLAUDE_SLUG;
use crate::rows::Cell;
use crate::store::State;
use crate::test_support::{
    account_with_cache, claude_account, claude_expired, codex_account, cu, prime_active_account,
    row_full, seed_claude_state, spawn_mock_token_server,
};
use crate::usage_log;
use chrono::{Duration, Utc};

// --- mutation-hardening: burn-rate projection, trackers, plan checks ---

/// Seed `history` for a Claude account: `n` snapshots 20 minutes apart ending
/// now, with the session / weekly percent given by `pct(age_hours)`.
fn seed_history(email: &str, n: usize, pct: impl Fn(f64, usize) -> (f64, f64)) {
    for i in 0..n {
        let age_h = (n - 1 - i) as f64 / 3.0;
        let (s, w) = pct(age_h, i);
        usage_log::append(&usage_log::Snapshot {
            ts: Utc::now() - Duration::seconds((age_h * 3600.0) as i64),
            provider: CLAUDE_SLUG.to_string(),
            account: email.to_string(),
            session_pct: Some(s as f32),
            weekly_pct: Some(w as f32),
            active_model: None,
            ..Default::default()
        })
        .unwrap();
    }
}

// --- mutation-hardening: process-level behaviour and CLI output ---

/// Re-run this test binary on `probe`, a test that does nothing unless the
/// parent set `USAGIO_PROBE_CLI`, and return what it printed. Used for code
/// whose observable result is text on stdout.
fn run_cli_probe(which: &str, envs: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--exact", "cli::tests::cli_output_probe", "--nocapture"])
        .env("USAGIO_PROBE_CLI", which);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "probe {which} failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("1 passed"),
        "probe {which} did not run:\n{text}"
    );
    text
}

/// The lines a probe printed between its `BEGIN` and `END` markers.
fn probe_lines(text: &str) -> Vec<String> {
    let begin = text.find("PROBE-BEGIN\n").expect("begin marker") + "PROBE-BEGIN\n".len();
    let end = text.find("PROBE-END").expect("end marker");
    text[begin..end].lines().map(str::to_string).collect()
}

fn history_line(ts: i64, account: &str, active: bool, session: f64, weekly: f64) -> String {
    serde_json::json!({
        "ts": ts, "account": account, "active": active,
        "session": session, "weekly": weekly,
    })
    .to_string()
}

/// Local weekday-from-Monday and hour for a timestamp, as `cmd_report` buckets.
fn local_bucket(ts: i64) -> (usize, usize) {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let dt = Local.timestamp_opt(ts, 0).single().unwrap();
    (
        dt.weekday().num_days_from_monday() as usize,
        dt.hour() as usize,
    )
}

/// A spread of timestamps (three days apart, odd hours) so weekday and hour
/// buckets differ between samples.
fn report_timestamps() -> Vec<i64> {
    (0..6)
        .map(|i| 1_700_000_000 + i * (3 * 86_400 + 5 * 3600 + 120))
        .collect()
}

/// Child half: runs one CLI renderer against fixtures and prints between markers.
#[test]
fn cli_output_probe() {
    let Ok(which) = std::env::var("USAGIO_PROBE_CLI") else {
        return;
    };
    let _cfg = crate::store::ScopedConfigDir::new();
    let reset = Utc::now() + Duration::hours(3) + Duration::seconds(30);
    println!("PROBE-BEGIN");
    match which.as_str() {
        "table" | "table-no-active" => {
            let mut live = row_full("live@e.com", 40.0, 80.0, reset);
            live.session.resets_at = Some(reset);
            live.opus = Some(Cell {
                pct: Some(25.0),
                resets_at: Some(reset),
            });
            let mut blank = row_full("blank@e.com", 0.0, 0.0, reset);
            blank.fetched_at = None;
            let mut lapsed = row_full("lapsed@e.com", 0.0, 0.0, reset);
            lapsed.no_subscription = true;
            let active = (which == "table").then_some("live@e.com");
            render_table(&[live, blank, lapsed], active);
        }
        "table-plain" => {
            render_table(
                &[row_full("solo@e.com", 10.0, 20.0, reset)],
                Some("solo@e.com"),
            );
        }
        "bars" => {
            let labels: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
            print_bars(&labels, &[10.0, 5.0, 0.0]);
            print_bars(&labels, &[0.0, 0.0, 0.0]);
        }
        "report-swaps" | "report-low" | "report-edge" => {
            let ts = report_timestamps();
            let (a_weekly, swap) = match which.as_str() {
                "report-swaps" => (91.0, true),
                "report-low" => (40.0, false),
                _ => (80.0, false),
            };
            let mut lines = Vec::new();
            for (i, t) in ts.iter().enumerate() {
                // Active account A climbs 4 points per sample.
                lines.push(history_line(*t, "a@e.com", true, 4.0 * i as f64, a_weekly));
                // B is never active: its jumps must not count as consumption.
                lines.push(history_line(
                    *t + 1,
                    "b@e.com",
                    false,
                    90.0 * (i % 2) as f64,
                    30.0,
                ));
            }
            // A drop in the active series (a reset) adds nothing.
            lines.push(history_line(ts[5] + 60, "a@e.com", true, 1.0, a_weekly));
            if swap {
                lines.push(
                    serde_json::json!({"ts": ts[2] + 5, "event": "swap", "active": true,
                        "account": "a@e.com", "session": 99.0})
                    .to_string(),
                );
            }
            std::fs::write(history_path().unwrap(), lines.join("\n") + "\n").unwrap();
            cmd_report(&[]).unwrap();
        }
        "report-empty" => {
            std::fs::write(history_path().unwrap(), "not json\n").unwrap();
            cmd_report(&[]).unwrap();
        }
        "pace-none" => {
            cmd_report(&["--pace".to_string()]).unwrap();
        }
        "pace" => {
            let mut a = claude_account("pace@e.com", 3_600_000);
            a.email = Some("pace@e.com".to_string());
            seed_claude_state(vec![a], None);
            seed_history("pace@e.com", 10, |age_h, _| {
                (60.0 - 10.0 * age_h, 80.0 - 4.0 * age_h)
            });
            cmd_report(&["--pace".to_string()]).unwrap();
        }
        "list" => {
            seed_claude_state(
                vec![account_with_cache("cl@e.com", Some(cu(10.0, 10.0, 60)))],
                None,
            );
            let mut st = State::load().unwrap();
            st.upsert_provider_account(
                "codex",
                codex_account("cx@e.com", Some(cu(20.0, 20.0, 60))),
            );
            st.save().unwrap();
            cmd_list(&[]).unwrap();
        }
        "list-empty" => {
            cmd_list(&[]).unwrap();
        }
        "token" => {
            let home = _cfg.home();
            crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
                std::fs::write(
                    home.join(".claude.json"),
                    serde_json::json!({"oauthAccount": {
                        "emailAddress": "duo@e.com", "accountUuid": "uuid-duo@e.com"}})
                    .to_string(),
                )
                .unwrap();
                let (base_url, _) = spawn_mock_token_server();
                crate::endpoints::set_test_override(
                    crate::endpoints::Endpoint::ClaudeToken,
                    Some(&format!("{base_url}/v1/oauth/token")),
                );
                // Inactive + expired: refreshed, and the NEW token is printed.
                seed_claude_state(vec![claude_expired("solo@e.com")], None);
                cmd_token(None).unwrap();
                // Active: the vendor slot's token is printed, not the stored one.
                let mut st = State::load().unwrap();
                st.upsert(claude_expired("duo@e.com"));
                st.active = Some("duo@e.com".to_string());
                st.save().unwrap();
                let mut slot = claude_account("duo@e.com", 3_600_000);
                slot.set_tokens("slot-at".into(), "slot-rt".into(), 9_999_999_999_999);
                prime_active_account(&slot);
                cmd_token(Some("duo")).unwrap();
            });
        }
        "context" => {
            let dir = tempfile::tempdir().unwrap();
            cmd_context(&[
                "--provider".to_string(),
                "claude".to_string(),
                "--project".to_string(),
                dir.path().to_string_lossy().to_string(),
            ])
            .unwrap();
        }
        other => panic!("unknown probe {other}"),
    }
    println!("PROBE-END");
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn token_command_prints_the_refreshed_or_adopted_bearer() {
    let text = run_cli_probe("token", &[]);
    assert_eq!(
        probe_lines(&text),
        ["mock-refreshed-access-token", "slot-at"],
        "stdout carries exactly the bearer, one per call"
    );
}

#[test]
fn table_marks_the_active_account_and_explains_rows_without_usage() {
    let text = run_cli_probe("table", &[]);
    let lines = probe_lines(&text);
    let row = |prefix: &str| {
        lines
            .iter()
            .find(|l| l.contains(prefix))
            .unwrap_or_else(|| panic!("no row for {prefix}:\n{}", lines.join("\n")))
            .clone()
    };
    let live = row("live@e.com");
    assert!(live.starts_with("▶  live@e.com"), "{live}");
    assert!(live.contains("40%") && live.contains("80%") && live.contains("25%"));
    assert!(live.contains("3h"), "reset countdown shown: {live}");
    let blank = row("blank@e.com");
    assert!(blank.starts_with("   blank@e.com"), "{blank}");
    assert!(blank.contains("no data yet"));
    let lapsed = row("lapsed@e.com");
    assert!(lapsed.starts_with("   lapsed@e.com"), "{lapsed}");
    assert!(lapsed.contains("no subscription (Free plan)"));
    assert!(lapsed.contains("re-checked every 30m"));
    // The Opus column only exists when some row has Opus usage.
    assert!(lines.iter().any(|l| l.contains("WEEKLY OPUS")));
    assert!(!lines
        .iter()
        .any(|l| l.contains("no active account tracked")));
    assert!(lines.iter().any(|l| l.contains("usagio list --refresh")));
}

#[test]
fn table_without_an_active_account_says_to_capture_and_omits_the_opus_column_when_unused() {
    let text = run_cli_probe("table-no-active", &[]);
    let lines = probe_lines(&text);
    assert!(lines
        .iter()
        .any(|l| l.contains("no active account tracked yet")));
    assert!(lines.iter().all(|l| !l.starts_with('▶')));
    let plain = probe_lines(&run_cli_probe("table-plain", &[]));
    assert!(!plain.iter().any(|l| l.contains("WEEKLY OPUS")));
    assert!(plain.iter().any(|l| l.starts_with("▶  solo@e.com")));
}

#[test]
fn bars_scale_to_the_largest_value() {
    let lines = probe_lines(&run_cli_probe("bars", &[]));
    let full = "█".repeat(30);
    let half = format!("{}{}", "█".repeat(15), " ".repeat(15));
    assert_eq!(lines[0], format!("  A    |{full}|  10.0"));
    assert_eq!(lines[1], format!("  B    |{half}|   5.0"));
    assert_eq!(lines[2], format!("  C    |{}|   0.0", " ".repeat(30)));
    // All-zero input never divides by zero and draws empty bars.
    assert_eq!(lines[3], format!("  A    |{}|   0.0", " ".repeat(30)));
}

/// `(label, value)` for the bar rows printed under `heading`.
fn bar_rows(lines: &[String], heading: &str, count: usize) -> Vec<(String, f64)> {
    let start = lines
        .iter()
        .position(|l| l.starts_with(heading))
        .expect(heading)
        + 1;
    lines[start..start + count]
        .iter()
        .map(|l| {
            let label = l.split_whitespace().next().unwrap().to_string();
            let value = l.rsplit('|').next().unwrap().trim().parse().unwrap();
            (label, value)
        })
        .collect()
}

#[test]
fn report_buckets_active_consumption_by_weekday_and_hour() {
    let lines = probe_lines(&run_cli_probe("report-swaps", &[]));
    let ts = report_timestamps();
    let mut by_weekday = [0.0_f64; 7];
    let mut by_hour = [0.0_f64; 24];
    for t in ts.iter().skip(1) {
        let (d, h) = local_bucket(*t);
        by_weekday[d] += 4.0;
        by_hour[h] += 4.0;
    }
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let got = bar_rows(&lines, "Consumption by weekday", 7);
    for (i, (label, v)) in got.iter().enumerate() {
        assert_eq!(label, days[i]);
        assert!(
            (v - by_weekday[i]).abs() < 0.06,
            "{label}: {v} vs {}",
            by_weekday[i]
        );
    }
    let got = bar_rows(&lines, "Consumption by hour", 24);
    for (h, (label, v)) in got.iter().enumerate() {
        assert_eq!(label, &format!("{h:02}"));
        assert!(
            (v - by_hour[h]).abs() < 0.06,
            "{label}: {v} vs {}",
            by_hour[h]
        );
    }
}

#[test]
fn report_summarises_samples_swaps_peaks_and_the_verdict() {
    let lines = probe_lines(&run_cli_probe("report-swaps", &[]));
    let text = lines.join("\n");
    // 6 active + 6 inactive + 1 reset sample + 1 swap event.
    assert!(text.contains("samples: 14   swaps: 1"), "{text}");
    assert!(text.contains("period:"));
    let peak = |who: &str| {
        lines
            .iter()
            .find(|l| l.trim_start().starts_with(who))
            .cloned()
            .unwrap()
    };
    assert!(peak("a@e.com").contains("91%"), "{}", peak("a@e.com"));
    assert!(peak("b@e.com").contains("30%"));
    assert!(text.contains("You hit 1 swap(s)"));
    assert!(!text.contains("never needed a swap"));

    let low = probe_lines(&run_cli_probe("report-low", &[])).join("\n");
    assert!(low.contains("swaps: 0"));
    assert!(
        low.contains("One account peaked at only 40% weekly"),
        "{low}"
    );

    // Exactly 80% is no longer "only": with no swaps it was close to enough.
    let edge = probe_lines(&run_cli_probe("report-edge", &[])).join("\n");
    assert!(edge.contains("never needed a swap"), "{edge}");
    assert!(!edge.contains("peaked at only"));
}

#[test]
fn report_without_usable_history_says_so() {
    assert!(run_cli_probe("report-empty", &[]).contains("No usage samples recorded yet."));
}

#[test]
fn pace_report_lists_each_account_with_its_forecast() {
    assert!(run_cli_probe("pace-none", &[]).contains("No accounts captured yet"));
    let text = probe_lines(&run_cli_probe("pace", &[])).join("\n");
    assert!(text.contains("Burn-rate forecast"), "{text}");
    assert!(text.contains("pace@e.com"));
    assert!(
        text.contains("Session") || text.contains("session"),
        "{text}"
    );
    assert!(text.contains("Weekly") || text.contains("weekly"), "{text}");
}

#[test]
fn list_prints_each_providers_accounts_under_its_own_heading() {
    let text = probe_lines(&run_cli_probe("list", &[])).join("\n");
    assert!(text.contains("cl@e.com"));
    assert!(text.contains("\nCodex:\n"), "{text}");
    assert!(text.contains("cx@e.com"));
    let empty = probe_lines(&run_cli_probe("list-empty", &[])).join("\n");
    assert!(empty.contains("No accounts yet."));
}

#[test]
fn context_command_prints_a_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let text = run_cli_probe("context", &[("HOME", tmp.path().to_str().unwrap())]);
    assert!(text.to_lowercase().contains("context"), "{text}");
}
