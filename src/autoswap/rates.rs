//! Per-account burn rates (§3.2): one model for decisions, cadence and the
//! menu's "empty in" rows.
//!
//! Rate = Δpct/Δt over the account's own active readings in the last 30 min.
//! Zero deltas are included (flat data gives 0). Intervals with a drop in pct
//! (a reset) or longer than 15 min (sleep, downtime) are skipped. The peak is
//! the highest rate over 5-minute spans in the last 60 min.

use super::policy::{
    DEFAULT_RATE_SESSION, DEFAULT_RATE_WEEKLY, PEAK_SPAN_MIN, PEAK_WINDOW_MIN, RATE_GAP_MAX_MIN,
    RATE_WINDOW_MIN,
};
use super::WindowId;
use chrono::{DateTime, Utc};

/// An account's measured rates, pct/min. `None` = not enough samples.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AccountRate {
    pub session: Option<f64>,
    pub weekly: Option<f64>,
    pub peak_session: Option<f64>,
    pub peak_weekly: Option<f64>,
}

/// The default (and floor) rate of a window.
pub fn default_rate(w: WindowId) -> f64 {
    match w {
        WindowId::Session => DEFAULT_RATE_SESSION,
        WindowId::Weekly => DEFAULT_RATE_WEEKLY,
    }
}

impl AccountRate {
    /// The measured rate, if any (the display uses this: "at current pace").
    pub fn own(&self, w: WindowId) -> Option<f64> {
        match w {
            WindowId::Session => self.session,
            WindowId::Weekly => self.weekly,
        }
    }

    /// The measured rate, or the default when there is no data yet.
    pub fn effective(&self, w: WindowId) -> f64 {
        self.own(w).unwrap_or_else(|| default_rate(w)).max(0.0)
    }

    /// `max(rate, DEFAULT)`: candidate runway, drain gain and I2 use this.
    pub fn floored(&self, w: WindowId) -> f64 {
        self.effective(w).max(default_rate(w))
    }

    /// The 5-minute peak in the last hour (0 when unknown).
    pub fn peak(&self, w: WindowId) -> f64 {
        match w {
            WindowId::Session => self.peak_session,
            WindowId::Weekly => self.peak_weekly,
        }
        .unwrap_or(0.0)
        .max(0.0)
    }
}

/// One reading: unix seconds and the two window percentages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub t: i64,
    pub session: Option<f64>,
    pub weekly: Option<f64>,
}

fn pick(p: &Point, w: WindowId) -> Option<f64> {
    match w {
        WindowId::Session => p.session,
        WindowId::Weekly => p.weekly,
    }
}

/// Rate over `[now − 30 min, now]`: Σ valid Δpct / Σ valid Δt.
fn window_rate(points: &[Point], w: WindowId, now_ts: i64) -> Option<f64> {
    let from = now_ts - RATE_WINDOW_MIN * 60;
    let pts: Vec<(i64, f64)> = points
        .iter()
        .filter(|p| p.t >= from && p.t <= now_ts)
        .filter_map(|p| pick(p, w).map(|v| (p.t, v)))
        .collect();
    let (mut dp, mut dt) = (0.0, 0.0);
    for pair in pts.windows(2) {
        let ((t0, v0), (t1, v1)) = (pair[0], pair[1]);
        let gap = t1 - t0;
        if gap <= 0 || gap > RATE_GAP_MAX_MIN * 60 || v1 < v0 {
            continue;
        }
        dp += v1 - v0;
        dt += gap as f64 / 60.0;
    }
    (dt > 0.0).then(|| (dp / dt).max(0.0))
}

/// Highest rate over spans of at least 5 minutes within the last 60 min,
/// skipping spans that contain a drop or a gap.
fn window_peak(points: &[Point], w: WindowId, now_ts: i64) -> Option<f64> {
    let from = now_ts - PEAK_WINDOW_MIN * 60;
    let pts: Vec<(i64, f64)> = points
        .iter()
        .filter(|p| p.t >= from && p.t <= now_ts)
        .filter_map(|p| pick(p, w).map(|v| (p.t, v)))
        .collect();
    let mut best: Option<f64> = None;
    for i in 1..pts.len() {
        // Walk back to the latest sample at least PEAK_SPAN_MIN earlier.
        let mut j = i;
        let mut ok = true;
        while j > 0 {
            let (ta, va) = pts[j - 1];
            let (tb, vb) = pts[j];
            if tb - ta > RATE_GAP_MAX_MIN * 60 || vb < va {
                ok = false;
                break;
            }
            j -= 1;
            if pts[i].0 - pts[j].0 >= PEAK_SPAN_MIN * 60 {
                break;
            }
        }
        if !ok || j == i {
            continue;
        }
        let span = (pts[i].0 - pts[j].0) as f64 / 60.0;
        if span < PEAK_SPAN_MIN as f64 {
            continue;
        }
        let r = (pts[i].1 - pts[j].1) / span;
        best = Some(best.map_or(r, |b: f64| b.max(r)));
    }
    best
}

/// Rates from readings (any order, duplicates allowed).
pub fn rates_from_points(points: &[Point], now: DateTime<Utc>) -> AccountRate {
    let now_ts = now.timestamp();
    let mut pts: Vec<Point> = points.to_vec();
    pts.sort_by_key(|p| p.t);
    pts.dedup_by_key(|p| p.t);
    AccountRate {
        session: window_rate(&pts, WindowId::Session, now_ts),
        weekly: window_rate(&pts, WindowId::Weekly, now_ts),
        peak_session: window_peak(&pts, WindowId::Session, now_ts),
        peak_weekly: window_peak(&pts, WindowId::Weekly, now_ts),
    }
}

/// Rates for `key` from usage-log rows: only successful readings (rows with
/// no outcome are pre-0.12.2 readings), only while the account was active (or
/// not marked), timed by `fetched_at` when the row has it.
pub fn rates_from_samples(
    samples: &[crate::usage_log::Snapshot],
    key: &str,
    now: DateTime<Utc>,
) -> AccountRate {
    let points: Vec<Point> = samples
        .iter()
        .filter(|s| s.account.eq_ignore_ascii_case(key))
        .filter(|s| s.is_reading() && s.active != Some(false))
        .map(|s| Point {
            t: s.fetched_at.unwrap_or_else(|| s.ts.timestamp()),
            session: s.session_pct.map(f64::from),
            weekly: s.weekly_pct.map(f64::from),
        })
        .collect();
    rates_from_points(&points, now)
}

/// Rates for one account from the on-disk usage log (last day of rows).
pub fn rates_for(provider: &str, key: &str, now: DateTime<Utc>) -> AccountRate {
    let rows = crate::usage_log::last_n_days(&crate::usage_log::AccountKey::new(provider, key), 1);
    rates_from_samples(&rows, key, now)
}
