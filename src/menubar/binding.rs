//! The binding-window indicator (which window will run out first) and its hysteresis.

use super::model::{summary_pcts, weekly_only, AcctView, ProviderSection};
use super::render::{pct, u16len};
use chrono::{DateTime, Utc};

/// The engine's view of an account row (display only).
pub(super) fn account_in_for(a: &AcctView) -> crate::autoswap::AccountIn {
    use crate::autoswap::{AccountIn, WindowIn};
    let pct_of = |id: &str| a.windows.iter().find(|w| w.id == id).and_then(|w| w.pct);
    AccountIn {
        session: pct_of("session").map(|pct| WindowIn {
            pct,
            resets_at: a.session_reset_at,
        }),
        weekly: pct_of("weekly").map(|pct| WindowIn {
            pct,
            resets_at: a.weekly_reset_at,
        }),
        fetched_at: a.fetched_at.map(|t| t.timestamp()),
        rate: a.rate,
        ..AccountIn::new(&a.key)
    }
}

/// The window that blocks first (§4), at the account's own rate.
pub(super) fn binding_for(
    a: &AcctView,
    now: DateTime<Utc>,
) -> Option<crate::autoswap::runway::Binding> {
    if !a.has_data || a.no_subscription {
        return None;
    }
    crate::autoswap::runway::binding_window_at(&account_in_for(a), now, a.trigger)
}

/// Displayed binding window per (provider, account), with hysteresis. In
/// memory only (not persisted, R2-M2).
pub(super) static BINDING_SHOWN: std::sync::Mutex<
    Option<std::collections::HashMap<String, crate::autoswap::runway::BindingShown>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// Set by the tests' `RealBindingHysteresis` guard.
    pub(super) static REAL_BINDING_HYSTERESIS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether [`shown_binding`] skips its hysteresis state: never in the app; in
/// tests unless the thread opted into the real path.
#[cfg(test)]
pub(super) fn binding_hysteresis_bypassed() -> bool {
    !REAL_BINDING_HYSTERESIS.with(std::cell::Cell::get)
}

#[cfg(not(test))]
pub(super) const fn binding_hysteresis_bypassed() -> bool {
    false
}

/// The window to show for `a` now, folding the hysteresis state.
pub(super) fn shown_binding(
    sec: &ProviderSection,
    a: &AcctView,
    now: DateTime<Utc>,
    trigger: f64,
) -> Option<crate::autoswap::WindowId> {
    if !a.has_data || a.no_subscription {
        return None;
    }
    // Tests render titles for many fixtures sharing keys in one process, so
    // by default they see the fresh binding window; a test that holds
    // `RealBindingHysteresis` exercises the real path below.
    if binding_hysteresis_bypassed() {
        let _ = trigger;
        return binding_for(a, now).map(|b| b.window);
    }
    let key = format!("{}/{}", sec.provider_id, a.key);
    let mut g = BINDING_SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(Default::default);
    let next = crate::autoswap::runway::BindingShown::update(
        map.get(&key).copied(),
        &account_in_for(a),
        now,
        trigger,
    );
    match next {
        Some(n) => {
            map.insert(key, n);
            Some(n.window)
        }
        None => {
            map.remove(&key);
            None
        }
    }
}

/// Tray title for the displayed binding window: session `47%`, weekly
/// `96%w`. With no window to show, the session (else weekly) pct.
pub(super) fn binding_title(a: &AcctView, shown: Option<crate::autoswap::WindowId>) -> String {
    let pct_of = |id: &str| a.windows.iter().find(|w| w.id == id).and_then(|w| w.pct);
    match shown {
        Some(crate::autoswap::WindowId::Weekly) => match pct_of("weekly") {
            Some(p) => format!("{p:.0}%w"),
            None => pct(pct_of("session")),
        },
        Some(crate::autoswap::WindowId::Session) => pct(pct_of("session").or(pct_of("weekly"))),
        None => {
            let (session, weekly) = summary_pcts(a);
            pct(session.or(weekly))
        }
    }
}

