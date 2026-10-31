// burn_rate.rs — per-window per-account forecast of when a quota will be exhausted.
//
// Consumes samples persisted by src/usage_log.rs (Snapshot { ts, provider, account,
// session_pct, weekly_pct, active_model }).  Weighted linear regression, recent
// samples weighted higher.  Requires at least MIN_SAMPLES within the current window;
// slope <= FLAT_SLOPE_THRESHOLD is treated as "rate flat" (below noise floor).
//
// Wire into menu redesign as a DetailRow beneath the corresponding Session/Weekly row
// for the report and menu displays.

use chrono::{DateTime, Duration, Utc};

use crate::providers::trait_def::Window;
use crate::usage_log::{self, AccountKey, Snapshot};

/// Minimum samples inside the current window required before we'll fit a rate.
pub const MIN_SAMPLES: usize = 6;

/// Slope below this magnitude is reported as "flat" — regression noise dominates.
pub const FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR: f32 = 0.5;

/// Ceiling on projected "empty in" — anything longer collapses to this to avoid
/// nonsense like "empty in ~470h."  Chosen so the menu row stays under one screen line.
pub const MAX_EMPTY_IN_HOURS: i64 = 999;

#[derive(Clone, Debug)]
pub struct BurnRateEstimate {
    pub window: Window,
    pub current_pct: f32,
    /// Slope of the linear fit, in "percent points per hour."  May be zero.
    pub rate_pct_per_hour: f32,
    /// Projected time to hit 100%.  If `rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD`
    /// this is `now + MAX_EMPTY_IN_HOURS`; callers should render via
    /// `format_menu_row` which handles the flat case in text.
    pub empty_at: DateTime<Utc>,
    pub reset_at: Option<DateTime<Utc>>,
}

pub fn estimate(
    account: &AccountKey,
    window: Window,
    now: DateTime<Utc>,
) -> Option<BurnRateEstimate> {
    // Session window looks at the last 5h.  Weekly looks at 7d.  In both cases we
    // rely on the log-side filter to only return samples inside the CURRENT cycle
    // (i.e. after the most recent reset); if it doesn't, the linear fit will still
    // be reasonable but the confidence will be lower.
    let lookback_days: u32 = match window {
        Window::Session => 1, // 5h fits in 1 day
        Window::Weekly => 8,
    };
    let mut samples = usage_log::last_n_days(account, lookback_days);
    // Keep only the ones whose target-window pct is Some, ordered by ts asc.
    samples.retain(|s| pct_for(s, window).is_some());
    samples.sort_by_key(|s| s.ts);

    // Apply the per-window horizon filter.
    let horizon = match window {
        Window::Session => Duration::hours(5),
        Window::Weekly => Duration::days(7),
    };
    let cutoff = now - horizon;
    samples.retain(|s| s.ts >= cutoff);

    if samples.len() < MIN_SAMPLES {
        return None;
    }

    let current_pct = pct_for(samples.last().unwrap(), window).unwrap();
    let reset_at = reset_for(samples.last().unwrap(), window);

    let (slope_per_hour, intercept, _r2) = weighted_linear_fit(&samples, window, now)?;

    // Empty-at projection: solve intercept + slope*t = 100 (t is hours from `now`).
    let empty_at = if slope_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR {
        // Flat: cap far in the future.
        now + Duration::hours(MAX_EMPTY_IN_HOURS)
    } else {
        // hours_from_now = (100 - current_pct) / slope
        let hours_to_empty = ((100.0 - current_pct) / slope_per_hour).max(0.0) as i64;
        now + Duration::hours(hours_to_empty.min(MAX_EMPTY_IN_HOURS))
    };

    // Silence unused-var lint on intercept — kept for debug/logging clarity.
    let _ = intercept;

    Some(BurnRateEstimate {
        window,
        current_pct,
        rate_pct_per_hour: slope_per_hour,
        empty_at,
        reset_at,
    })
}

