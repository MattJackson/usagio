//! The v2 shell: builds a [`Snapshot`] from state, runs `decide`, performs
//! `NeedReading` fetches within the request budget (§5.7), re-decides, and
//! executes the switch (memory, latches, notifications, logs).
//!
//! The read loop ([`decide_with_reads`]) and the [`Budget`] are shared with
//! the simulator, so the same rules are exercised by the gate scenarios.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::memory::SwapMemory;
use super::policy::*;
use super::{
    assess_all, decide, AccountIn, Action, Decision, Kind, ManualPick, Policy, Reason, Snapshot,
    Tier, Urgency, WindowIn,
};
use crate::store::State;
use crate::{logging, Row};

// ---------------------------------------------------------------------------
// Request budget (§5.7)
// ---------------------------------------------------------------------------

/// Why a read was deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defer {
    PerMinute,
    PerCycle,
    PerAccount,
    PerHour,
    StormGuard,
}

impl Defer {
    pub fn as_str(self) -> &'static str {
        match self {
            Defer::PerMinute => "per_minute",
            Defer::PerCycle => "per_cycle",
            Defer::PerAccount => "per_account",
            Defer::PerHour => "per_hour",
            Defer::StormGuard => "storm_guard",
        }
    }
}

/// One provider's usage-request budget.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Active + verification request times (unix secs).
    requests: Vec<i64>,
    /// Verification read times, last hour.
    verify_reads: Vec<i64>,
    last_verify: HashMap<String, i64>,
    last_429: Option<i64>,
    /// Reads deferred so far (for the log).
    pub deferred: usize,
}

impl Budget {
    fn trim(&mut self, now: i64) {
        // `0..` drops entries "from the future" after a backward clock jump.
        self.requests.retain(|t| (0..60).contains(&(now - *t)));
        self.verify_reads
            .retain(|t| (0..3600).contains(&(now - *t)));
    }

    /// An active-account usage request was made at `now` (by the poller).
    pub fn note_request(&mut self, now: i64) {
        self.trim(now);
        self.requests.push(now);
    }

    /// A 429 on any of the provider's tokens at `now` (storm guard).
    pub fn note_429(&mut self, now: i64) {
        self.last_429 = Some(self.last_429.map_or(now, |t| t.max(now)));
    }

    /// Requests counted in the last 60 s.
    pub fn requests_last_minute(&self, now: i64) -> usize {
        self.requests
            .iter()
            .filter(|t| (0..60).contains(&(now - **t)))
            .count()
    }

    /// Verification reads in the last hour.
    pub fn verify_reads_last_hour(&self, now: i64) -> usize {
        self.verify_reads
            .iter()
            .filter(|t| (0..3600).contains(&(now - **t)))
            .count()
    }

    /// May a read of `key` be made now?
    pub fn allow(
        &self,
        key: &str,
        now: i64,
        is_active: bool,
        reset_pending: bool,
        verify_this_cycle: usize,
    ) -> std::result::Result<(), Defer> {
        if self.requests_last_minute(now) >= PROVIDER_REQ_PER_MIN {
            return Err(Defer::PerMinute);
        }
        if is_active {
            return Ok(());
        }
        if verify_this_cycle >= MAX_VERIFY_FETCHES {
            return Err(Defer::PerCycle);
        }
        if self
            .last_verify
            .get(&key.to_ascii_lowercase())
            .is_some_and(|t| (0..VERIFY_MIN_INTERVAL_S).contains(&(now - *t)))
        {
            return Err(Defer::PerAccount);
        }
        if self.verify_reads_last_hour(now) >= VERIFY_PER_HOUR {
            return Err(Defer::PerHour);
        }
        if !reset_pending
            && self
                .last_429
                .is_some_and(|t| (0..STORM_GUARD_S).contains(&(now - t)))
        {
            return Err(Defer::StormGuard);
        }
        Ok(())
    }

