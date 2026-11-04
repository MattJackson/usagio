//! `usagio report`: usage patterns and the burn-rate forecast.

use anyhow::{anyhow, bail, Result};
use chrono::Utc;

use crate::display::bar;
use crate::history::history_path;
use crate::providers::CLAUDE_SLUG;
use crate::store::State;

#[derive(serde::Deserialize)]
pub(crate) struct Sample {
    pub(crate) ts: i64,
    #[serde(default)]
    pub(crate) account: Option<String>,
    #[serde(default)]
    pub(crate) active: Option<bool>,
    #[serde(default)]
    pub(crate) session: Option<f64>,
    #[serde(default)]
    pub(crate) weekly: Option<f64>,
    #[serde(default)]
    pub(crate) event: Option<String>,
    /// Provider of a swap event (absent on older Claude-only events).
    #[serde(default)]
    pub(crate) provider: Option<String>,
}

/// Swap events `cmd_report` counts. Its usage samples are Claude-only
/// (`watch_cycle`), so swaps of other providers stay out of its verdict.
pub(crate) fn report_swap_count(samples: &[Sample]) -> usize {
    samples
        .iter()
        .filter(|s| s.event.as_deref() == Some("swap"))
        .filter(|s| s.provider.as_deref().is_none_or(|p| p == CLAUDE_SLUG))
        .count()
}

/// Positive session-% deltas between consecutive SAME-account active samples —
/// the quantity `cmd_report` buckets by weekday/hour. A delta across an account
/// switch would subtract two unrelated accounts' percentages, so those are
/// skipped; only increases count. Returns `(timestamp, delta)`. Pure, for tests.
pub(crate) fn consumption_deltas(active: &[&Sample]) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let mut prev: Option<&Sample> = None;
    for s in active {
        if let Some(p) = prev {
            if p.account == s.account {
                if let (Some(pv), Some(cur)) = (p.session, s.session) {
                    let delta = cur - pv;
                    if delta > 0.0 {
                        out.push((s.ts, delta));
                    }
                }
            }
        }
        prev = Some(s);
    }
    out
}

pub(crate) fn cmd_report(args: &[String]) -> Result<()> {
    // Analytics subflavours share the same command surface so `report` stays
    // the one-stop CLI for usage insight. `--pace` prints per-account burn-
    // rate forecasts. With no flag we fall through to the classic
    // weekday/hour histogram.
    if args.iter().any(|a| a == "--pace") {
        return cmd_report_pace();
    }

    use chrono::{Datelike, Local, TimeZone, Timelike};

    let path = history_path()?;
    let data = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no history yet — run `usagio watch` (or `install`) to collect it")
        }
        Err(e) => return Err(anyhow!(e).context(format!("reading {}", path.display()))),
    };
    let samples: Vec<Sample> = data
        .lines()
        .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
        .collect();
    if samples.is_empty() {
        println!("No usage samples recorded yet.");
        return Ok(());
    }

    let mut active: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.event.is_none() && s.active == Some(true))
        .collect();
    active.sort_by_key(|s| s.ts);

    let mut by_weekday = [0f64; 7];
    let mut by_hour = [0f64; 24];
    for (ts, delta) in consumption_deltas(&active) {
        if let Some(dt) = Local.timestamp_opt(ts, 0).single() {
            by_weekday[dt.weekday().num_days_from_monday() as usize] += delta;
            by_hour[dt.hour() as usize] += delta;
        }
    }

    let mut peak: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for s in &samples {
        if let (Some(a), Some(w)) = (&s.account, s.weekly) {
            let e = peak.entry(a.clone()).or_insert(0.0);
            if w > *e {
                *e = w;
            }
        }
    }
    let swaps = report_swap_count(&samples);
    let span_start = samples
        .first()
        .and_then(|s| Local.timestamp_opt(s.ts, 0).single());
    let span_end = samples
        .last()
        .and_then(|s| Local.timestamp_opt(s.ts, 0).single());

    println!("\nUsage report (Claude accounts)");
    if let (Some(a), Some(b)) = (span_start, span_end) {
        println!(
            "  period: {} → {}",
            a.format("%Y-%m-%d %H:%M"),
            b.format("%Y-%m-%d %H:%M")
        );
    }
    println!("  samples: {}   swaps: {swaps}\n", samples.len());

    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    println!("Consumption by weekday (relative):");
    print_bars(
        &days.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &by_weekday,
    );
    println!("\nConsumption by hour of day (relative):");
    let hours: Vec<String> = (0..24).map(|h| format!("{h:02}")).collect();
    print_bars(&hours, &by_hour);

    println!("\nPeak weekly utilization per account:");
    for (email, p) in &peak {
        println!("  {:<28} {}", email, bar(Some(*p)));
    }
    let maxpeak = peak.values().cloned().fold(0.0_f64, f64::max);
    println!();
    if maxpeak < 80.0 {
        println!(
            "One account peaked at only {maxpeak:.0}% weekly — a single subscription likely covers your usage."
        );
    } else if swaps == 0 {
        println!("You approached your weekly limit but never needed a swap — one account is close to enough.");
    } else {
        println!("You hit {swaps} swap(s) — multiple accounts are earning their keep.");
    }
    println!();
    Ok(())
}

// --- report --pace ---------------------------------------------------------
//
// Reuses the burn_rate module, so the CLI stays a thin renderer over the same
// data the menu bar consumes.

/// `(slug, key)` of every captured account `report --pace` covers: Claude's
/// (those with an email) then every other provider's.
pub(crate) fn pace_targets(state: &State) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = state
        .accounts
        .iter()
        .filter_map(|a| a.email.clone())
        .map(|e| (CLAUDE_SLUG.to_string(), e))
        .collect();
    for (slug, pa) in &state.providers {
        if slug != CLAUDE_SLUG {
            out.extend(pa.accounts.iter().map(|a| (slug.clone(), a.key.clone())));
        }
    }
    out
}

pub(crate) fn cmd_report_pace() -> Result<()> {
    use crate::providers::trait_def::Window;
    use crate::usage_log::AccountKey;
    let state = State::load()?;
    let targets = pace_targets(&state);
    if targets.is_empty() {
        println!("No accounts captured yet — run `usagio capture` first.");
        return Ok(());
    }
    println!("\nBurn-rate forecast");
    println!("  Projection from recent usage history, not a guarantee.\n");
    let now = Utc::now();
    for (slug, email) in &targets {
        let key = AccountKey::new(slug, email);
        if slug == CLAUDE_SLUG {
            println!("  {email}");
        } else {
            println!("  {slug}:{email}");
        }
        for window in [Window::Session, Window::Weekly] {
            if let Some(est) = crate::burn_rate::estimate(&key, window, now) {
                println!("    {}", crate::burn_rate::format_menu_row(&est));
            }
        }
    }
    println!();
    Ok(())
}

pub(crate) fn print_bars(labels: &[String], values: &[f64]) {
    let max = values.iter().cloned().fold(0.0_f64, f64::max).max(1e-9);
    for (label, v) in labels.iter().zip(values.iter()) {
        let filled = ((v / max) * 30.0).round() as usize;
        let b: String = "█".repeat(filled) + &" ".repeat(30 - filled);
        println!("  {label:<4} |{b}| {v:>5.1}");
    }
}