/// Render a short menu row: "Weekly · 61% · ~40m" when the window
/// runs out before it resets, "Weekly · 61% · lasts to reset" when it
/// doesn't (or the pace is flat), "Weekly · 61% · pace flat" with no reset.
/// The reset time itself is on the "renews in" row, so it isn't repeated.
pub fn format_menu_row(est: &BurnRateEstimate) -> String {
    let name = match est.window {
        Window::Session => "Session",
        Window::Weekly => "Weekly",
    };
    let flat = est.rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR;
    let tail = match est.reset_at {
        Some(reset) if flat || est.empty_at >= reset => "lasts to reset".to_string(),
        _ if flat => "pace flat".to_string(),
        _ => format!("~{}", format_short_duration(est.empty_at - Utc::now())),
    };
    format!("{name} · {:.0}% · {tail}", est.current_pct)
}

fn pct_for(s: &Snapshot, w: Window) -> Option<f32> {
    match w {
        Window::Session => s.session_pct,
        Window::Weekly => s.weekly_pct,
    }
}

fn reset_for(s: &Snapshot, w: Window) -> Option<DateTime<Utc>> {
    // Snapshot doesn't carry reset time today; the caller (renderer) supplies it from
    // the account's cached usage.  We keep the field on BurnRateEstimate so the
    // renderer can populate it after `estimate`.  Return None here so the module is
    // testable standalone.
    let _ = (s, w);
    None
}

/// Weighted linear fit of pct vs hours-from-now.  Weights: exponential decay, most
/// recent samples weighted 1.0, oldest ~0.35 (half-life = window_horizon / 2).
///
/// Returns (slope_pct_per_hour, intercept_pct_at_now, r_squared).
fn weighted_linear_fit(
    samples: &[Snapshot],
    window: Window,
    now: DateTime<Utc>,
) -> Option<(f32, f32, f32)> {
    if samples.len() < MIN_SAMPLES {
        return None;
    }
    let horizon_hours = match window {
        Window::Session => 5.0f32,
        Window::Weekly => 24.0 * 7.0,
    };
    let half_life = horizon_hours * 0.5;
    let ln2 = std::f32::consts::LN_2;

    // x_i = hours before now (positive going into the past).
    let xs: Vec<f32> = samples
        .iter()
        .map(|s| {
            let d = now - s.ts;
            (d.num_seconds() as f32) / 3600.0
        })
        .collect();
    let ys: Vec<f32> = samples
        .iter()
        .map(|s| pct_for(s, window).unwrap())
        .collect();
    let ws: Vec<f32> = xs.iter().map(|x| (-x * ln2 / half_life).exp()).collect();

    let sum_w: f32 = ws.iter().sum();
    if sum_w <= f32::EPSILON {
        return None;
    }
    let mean_x = xs.iter().zip(&ws).map(|(x, w)| x * w).sum::<f32>() / sum_w;
    let mean_y = ys.iter().zip(&ws).map(|(y, w)| y * w).sum::<f32>() / sum_w;

    let mut cov_xy = 0.0f32;
    let mut var_x = 0.0f32;
    let mut var_y = 0.0f32;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov_xy += ws[i] * dx * dy;
        var_x += ws[i] * dx * dx;
        var_y += ws[i] * dy * dy;
    }
    if var_x < f32::EPSILON {
        return None;
    }
    // Slope in y-per-x, i.e. pct per hour PAST-facing.  Flip sign for future-facing.
    let slope_past = cov_xy / var_x;
    let slope_future = -slope_past;
    // Intercept "at now" (x=0).
    let intercept = mean_y - slope_past * mean_x;
    let r2 = if var_y < f32::EPSILON {
        0.0
    } else {
        (cov_xy * cov_xy) / (var_x * var_y)
    };
    Some((slope_future, intercept, r2))
}