    /// Count a read of `key` made at `now`.
    pub fn record_read(&mut self, key: &str, now: i64, is_active: bool) {
        self.trim(now);
        self.requests.push(now);
        if !is_active {
            self.verify_reads.push(now);
            self.last_verify.insert(key.to_ascii_lowercase(), now);
        }
    }
}

// ---------------------------------------------------------------------------
// The read loop
// ---------------------------------------------------------------------------

/// What a read returned.
#[derive(Debug, Clone)]
pub enum ReadResult {
    /// A fresh reading: the account's new windows / fetched_at / credits.
    Ok(Box<AccountIn>),
    RateLimited,
    Failed,
}

/// One read the loop made or deferred.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadLog {
    pub key: String,
    pub is_active: bool,
    /// `"ok"`, `"429"`, `"err"` or `"defer:<why>"`.
    pub outcome: String,
}

fn has_pending(a: &AccountIn, now: DateTime<Utc>) -> bool {
    [a.session.as_ref(), a.weekly.as_ref()]
        .into_iter()
        .flatten()
        .any(|w| w.resets_at.is_some_and(|r| r <= now))
}

/// Decide, performing `NeedReading` reads within `budget` and re-deciding,
/// until the decision is a `Stay` or a `Switch`. Every account a decision
/// asked for is marked `attempted` (a deferred read counts as failed), so the
/// loop terminates. `read(key, is_active)` performs one fetch.
pub fn decide_with_reads(
    snap: &mut Snapshot,
    budget: &mut Budget,
    read: &mut dyn FnMut(&str, bool) -> ReadResult,
) -> (Decision, Vec<ReadLog>) {
    let mut log = Vec::new();
    let mut verify_this_cycle = 0usize;
    let now = snap.now;
    let now_ts = now.timestamp();
    let mut last = decide(snap);
    for _ in 0..6 {
        if last.action != Action::NeedReading {
            return (last, log);
        }
        let mut progressed = false;
        for key in last.need_reading.clone() {
            let Some(idx) = snap
                .accounts
                .iter()
                .position(|a| a.key.eq_ignore_ascii_case(&key))
            else {
                continue;
            };
            if snap.accounts[idx].attempted {
                continue;
            }
            progressed = true;
            let is_active = key.eq_ignore_ascii_case(&snap.active);
            let pending = has_pending(&snap.accounts[idx], now);
            let acc = &mut snap.accounts[idx];
            acc.attempted = true;
            match budget.allow(&key, now_ts, is_active, pending, verify_this_cycle) {
                Err(why) => {
                    budget.deferred += 1;
                    acc.unverified = true;
                    if is_active {
                        acc.in_backoff = true;
                    }
                    log.push(ReadLog {
                        key: key.clone(),
                        is_active,
                        outcome: format!("defer:{}", why.as_str()),
                    });
                }
                Ok(()) => {
                    budget.record_read(&key, now_ts, is_active);
                    if !is_active {
                        verify_this_cycle += 1;
                    }
                    let outcome = match read(&key, is_active) {
                        ReadResult::Ok(fresh) => {
                            acc.session = fresh.session;
                            acc.weekly = fresh.weekly;
                            acc.fetched_at = fresh.fetched_at;
                            acc.credits = fresh.credits;
                            acc.unverified = false;
                            "ok"
                        }
                        ReadResult::RateLimited => {
                            budget.note_429(now_ts);
                            acc.unverified = true;
                            if is_active {
                                acc.in_backoff = true;
                            }
                            "429"
                        }
                        ReadResult::Failed => {
                            acc.unverified = true;
                            if is_active {
                                acc.in_backoff = true;
                            }
                            "err"
                        }
                    };
                    log.push(ReadLog {
                        key: key.clone(),
                        is_active,
                        outcome: outcome.to_string(),
                    });
                }
            }
        }
        last = decide(snap);
        if !progressed {
            break;
        }
    }
    if last.action == Action::NeedReading {
        // Nothing more can be read this cycle: stay, and say why.
        last.action = Action::Stay;
    }
    (last, log)
}