/// The span (offset, len) of the binding window's segment inside the
/// trailing `sa / sb` text, for the active row.
pub(super) fn binding_span(
    a: &AcctView,
    trailing: &str,
    now: DateTime<Utc>,
) -> Option<(usize, usize)> {
    let b = binding_for(a, now)?;
    let (sa, sb) = trailing.trim_end_matches(" 🎁").split_once(" / ")?;
    let first_is_session = a.windows.first().is_some_and(|w| w.id == "session");
    let in_first = (b.window == crate::autoswap::WindowId::Session) == first_is_session;
    if in_first {
        Some((0, u16len(sa)))
    } else {
        Some((u16len(sa) + u16len(" / "), u16len(sb)))
    }
}

/// `~Nm` / `~Nh Mm` for a minute count.
pub(super) fn short_minutes(m: f64) -> String {
    if !m.is_finite() {
        return "—".to_string();
    }
    let m = m.max(0.0).round() as i64;
    if m >= 60 {
        format!("~{}h {}m", m / 60, m % 60)
    } else {
        format!("~{m}m")
    }
}

/// Tooltip body: `email — session X, weekly Y%w ← binds, blocks in ~Nm at
/// current pace`, plus the engine's floored-rate verdict when it differs.
pub(super) fn binding_tooltip(a: &AcctView, now: DateTime<Utc>, trigger: f64) -> String {
    use crate::autoswap::runway::time_to;
    let s = a.windows.first().and_then(|w| w.pct);
    let w = a.windows.get(1).and_then(|w| w.pct);
    let ids: Vec<&str> = a.windows.iter().map(|w| w.id.as_str()).collect();
    let b = binding_for(a, now);
    let mark = |id: &str| -> String {
        match b {
            Some(b) if b.window.as_str() == id => match b.time_to_block {
                Some(m) => format!(" ← binds, blocks in {} at current pace", short_minutes(m)),
                None => " ← binds".to_string(),
            },
            _ => String::new(),
        }
    };
    let weekly_txt = |p: Option<f64>| match p {
        Some(v) => format!("{v:.0}%w"),
        None => pct(None),
    };
    let first = ids.first().copied().unwrap_or("session");
    let second = ids.get(1).copied().unwrap_or("weekly");
    let render = |id: &str, p: Option<f64>| {
        if id == "weekly" {
            weekly_txt(p)
        } else {
            pct(p)
        }
    };
    let mut out = if weekly_only(a) {
        format!(
            "{} — weekly {}{}",
            a.display,
            render(second, w),
            mark(second)
        )
    } else {
        format!(
            "{} — session {}{}, weekly {}{}",
            a.display,
            render(first, s),
            mark(first),
            render(second, w),
            mark(second)
        )
    };
    // The engine judges with the floored rate; say so when it is stricter
    // than the account's own pace (R2-m3).
    if let Some(b) = b {
        let engine = time_to(b.pct, trigger, a.rate.floored(b.window));
        let own = b.time_to_block.unwrap_or(f64::INFINITY);
        if engine < crate::autoswap::policy::MIN_LANDING_MIN
            && own >= 2.0 * crate::autoswap::policy::MIN_LANDING_MIN
        {
            out.push_str(" · auto-swap treats it as near limit");
        }
    }
    out
}

/// Rates for the binding display, recomputed at most every 30 s per account
/// (the snapshot is rebuilt every UI tick).
pub(super) fn cached_rates(
    slug: &str,
    key: &str,
    now: DateTime<Utc>,
) -> crate::autoswap::AccountRate {
    type RateCache = std::collections::HashMap<String, (i64, crate::autoswap::AccountRate)>;
    static CACHE: std::sync::Mutex<Option<RateCache>> = std::sync::Mutex::new(None);
    let k = format!("{slug}/{key}");
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(Default::default);
    if let Some((at, r)) = map.get(&k) {
        if (now.timestamp() - at).abs() < 30 {
            return *r;
        }
    }
    let r = crate::autoswap::rates::rates_for(slug, key, now);
    map.insert(k, (now.timestamp(), r));
    r
}

#[cfg(test)]
#[path = "binding_tests.rs"]
mod tests;