/// "1d 23h" / "23h 52m" / "51m" / "<1m" — mirrors src/countdown.rs formatting rules.
///
/// Sub-hour buckets round to the nearest minute so a fixture that computes
/// `now + Duration::minutes(40)` still prints "40m" when the format helper is
/// called a few milliseconds later. Hour/day buckets keep floor semantics so
/// "23h 59m" doesn't jump to "1d 0h" one second before the boundary.
fn format_short_duration(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return if secs > 0 {
            "<1m".to_string()
        } else {
            "0m".to_string()
        };
    }
    let mins_total = secs / 60;
    let hours_total = mins_total / 60;
    let days = hours_total / 24;
    if days >= 1 {
        let hours = hours_total % 24;
        return format!("{days}d {hours}h");
    }
    if hours_total >= 1 {
        let mins = mins_total % 60;
        return format!("{hours_total}h {mins}m");
    }
    // Sub-hour: round to nearest minute so a value one second shy of the next
    // minute doesn't display as the previous minute. Cap at 59 so we never
    // overflow into "60m" (a caller with 3599s + rounding lands here).
    let mins_rounded = ((secs + 30) / 60).min(59);
    format!("{mins_rounded}m")
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::trait_def::Window;
    use crate::usage_log::{AccountKey, Snapshot};
    use chrono::{Datelike, TimeZone};

    fn snap(mins_ago: i64, weekly: f32) -> Snapshot {
        Snapshot {
            ts: Utc::now() - Duration::minutes(mins_ago),
            provider: "claude".into(),
            account: "matt@example.com".into(),
            session_pct: None,
            weekly_pct: Some(weekly),
            active_model: None,
            ..Default::default()
        }
    }

    // Linear ascent 0% -> 60% over 6 hours = 10 pct/hr.  Expect empty_at ~= now + 4h.
    #[test]
    fn linear_ascent_projects_correctly() {
        // Build a synthetic sample series: one sample every 30 min for 6h, 0..60% linear.
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..=12)
            .map(|i| {
                let mins_ago = (12 - i) * 30;
                let pct = i as f32 * 5.0;
                Snapshot {
                    ts: now - Duration::minutes(mins_ago),
                    provider: "claude".into(),
                    account: "matt@example.com".into(),
                    session_pct: None,
                    weekly_pct: Some(pct),
                    active_model: None,
                    ..Default::default()
                }
            })
            .collect();
        let (slope, _intercept, r2) = weighted_linear_fit(&samples, Window::Weekly, now).unwrap();
        assert!((slope - 10.0).abs() < 1.5, "slope ~10 pct/hr, got {slope}");
        assert!(
            r2 > 0.95,
            "R² should be near 1 for a linear ascent, got {r2}"
        );
    }

    #[test]
    fn flat_rate_is_reported_flat() {
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..8).map(|i| snap(i * 15, 30.0)).collect();
        let (slope, _, _) = weighted_linear_fit(&samples, Window::Weekly, now).unwrap();
        assert!(slope.abs() <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR + 0.1);
    }

    #[test]
    fn fewer_than_min_samples_returns_none() {
        let now = Utc::now();
        let samples: Vec<Snapshot> = (0..(MIN_SAMPLES - 1))
            .map(|i| snap(i as i64 * 10, i as f32 * 3.0))
            .collect();
        assert!(weighted_linear_fit(&samples, Window::Weekly, now).is_none());
    }

    fn est(rate: f32, empty_in: Duration, reset_in: Option<Duration>) -> BurnRateEstimate {
        let now = Utc::now();
        BurnRateEstimate {
            window: Window::Weekly,
            current_pct: 61.0,
            rate_pct_per_hour: rate,
            empty_at: now + empty_in,
            reset_at: reset_in.map(|r| now + r),
        }
    }

    #[test]
    fn format_menu_row_flat() {
        let e = est(0.1, Duration::hours(500), None);
        assert_eq!(format_menu_row(&e), "Weekly · 61% · pace flat");
        let e = est(0.1, Duration::hours(500), Some(Duration::hours(10)));
        assert_eq!(format_menu_row(&e), "Weekly · 61% · lasts to reset");
    }

    /// Regression: empty in ~59m with the reset 6d away used to read
    /// "empty in ~59m · safe for this cycle (6d 17h to spare)".
    #[test]
    fn format_menu_row_empties_before_reset() {
        let e = est(
            35.0,
            Duration::minutes(59) + Duration::seconds(30),
            Some(Duration::days(6)),
        );
        assert_eq!(format_menu_row(&e), "Weekly · 61% · ~59m");
        // Mid-minute, so a slow runner can't tick it over a boundary.
        let e = est(15.0, Duration::minutes(119) + Duration::seconds(50), None);
        assert_eq!(format_menu_row(&e), "Weekly · 61% · ~1h 59m");
    }

    #[test]
    fn format_menu_row_lasts_past_reset() {
        let e = est(1.0, Duration::hours(40), Some(Duration::hours(10)));
        assert_eq!(format_menu_row(&e), "Weekly · 61% · lasts to reset");
        // Exact tie: it never sits empty, so it lasts.
        let now = Utc::now();
        let tie = BurnRateEstimate {
            empty_at: now + Duration::hours(3),
            reset_at: Some(now + Duration::hours(3)),
            ..est(20.0, Duration::zero(), None)
        };
        assert_eq!(format_menu_row(&tie), "Weekly · 61% · lasts to reset");
    }

    #[test]
    fn format_short_duration_ladder() {
        assert_eq!(format_short_duration(Duration::seconds(0)), "0m");
        assert_eq!(format_short_duration(Duration::seconds(30)), "<1m");
        assert_eq!(format_short_duration(Duration::minutes(1)), "1m");
        assert_eq!(format_short_duration(Duration::minutes(59)), "59m");
        assert_eq!(format_short_duration(Duration::minutes(60)), "1h 0m");
        assert_eq!(format_short_duration(Duration::minutes(60 + 23)), "1h 23m");
        assert_eq!(
            format_short_duration(Duration::hours(23) + Duration::minutes(59)),
            "23h 59m"
        );
        assert_eq!(format_short_duration(Duration::hours(24)), "1d 0h");
        assert_eq!(
            format_short_duration(Duration::days(1) + Duration::hours(23)),
            "1d 23h"
        );
    }

    // ---- weighted_linear_fit: exact values on non-linear data ----
    // Expected numbers come from an independent f64 reference implementation of
    // the exponentially weighted regression (half-life = horizon / 2).

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap()
    }

    fn series(now: DateTime<Utc>, ages_mins: &[i64], session: bool, ys: &[f32]) -> Vec<Snapshot> {
        ages_mins
            .iter()
            .zip(ys)
            .map(|(a, y)| Snapshot {
                ts: now - Duration::minutes(*a),
                provider: "claude".into(),
                account: "matt@example.com".into(),
                session_pct: session.then_some(*y),
                weekly_pct: (!session).then_some(*y),
                active_model: None,
                ..Default::default()
            })
            .collect()
    }

    fn assert_close(got: f32, want: f32, what: &str) {
        assert!((got - want).abs() < 0.02, "{what}: got {got}, want {want}");
    }

    #[test]
    fn fit_session_matches_reference_values() {
        let now = fixed_now();
        let samples = series(
            now,
            &[290, 240, 180, 130, 90, 50, 20, 0],
            true,
            &[5.0, 9.0, 20.0, 24.0, 41.0, 45.0, 60.0, 62.0],
        );
        let (slope, intercept, r2) = weighted_linear_fit(&samples, Window::Session, now).unwrap();
        assert_close(slope, 13.1212, "slope");
        assert_close(intercept, 60.2986, "intercept");
        assert_close(r2, 0.95282, "r2");
    }

    #[test]
    fn fit_weekly_matches_reference_values() {
        let now = fixed_now();
        let samples = series(
            now,
            &[9600, 7800, 6000, 4200, 2400, 1200, 300, 0],
            false,
            &[2.0, 10.0, 12.0, 30.0, 31.0, 50.0, 52.0, 70.0],
        );
        let (slope, intercept, r2) = weighted_linear_fit(&samples, Window::Weekly, now).unwrap();
        assert_close(slope, 0.41082, "slope");
        assert_close(intercept, 58.6851, "intercept");
        assert_close(r2, 0.87525, "r2");
    }

    #[test]
    fn fit_exactly_min_samples_is_accepted() {
        let now = fixed_now();
        let samples = series(
            now,
            &[50, 40, 30, 20, 10, 0],
            true,
            &[1.0, 3.0, 2.0, 6.0, 5.0, 9.0],
        );
        assert_eq!(samples.len(), MIN_SAMPLES);
        assert!(weighted_linear_fit(&samples, Window::Session, now).is_some());
    }

    #[test]
    fn fit_identical_timestamps_has_no_slope() {
        let now = fixed_now();
        let samples = series(
            now,
            &[10, 10, 10, 10, 10, 10, 10],
            true,
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
        );
        assert!(weighted_linear_fit(&samples, Window::Session, now).is_none());
    }

    #[test]
    fn fit_constant_pct_has_zero_confidence() {
        let now = fixed_now();
        let samples = series(
            now,
            &[100, 80, 60, 40, 20, 0],
            true,
            &[30.0, 30.0, 30.0, 30.0, 30.0, 30.0],
        );
        let (slope, _, r2) = weighted_linear_fit(&samples, Window::Session, now).unwrap();
        assert_eq!(r2, 0.0);
        assert!(slope.abs() < 1e-4, "{slope}");
    }

    // ---- estimate: end-to-end through a scoped history log ----

    static LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Writes `(minutes_ago, pct)` rows for a unique account into a scoped
    /// config dir's monthly history files and returns the guard + key + now.
    fn with_history(
        session: bool,
        rows: &[(i64, f32)],
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        crate::store::ScopedConfigDir,
        AccountKey,
        DateTime<Utc>,
    ) {
        let lock = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = crate::store::ScopedConfigDir::new();
        let dir = crate::store::config_dir().unwrap();
        let now = Utc::now();
        for (ago, pct) in rows {
            let ts = now - Duration::minutes(*ago);
            let snap = Snapshot {
                ts,
                provider: "claude".into(),
                account: "burn@example.com".into(),
                session_pct: session.then_some(*pct),
                weekly_pct: (!session).then_some(*pct),
                active_model: None,
                ..Default::default()
            };
            let path = dir.join(format!("history.{:04}-{:02}.ndjson", ts.year(), ts.month()));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            use std::io::Write;
            writeln!(f, "{}", serde_json::to_string(&snap).unwrap()).unwrap();
        }
        crate::usage_log::invalidate_cache();
        (
            lock,
            guard,
            AccountKey::new("claude", "burn@example.com"),
            now,
        )
    }

    /// 10 pct/hr climb ending at 35% (one minute ago): 65 points left -> 6.5h,
    /// truncated to 6h.
    fn climbing_rows() -> Vec<(i64, f32)> {
        (0..7)
            .map(|k| (1 + k * 30, 35.0 - 5.0 * k as f32))
            .collect()
    }

    #[test]
    fn estimate_projects_empty_time_for_climbing_session() {
        let (_l, _g, key, now) = with_history(true, &climbing_rows());
        let est = estimate(&key, Window::Session, now).expect("enough samples");
        assert_eq!(est.current_pct, 35.0);
        assert!(
            (est.rate_pct_per_hour - 10.0).abs() < 0.3,
            "{}",
            est.rate_pct_per_hour
        );
        assert_eq!(est.empty_at - now, Duration::hours(6));
        assert!(est.reset_at.is_none());
    }

    #[test]
    fn estimate_ignores_samples_outside_session_horizon() {
        let mut rows = climbing_rows();
        // Older than the 5h session horizon: must not influence the fit.
        rows.extend([(330, 90.0), (360, 95.0), (420, 99.0)]);
        let (_l, _g, key, now) = with_history(true, &rows);
        let est = estimate(&key, Window::Session, now).expect("enough in-horizon samples");
        assert!(
            (est.rate_pct_per_hour - 10.0).abs() < 0.3,
            "{}",
            est.rate_pct_per_hour
        );
        assert_eq!(est.empty_at - now, Duration::hours(6));
    }

    #[test]
    fn estimate_needs_min_samples_exactly() {
        let rows = climbing_rows();
        let (_l, _g, key, now) = with_history(true, &rows[..MIN_SAMPLES]);
        assert!(estimate(&key, Window::Session, now).is_some());
        drop((_l, _g));
        let (_l, _g, key, now) = with_history(true, &rows[..MIN_SAMPLES - 1]);
        assert!(estimate(&key, Window::Session, now).is_none());
    }

    #[test]
    fn estimate_falling_usage_is_flat_and_capped() {
        let rows: Vec<(i64, f32)> = (0..7)
            .map(|k| (1 + k * 30, 20.0 + 5.0 * k as f32))
            .collect();
        let (_l, _g, key, now) = with_history(true, &rows);
        let est = estimate(&key, Window::Session, now).expect("enough samples");
        assert!(est.rate_pct_per_hour <= FLAT_SLOPE_THRESHOLD_PCT_PER_HOUR);
        assert_eq!(est.empty_at - now, Duration::hours(MAX_EMPTY_IN_HOURS));
    }

    // Ignored by default — this hits the live usage_log crate function, which panics
    // in unit-test mode without a fixture harness.  See tests/integration when wiring.
    #[test]
    #[ignore]
    fn estimate_via_public_api() {
        let key = AccountKey::default();
        let _ = estimate(&key, Window::Weekly, Utc::now());
    }
}