// ---------------------------------------------------------------------------
// Decision log dedupe (§3.6)
// ---------------------------------------------------------------------------

/// Remembers the last logged (action, reason, target) so a decision is logged
/// once per change (plus every switch).
#[derive(Debug, Clone, Default)]
pub struct DecisionLog {
    last: Option<(String, String, Option<String>)>,
}

impl DecisionLog {
    /// Whether `d` should be logged, recording it if so.
    pub fn should_log(&mut self, d: &Decision) -> bool {
        let key = (
            d.action_str().to_string(),
            d.reason.as_str().to_string(),
            d.target().map(str::to_string),
        );
        if matches!(d.action, Action::Switch { .. }) {
            self.last = Some(key);
            return true;
        }
        if self.last.as_ref() == Some(&key) {
            return false;
        }
        self.last = Some(key);
        true
    }
}

/// Per-provider in-memory shell state, carried in `SwapGuard` across cycles.
#[derive(Debug, Clone, Default)]
pub struct ShellState {
    pub budget: Budget,
    pub log: DecisionLog,
}

// ---------------------------------------------------------------------------
// Engine selection and process-wide facts
// ---------------------------------------------------------------------------

/// The env var that selects the classic engine (`v1`) for `usagio watch` and
/// the CLI. Secondary to the Settings toggle.
pub const ENGINE_ENV_VAR: &str = "USAGIO_AUTOSWAP_ENGINE";

/// Whether the v2 engine is selected: `State.autoswap_engine` (Settings ▸
/// Auto-swap ▸ Engine) wins; unset, `USAGIO_AUTOSWAP_ENGINE=v1` selects the
/// classic engine; otherwise v2 (the default).
pub fn engine_v2(state: &State) -> bool {
    match state.autoswap_engine.as_deref() {
        Some(e) => !is_classic(e),
        None => std::env::var(ENGINE_ENV_VAR)
            .ok()
            .is_none_or(|v| !is_classic(v.trim())),
    }
}

fn is_classic(e: &str) -> bool {
    e.eq_ignore_ascii_case("v1") || e.eq_ignore_ascii_case("classic")
}

static SLEEP_GAPS: std::sync::Mutex<Vec<(i64, i64)>> = std::sync::Mutex::new(Vec::new());

/// Record a detected system sleep `(from, to)` (unix secs); the blind
/// projection excludes it. Keeps the last day.
pub fn note_sleep(from: i64, to: i64) {
    let mut g = SLEEP_GAPS.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|&(_, b)| to - b < 86_400);
    g.push((from, to));
}

fn sleep_gaps() -> Vec<(i64, i64)> {
    SLEEP_GAPS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ---------------------------------------------------------------------------
// Building the snapshot
// ---------------------------------------------------------------------------

fn window_in(pct: Option<f64>, resets_at: Option<DateTime<Utc>>) -> Option<WindowIn> {
    pct.map(|pct| WindowIn { pct, resets_at })
}

/// The engine's view of one row.
pub(crate) fn account_in_from_row(r: &Row, rate: super::AccountRate) -> AccountIn {
    AccountIn {
        key: r.email.clone(),
        session: window_in(r.session.pct, r.session.resets_at),
        weekly: window_in(r.weekly.pct, r.weekly.resets_at),
        fetched_at: if r.has_data() { r.fetched_at } else { None },
        rate,
        credits: r.spendable_credits(),
        lapsed: r.no_subscription,
        unverified: false,
        attempted: false,
        in_backoff: false,
    }
}

/// The manual pick of `slug`, as the engine sees it.
pub(crate) fn manual_pick_of(state: &State, slug: &str) -> Option<ManualPick> {
    let key = state.manual_pick(slug)?.to_string();
    let settled = state
        .manual_pick_tier
        .get(slug)
        .and_then(|t| Tier::parse(t))
        .unwrap_or(if state.manual_hold(slug).is_some() {
            Tier::Draining
        } else {
            Tier::Ready
        });
    Some(ManualPick {
        at: state.manual_pick_at.get(slug).copied().unwrap_or(0),
        settled_by_fresh: !state.manual_pick_at.contains_key(slug),
        key,
        settled,
    })
}

/// `build_snapshot` (§8.1): the switchable-filtered rows of `slug` plus the
/// active one. Rows without a reading are included (as Unknown) so Prepare
/// can refuse to act on incomplete data (B12).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_snapshot(
    state: &State,
    slug: &str,
    rows: &[Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    now: DateTime<Utc>,
    rate_of: &dyn Fn(&str) -> super::AccountRate,
) -> Snapshot {
    let env_override = crate::env_override_active(slug);
    let switchable = crate::provider_supports_swap(slug) && !env_override;
    let accounts: Vec<AccountIn> = rows
        .iter()
        .filter(|r| {
            r.email.eq_ignore_ascii_case(active)
                || (switchable && r.provider_id == slug && !r.needs_relogin && !r.no_subscription)
        })
        .map(|r| {
            let mut a = account_in_from_row(r, rate_of(&r.email));
            if r.email.eq_ignore_ascii_case(active) {
                a.in_backoff = crate::active_fetch_in_backoff(slug, &r.email, now.timestamp());
            }
            a
        })
        .collect();
    let mut policy = Policy::new(trigger, ceiling);
    policy.use_credits = state.use_credits_when_exhausted;
    policy.env_override = env_override;
    Snapshot {
        now,
        provider: slug.to_string(),
        active: active.to_string(),
        accounts,
        manual: manual_pick_of(state, slug),
        memory: state.swap_memory.get(slug).cloned().unwrap_or_default(),
        policy,
        sleep: sleep_gaps(),
    }
}

// ---------------------------------------------------------------------------
// The real cycle
// ---------------------------------------------------------------------------

/// What one v2 cycle did, for `watch_cycle` and the cadence.
#[derive(Debug, Clone)]
pub(crate) struct CycleResult {
    pub swapped: Option<(String, String)>,
    pub actionable: bool,
    pub urgency: Urgency,
    pub next_check_at: Option<DateTime<Utc>>,
}

/// Apply `f` to `slug`'s swap memory under the state lock (reload, mutate,
/// save) — never from a pre-fetch copy (§3.5).
pub(crate) fn update_memory(slug: &str, f: impl FnOnce(&mut SwapMemory)) -> Result<SwapMemory> {
    crate::with_state_lock(|| {
        let mut st = State::load()?;
        let m = st.swap_memory.entry(slug.to_string()).or_default();
        let before = m.clone();
        f(m);
        let after = m.clone();
        if after != before {
            st.save()?;
        }
        Ok(after)
    })
}

/// Start-of-cycle bookkeeping under the lock: External detection (the active
/// key changed with no Auto/Manual note → `left_at`, cooldown, pick cleared)
/// and pruning.
pub(crate) fn begin_cycle(slug: &str, now: DateTime<Utc>) -> Result<State> {
    crate::with_state_lock(|| {
        let mut st = State::load()?;
        let active = crate::provider_active(&st, slug);
        let m = st.swap_memory.entry(slug.to_string()).or_default();
        let before = m.clone();
        let left = m.detect_external(active.as_deref(), now.timestamp());
        m.prune(now);
        let changed = *m != before;
        if let Some(left) = &left {
            logging::log(&format!(
                "event=active_change kind=external provider={slug} from={left} to={}",
                active.as_deref().unwrap_or("-")
            ));
            st.set_manual_pick(slug, None);
            st.set_manual_hold(slug, None);
            st.manual_pick_tier.remove(slug);
        }
        if changed || left.is_some() {
            st.save()?;
        }
        Ok(st)
    })
}

fn row_pct(r: Option<&Row>) -> f64 {
    r.map(Row::max_pct).unwrap_or(0.0)
}

fn history_reason(kind: Kind) -> &'static str {
    match kind {
        Kind::Rebalance => "proactive",
        k => k.as_str(),
    }
}

/// Write one decision line to `decisions.<YYYY-MM>.ndjson` (§6).
fn log_decision_ndjson(
    slug: &str,
    d: &Decision,
    reads: &[ReadLog],
    budget: &Budget,
    now: DateTime<Utc>,
) {
    let v = serde_json::json!({
        "ts": now.timestamp(),
        "provider": slug,
        "engine": "v2",
        "action": d.action_str(),
        "reason": d.reason.as_str(),
        "target": d.target(),
        "active_tier": d.active_tier.as_str(),
        "urgency": d.urgency.as_str(),
        "est": d.diag.est,
        "lb": d.diag.lb,
        "proj": d.diag.proj,
        "runway": if d.diag.soft_runway.is_finite() { Some(d.diag.soft_runway) } else { None },
        "rates": { "session": d.diag.rate_session, "weekly": d.diag.rate_weekly },
        "need_reading": reads.iter().map(|r| serde_json::json!({"key": r.key, "outcome": r.outcome})).collect::<Vec<_>>(),
        "budget": {
            "req_last_min": budget.requests_last_minute(now.timestamp()),
            "verify_last_hour": budget.verify_reads_last_hour(now.timestamp()),
            "deferred": budget.deferred,
        },
    });
    if let Err(e) = crate::usage_log::append_decision(&v) {
        logging::log(&format!("decisions log append failed: {e:#}"));
    }
}

/// One v2 cycle for `slug` (§8.1 `run_cycle`): decide → NeedReading via
/// `verify_swap_target` (budgeted) → re-decide → `switch_if_still_active` →
/// memory, notify, log.
pub(crate) fn run_cycle(
    slug: &str,
    rows: &mut [Row],
    active: &str,
    trigger: f64,
    ceiling: f64,
    shell: &mut ShellState,
) -> Result<CycleResult> {
    let now = Utc::now();
    let state = begin_cycle(slug, now)?;
    let mut snap = build_snapshot(&state, slug, rows, active, trigger, ceiling, now, &|k| {
        super::rates::rates_for(slug, k, now)
    });
    let (d, reads) = {
        let rows_ref = &mut *rows;
        let mut read = |key: &str, _is_active: bool| -> ReadResult {
            match crate::verify_swap_target_detailed(slug, key) {
                Ok(fresh) => {
                    let a = account_in_from_row(&fresh, Default::default());
                    if let Some(r) = rows_ref
                        .iter_mut()
                        .find(|r| r.email.eq_ignore_ascii_case(key))
                    {
                        *r = fresh;
                    }
                    ReadResult::Ok(Box::new(a))
                }
                Err(true) => ReadResult::RateLimited,
                Err(false) => ReadResult::Failed,
            }
        };
        decide_with_reads(&mut snap, &mut shell.budget, &mut read)
    };
    for r in reads.iter().filter(|r| r.outcome.starts_with("defer:")) {
        logging::log(&format!(
            "event=budget_defer provider={slug} account={} why={}",
            r.key,
            r.outcome.trim_start_matches("defer:")
        ));
    }
    let assessed = assess_all(&snap);
    let act_assessed = assessed.iter().find(|a| a.is_active).cloned();
    let act_row_pct = row_pct(rows.iter().find(|r| r.email.eq_ignore_ascii_case(active)));

    let mut swapped = None;
    let mut switched_ok = false;
    if let Action::Switch { to, kind } = &d.action {
        let target_row = rows.iter().find(|r| r.email.eq_ignore_ascii_case(to));
        let (pick_s, pick_w) = target_row
            .map(|r| (r.session.pct.unwrap_or(0.0), r.weekly.pct.unwrap_or(0.0)))
            .unwrap_or((0.0, 0.0));
        let target_pct = row_pct(target_row);
        if *kind != Kind::Rebalance {
            logging::log(&format!(
                "event=swap_decision provider={slug} active={active} active_pct={act_row_pct:.0}% \
                 target={to} target_pct={target_pct:.0}% action=switching reason={} v=2",
                kind.as_str()
            ));
        }
        let latch = leave_latch(*kind, act_assessed.as_ref(), now);
        match crate::switch_if_still_active_latched(slug, to, active, latch) {
            Ok(Some(label)) => {
                switched_ok = true;
                let kind = *kind;
                crate::log_event(&serde_json::json!({
                    "ts": now.timestamp(),
                    "event": "swap",
                    "provider": slug,
                    "engine": "v2",
                    "reason": history_reason(kind),
                    "from": active,
                    "to": to,
                    "session": pick_s,
                    "weekly": pick_w,
                }));
                match kind {
                    Kind::Prepare => crate::notify(&format!(
                        "Prepared {label} for its next reset — usage is still blocked."
                    )),
                    Kind::Rebalance => crate::notify(&format!(
                        "Flipped back to {label} — {pick_s:.0}% / {pick_w:.0}%"
                    )),
                    _ => crate::notify(&format!(
                        "Switched to {label} — {pick_s:.0}% / {pick_w:.0}%"
                    )),
                }
                swapped = Some((active.to_string(), to.clone()));
            }
            Ok(None) => logging::log(&format!(
                "event=swap_skipped provider={slug} target={to} reason=active_changed v=2"
            )),
            Err(e) => logging::log(&format!("swap to {slug}/{to} failed: {e:#}")),
        }
    } else if shell.log.should_log(&d) {
        let target = d
            .blocked_target
            .as_deref()
            .map(|t| format!(" target={t}"))
            .unwrap_or_default();
        logging::log(&format!(
            "event=swap_decision provider={slug} active={active} active_pct={act_row_pct:.0}%{target} \
             action={} reason={} tier={} v=2",
            d.action_str(),
            d.reason.as_str(),
            d.active_tier.as_str()
        ));
        log_decision_ndjson(&snap.provider, &d, &reads, &shell.budget, now);
    }
    if switched_ok {
        shell.log.should_log(&d);
        log_decision_ndjson(slug, &d, &reads, &shell.budget, now);
    }

    // Stuck notice + the Draining hysteresis memory, in one locked write.
    if !switched_ok {
        let soonest = rows
            .iter()
            .filter(|r| r.has_data())
            .filter_map(|r| r.weekly.resets_at)
            .min()
            .map(crate::humanize_until)
            .unwrap_or_else(|| "unknown".to_string());
        let mut fire = false;
        let act_key = active.to_string();
        let res = update_memory(slug, |m| {
            fire = stuck_update(m, &d, &act_key);
        });
        if let Err(e) = res {
            logging::log(&format!("swap memory save failed: {e:#}"));
        } else if fire {
            crate::notify(&format!(
                "staying on {active} — no better target available (soonest reset in {soonest})"
            ));
        }
    }

    // Dual-write the v1 hold while v1 exists (rollback, §3.5).
    dual_write_hold(slug);

    let actionable = d.actionable
        || matches!(d.action, Action::Switch { .. })
        || (d.reason == Reason::Cooldown && d.blocked_target.is_some());
    Ok(CycleResult {
        swapped,
        actionable,
        urgency: d.urgency,
        next_check_at: d.next_check_at,
    })
}

/// Keep `manual_holds[slug]` ⇔ (pick settled tier ≥ Draining), so a rollback
/// to the classic engine holds the same picks.
pub(crate) fn dual_write_hold(slug: &str) {
    let res = crate::with_state_lock(|| {
        let mut st = State::load()?;
        let Some(key) = st.manual_pick(slug).map(str::to_string) else {
            return Ok(());
        };
        let held = st
            .manual_pick_tier
            .get(slug)
            .and_then(|t| Tier::parse(t))
            .is_some_and(|t| t >= Tier::Draining);
        let cur = st.manual_hold(slug).is_some();
        if held != cur {
            st.set_manual_hold(slug, held.then_some(key.as_str()));
            st.save()?;
        }
        Ok(())
    });
    if let Err(e) = res {
        logging::log(&format!("manual hold dual-write failed: {e:#}"));
    }
}

/// The stuck notice and the Draining hysteresis memory after a non-switch
/// decision (§3.6). The notice fires once on entering a stuck stay; it
/// re-arms only on a switch (`note_active_change`), when the active account
/// is Ready or held, or on a tier change backed by a Real reading — not on a
/// 429 streak making the active account Unknown for a while. Returns whether
/// to send the notice now.
pub fn stuck_update(m: &mut SwapMemory, d: &Decision, active: &str) -> bool {
    let tier = d.active_tier;
    if d.active_real {
        if let Some(prev) = m.real_tier.as_deref().and_then(Tier::parse) {
            if prev != tier {
                m.stuck_notified = false;
            }
        }
        m.real_tier = Some(tier.as_str().to_string());
    }
    if (tier == Tier::Ready && d.active_real) || d.reason == Reason::ManualHold {
        m.stuck_notified = false;
    }
    let fire = d.stuck && !m.stuck_notified;
    if d.stuck {
        m.stuck_notified = true;
    }
    match tier {
        Tier::Draining => m.draining = Some(active.to_string()),
        Tier::Ready | Tier::Blocked | Tier::Credits => m.draining = None,
        Tier::Unknown => {}
    }
    fire
}

/// The latch for leaving the active account by this switch, handed to the
/// switch so it is written in the same locked section as the CAS (§3.5).
pub(crate) fn leave_latch(
    kind: Kind,
    act: Option<&super::assess::Assessed>,
    now: DateTime<Utc>,
) -> Option<(super::memory::LatchKind, super::memory::Latch)> {
    let l = act?.latch(now)?;
    match kind {
        Kind::Trigger => Some((super::memory::LatchKind::Trigger, l)),
        Kind::Drain | Kind::Serve => Some((super::memory::LatchKind::Drained, l)),
        _ => None,
    }
}

thread_local! {
    /// The leave latch for the switch in progress on this thread (consumed
    /// inside the switch's locked section by `State::note_active_change`).
    static PENDING_LATCH: std::cell::RefCell<Option<(String, String, super::memory::LatchKind, super::memory::Latch)>> =
        const { std::cell::RefCell::new(None) };
}

/// Arm the leave latch for `slug`'s switch away from `from` (cleared by the
/// returned guard when the switch returns).
pub(crate) fn arm_leave_latch(
    slug: &str,
    from: &str,
    latch: Option<(super::memory::LatchKind, super::memory::Latch)>,
) -> impl Drop {
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            PENDING_LATCH.with(|p| *p.borrow_mut() = None);
        }
    }
    PENDING_LATCH.with(|p| {
        *p.borrow_mut() = latch.map(|(k, l)| (slug.to_string(), from.to_string(), k, l));
    });
    Disarm
}

/// Take the armed leave latch if it is for `slug` leaving `from`.
pub(crate) fn take_leave_latch(
    slug: &str,
    from: &str,
) -> Option<(super::memory::LatchKind, super::memory::Latch)> {
    PENDING_LATCH.with(|p| {
        let mut p = p.borrow_mut();
        let hit = p
            .as_ref()
            .is_some_and(|(s, f, _, _)| s == slug && f.eq_ignore_ascii_case(from));
        if hit {
            p.take().map(|(_, _, k, l)| (k, l))
        } else {
            None
        }
    })
}
