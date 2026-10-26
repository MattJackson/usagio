//! Fleet simulator (design §8.2) and the gate scenarios S1–S16, S8b, S4′/S5′,
//! plus the randomized invariant test (I1, I2, I4, no-NaN).
//!
//! Only the active account accrues usage (plus the post-leave lag on the
//! account just left). A reset drops a window to 0 at `resets_at`. Readings
//! are integers sampled at poll time, like the API. The poller mirrors the
//! v2 shell: the active fetch policy (§5), regular inactive polls (600 s
//! floor), the request budget and `decide_with_reads`, with the realism
//! knobs on by default.

use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::{Rng, SeedableRng};

use super::assess::Assessed;
use super::memory::{ChangeKind, SwapMemory};
use super::rates::{rates_from_points, Point};
use super::shell::{decide_with_reads, Budget, ReadResult};
use super::*;

const SESSION_PERIOD_H: i64 = 5;
const WEEKLY_PERIOD_D: i64 = 7;
const STEP_S: i64 = 5;
const INACTIVE_FLOOR_S: i64 = 600;

/// One simulated account: its true usage and what usagio has cached.
#[derive(Clone, Debug)]
pub struct SimAccount {
    pub key: String,
    pub s: f64,
    pub s_reset: DateTime<Utc>,
    pub has_session: bool,
    pub w: f64,
    pub w_reset: DateTime<Utc>,
    pub credits: Option<i64>,
    /// When usagio last read it (the initial cache is a reading at this
    /// time of the start values); `None` = never read.
    pub read_at: Option<DateTime<Utc>>,
}

impl SimAccount {
    pub fn new(key: &str, s: f64, w: f64, s_reset: DateTime<Utc>, w_reset: DateTime<Utc>) -> Self {
        SimAccount {
            key: key.to_string(),
            s,
            s_reset,
            has_session: true,
            w,
            w_reset,
            credits: None,
            read_at: None,
        }
    }

    pub fn read(mut self, at: DateTime<Utc>) -> Self {
        self.read_at = Some(at);
        self
    }
}

#[derive(Clone, Debug)]
struct Cached {
    s: Option<f64>,
    s_reset: DateTime<Utc>,
    w: f64,
    w_reset: DateTime<Utc>,
    fetched_at: i64,
    credits: Option<i64>,
}

type Mask = Box<dyn Fn(&str, DateTime<Utc>) -> bool>;
/// A correlated-429 knob: probability per cycle, and when it applies.
type Correlated = (f64, Box<dyn Fn(DateTime<Utc>) -> bool>);

/// A scenario (§8.2).
pub struct Scenario {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub accounts: Vec<SimAccount>,
    pub active: String,
    /// `(from, session_rate, weekly_rate)` pct/min, sorted by `from`.
    pub workload: Vec<(DateTime<Utc>, f64, f64)>,
    pub trigger: f64,
    /// Per-account 429 mask for usage reads.
    pub mask_429: Mask,
    /// Correlated 429s: one draw per cycle for every token, while `in(t)`.
    pub correlated: Option<Correlated>,
    /// `NeedReading` reads that fail (non-429).
    pub verify_fail: Mask,
    pub manual: Vec<(DateTime<Utc>, String)>,
    pub external: Vec<(DateTime<Utc>, String)>,
    pub sleep: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub post_leave_lag: bool,
    pub use_credits: bool,
    pub seed: u64,
}

impl Scenario {
    pub fn new(
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        active: &str,
        accounts: Vec<SimAccount>,
    ) -> Self {
        Scenario {
            start,
            end,
            accounts,
            active: active.to_string(),
            workload: vec![(start, 0.0, 0.0)],
            trigger: 95.0,
            mask_429: Box::new(|_, _| false),
            correlated: None,
            verify_fail: Box::new(|_, _| false),
            manual: Vec::new(),
            external: Vec::new(),
            sleep: Vec::new(),
            post_leave_lag: true,
            use_credits: false,
            seed: 7,
        }
    }
}

/// One switch.
#[derive(Clone, Debug)]
pub struct SimSwitch {
    pub t: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub kind: Kind,
    /// The target's reading time when the switch was decided.
    pub target_fetched_at: Option<i64>,
    /// When we last left the target before this switch, if ever.
    pub target_left_at: Option<i64>,
    /// The target's assessment at decision time (latches, runway).
    pub target: Assessed,
    /// The account left's assessment at decision time.
    pub source: Assessed,
    /// Some other account was servable when the switch was decided.
    pub servable_existed: bool,
}

/// What a run did (§8.2).
#[derive(Debug, Default)]
pub struct Trace {
    pub switches: Vec<SimSwitch>,
    /// Seconds on a truly blocked active account while another account
    /// truly had room.
    pub blocked_secs: i64,
    pub notices: usize,
    /// `(t, key, outcome, kind)` for every usage request (kind: active,
    /// verify, poll).
    pub fetches: Vec<(DateTime<Utc>, String, String, &'static str)>,
    /// A sliding-window budget overrun (§5.7), if any.
    pub budget_overrun: Option<String>,
    /// `(t, decision)` per cycle.
    pub decisions: Vec<(DateTime<Utc>, Decision)>,
    /// Invariant violations seen by the per-cycle checks.
    pub violations: Vec<String>,
    /// The first time the active account was read at or over the trigger,
    /// per account.
    pub first_real_at_trigger: Vec<(String, DateTime<Utc>)>,
}

impl Trace {
    pub fn blocked_minutes(&self) -> f64 {
        self.blocked_secs as f64 / 60.0
    }
}

struct World {
    sc: Scenario,
    accts: Vec<SimAccount>,
    cache: Vec<Option<Cached>>,
    history: Vec<Vec<Point>>,
    active: String,
    memory: SwapMemory,
    manual: Option<ManualPick>,
    budget: Budget,
    cur: u64,
    last_active_attempt: Option<(i64, bool)>,
    left_lag: Option<(String, DateTime<Utc>)>,
    first_after_wake: bool,
    rng: rand::rngs::StdRng,
    trace: Trace,
}

fn floor_int(p: f64) -> f64 {
    p.clamp(0.0, 100.0).floor()
}

impl World {
    fn idx(&self, key: &str) -> usize {
        self.accts
            .iter()
            .position(|a| a.key.eq_ignore_ascii_case(key))
            .expect("account in scenario")
    }

    fn rates_at(&self, t: DateTime<Utc>) -> (f64, f64) {
        let mut r = (0.0, 0.0);
        for &(from, s, w) in &self.sc.workload {
            if from <= t {
                r = (s, w);
            }
        }
        r
    }

    fn asleep(&self, t: DateTime<Utc>) -> bool {
        self.sc.sleep.iter().any(|&(a, b)| t >= a && t < b)
    }

    /// Advance true usage by one step ending at `t`.
    fn accrue(&mut self, t: DateTime<Utc>) {
        let dt_min = STEP_S as f64 / 60.0;
        let (rs, rw) = if self.asleep(t) {
            (0.0, 0.0)
        } else {
            self.rates_at(t)
        };
        let mut burners = vec![self.active.clone()];
        if let Some((k, until)) = &self.left_lag {
            if t <= *until {
                burners.push(k.clone());
            }
        }
        for a in self.accts.iter_mut() {
            if t >= a.s_reset {
                a.s = 0.0;
                while a.s_reset <= t {
                    a.s_reset += Duration::hours(SESSION_PERIOD_H);
                }
            }
            if t >= a.w_reset {
                a.w = 0.0;
                while a.w_reset <= t {
                    a.w_reset += Duration::days(WEEKLY_PERIOD_D);
                }
            }
            if burners.iter().any(|b| b.eq_ignore_ascii_case(&a.key)) {
                let blocked = a.w >= 100.0 || (a.has_session && a.s >= 100.0);
                if !blocked || a.credits.is_some() {
                    if a.has_session {
                        a.s = (a.s + rs * dt_min).min(100.0);
                    }
                    a.w = (a.w + rw * dt_min).min(100.0);
                }
            }
        }
        // Blocked time: the active account truly blocked while another has room.
        let act = &self.accts[self.idx(&self.active)];
        let act_blocked =
            (act.w >= 100.0 || (act.has_session && act.s >= 100.0)) && act.credits.is_none();
        let other_room = self.accts.iter().any(|a| {
            !a.key.eq_ignore_ascii_case(&self.active)
                && a.w < 100.0
                && !(a.has_session && a.s >= 100.0)
        });
        if act_blocked && other_room && !self.asleep(t) {
            self.trace.blocked_secs += STEP_S;
        }
    }

    fn reading(&self, i: usize, t: DateTime<Utc>) -> Cached {
        let a = &self.accts[i];
        Cached {
            s: a.has_session.then(|| floor_int(a.s)),
            s_reset: a.s_reset,
            w: floor_int(a.w),
            w_reset: a.w_reset,
            fetched_at: t.timestamp(),
            credits: a.credits,
        }
    }

    fn correlated_429(&mut self, t: DateTime<Utc>) -> bool {
        match &self.sc.correlated {
            Some((p, active)) if active(t) => {
                let p = *p;
                self.rng.gen::<f64>() < p
            }
            _ => false,
        }
    }

    fn account_in(&self, i: usize, now: DateTime<Utc>) -> AccountIn {
        let a = &self.accts[i];
        let mut out = AccountIn::new(&a.key);
        if let Some(c) = &self.cache[i] {
            out.session = c.s.map(|pct| WindowIn {
                pct,
                resets_at: Some(c.s_reset),
            });
            out.weekly = Some(WindowIn {
                pct: c.w,
                resets_at: Some(c.w_reset),
            });
            out.fetched_at = Some(c.fetched_at);
            out.credits = c.credits.and_then(|m| Money::new(m, 2, "USD"));
        }
        out.rate = rates_from_points(&self.history[i], now);
        out
    }

    fn store(&mut self, i: usize, c: Cached, active: bool) {
        if active {
            self.history[i].push(Point {
                t: c.fetched_at,
                session: c.s,
                weekly: Some(c.w),
            });
        }
        self.cache[i] = Some(c);
    }

    fn note_switch(&mut self, from: &str, to: &str, now: DateTime<Utc>, kind: ChangeKind) {
        if self.sc.post_leave_lag {
            self.left_lag = Some((from.to_string(), now + Duration::seconds(60)));
        }
        self.memory
            .note_active_change(Some(from), to, now.timestamp(), kind);
        self.active = to.to_string();
        self.last_active_attempt = None;
    }

    fn cycle(&mut self, now: DateTime<Utc>, just_switched: bool) -> (Decision, bool, bool) {
        let now_ts = now.timestamp();
        let trigger = self.sc.trigger;
        // External detection.
        if let Some(left) = self.memory.detect_external(Some(&self.active), now_ts) {
            let _ = left;
            self.manual = None;
        }
        self.memory.prune(now);
        let storm = self.correlated_429(now);
        let ai = self.idx(&self.active.clone());

        // 1. The active fetch (§5): never while lb-blocked (until pending /
        //    12 h), the tier floor (60 s TIGHT), the provider's backoff.
        let mut rate_limited = false;
        let fetch_active = match &self.cache[ai] {
            None => true,
            Some(c) => {
                let pending = c.s_reset <= now || c.w_reset <= now;
                let lb_blocked = (c.w >= policy::BLOCKED_PCT && c.w_reset > now)
                    || c.s
                        .is_some_and(|s| s >= policy::BLOCKED_PCT && c.s_reset > now);
                let age = now_ts - c.fetched_at;
                if just_switched {
                    true
                } else if lb_blocked && age < policy::MAX_LOCKED_STALENESS_S {
                    false
                } else if pending {
                    true
                } else {
                    let max = c.s.unwrap_or(0.0).max(c.w);
                    let floor = crate::v2_active_floor_secs(Some(max), trigger, 180) as i64;
                    let backoff = self
                        .last_active_attempt
                        .is_some_and(|(t, failed)| failed && now_ts - t < self.cur as i64);
                    age >= floor && !backoff
                }
            }
        };
        // The active fetch counts toward (and respects) the per-minute cap.
        let fetch_active = fetch_active
            && self
                .budget
                .allow(&self.active.clone(), now_ts, true, false, 0)
                .is_ok();
        if fetch_active {
            self.budget.note_request(now_ts);
            let fail_wake = std::mem::take(&mut self.first_after_wake);
            let is429 = storm || (self.sc.mask_429)(&self.active, now);
            if is429 || fail_wake {
                rate_limited = is429;
                if is429 {
                    self.budget.note_429(now_ts);
                }
                self.last_active_attempt = Some((now_ts, true));
                self.trace.fetches.push((
                    now,
                    self.active.clone(),
                    if is429 { "429" } else { "err" }.into(),
                    "active",
                ));
            } else {
                let r = self.reading(ai, now);
                if r.w >= trigger || r.s.is_some_and(|s| s >= trigger) {
                    let k = self.active.clone();
                    if !self
                        .trace
                        .first_real_at_trigger
                        .iter()
                        .any(|(a, _)| *a == k)
                    {
                        self.trace.first_real_at_trigger.push((k, now));
                    }
                }
                self.store(ai, r, true);
                self.last_active_attempt = Some((now_ts, false));
                self.trace
                    .fetches
                    .push((now, self.active.clone(), "ok".into(), "active"));
                // Settle a manual pick on its first fresh reading.
                if let Some(p) = self.manual.as_mut() {
                    if !p.settled_by_fresh && now_ts <= p.at + policy::MANUAL_PICK_SETTLE_S {
                        let c = self.cache[ai].as_ref().unwrap();
                        p.settled = crate::tier_for_pct(c.s.unwrap_or(0.0).max(c.w), trigger);
                        p.settled_by_fresh = true;
                    } else if let Some(c) = self.cache[ai].as_ref() {
                        // The hold ends once the pick's tier improves (0.12.1).
                        let t = crate::tier_for_pct(c.s.unwrap_or(0.0).max(c.w), trigger);
                        if t < p.settled {
                            p.settled = t;
                        }
                        p.settled_by_fresh = true;
                    }
                }
            }
        }

        // 2. Regular inactive polls: 600 s floor, locked rows skipped until
        //    their reset passes, spared after an active 429.
        for i in 0..self.accts.len() {
            if i == ai || rate_limited {
                continue;
            }
            let due = match &self.cache[i] {
                None => true,
                Some(c) => {
                    let crossed = (c.s_reset <= now && c.fetched_at < c.s_reset.timestamp())
                        || (c.w_reset <= now && c.fetched_at < c.w_reset.timestamp());
                    let locked = (c.w >= policy::BLOCKED_PCT && c.w_reset > now)
                        || c.s
                            .is_some_and(|s| s >= policy::BLOCKED_PCT && c.s_reset > now);
                    crossed || (!locked && now_ts - c.fetched_at >= INACTIVE_FLOOR_S)
                }
            };
            if !due {
                continue;
            }
            let key = self.accts[i].key.clone();
            if storm || (self.sc.mask_429)(&key, now) {
                self.budget.note_429(now_ts);
                self.trace.fetches.push((now, key, "429".into(), "poll"));
                continue;
            }
            let r = self.reading(i, now);
            self.store(i, r, false);
            self.trace.fetches.push((now, key, "ok".into(), "poll"));
        }

        // 3. Decide, reading what the engine asks for within the budget.
        let accounts: Vec<AccountIn> = (0..self.accts.len())
            .map(|i| self.account_in(i, now))
            .collect();
        let mut snap = Snapshot {
            now,
            provider: "claude".into(),
            active: self.active.clone(),
            accounts,
            manual: self.manual.clone(),
            memory: self.memory.clone(),
            policy: {
                let mut p = Policy::new(trigger, trigger);
                p.use_credits = self.sc.use_credits;
                p
            },
            sleep: self
                .sc
                .sleep
                .iter()
                .map(|(a, b)| (a.timestamp(), b.timestamp()))
                .collect(),
        };
        if let Some(a) = snap.accounts.get_mut(ai) {
            a.in_backoff = self
                .last_active_attempt
                .is_some_and(|(t, failed)| failed && now_ts - t < self.cur as i64)
                || (fetch_active && self.last_active_attempt.is_some_and(|(_, f)| f));
        }
        let mut reads: Vec<(String, Cached, bool)> = Vec::new();
        let accts = self.accts.clone();
        let verify_fail = &self.sc.verify_fail;
        let mask = &self.sc.mask_429;
        let active_key = self.active.clone();
        let mut fetch_log = Vec::new();
        let (d, _log) = {
            let mut read = |key: &str, is_active: bool| -> ReadResult {
                let i = accts
                    .iter()
                    .position(|a| a.key.eq_ignore_ascii_case(key))
                    .unwrap();
                let kind = if is_active { "active" } else { "verify" };
                if storm || mask(key, now) {
                    fetch_log.push((now, key.to_string(), "429".to_string(), kind));
                    return ReadResult::RateLimited;
                }
                if verify_fail(key, now) {
                    fetch_log.push((now, key.to_string(), "err".to_string(), kind));
                    return ReadResult::Failed;
                }
                let a = &accts[i];
                let c = Cached {
                    s: a.has_session.then(|| floor_int(a.s)),
                    s_reset: a.s_reset,
                    w: floor_int(a.w),
                    w_reset: a.w_reset,
                    fetched_at: now.timestamp(),
                    credits: a.credits,
                };
                fetch_log.push((now, key.to_string(), "ok".to_string(), kind));
                let mut fresh = AccountIn::new(key);
                fresh.session = c.s.map(|pct| WindowIn {
                    pct,
                    resets_at: Some(c.s_reset),
                });
                fresh.weekly = Some(WindowIn {
                    pct: c.w,
                    resets_at: Some(c.w_reset),
                });
                fresh.fetched_at = Some(c.fetched_at);
                fresh.credits = c.credits.and_then(|m| Money::new(m, 2, "USD"));
                reads.push((key.to_string(), c, key.eq_ignore_ascii_case(&active_key)));
                ReadResult::Ok(Box::new(fresh))
            };
            decide_with_reads(&mut snap, &mut self.budget, &mut read)
        };
        for f in fetch_log {
            if f.2 == "429" {
                self.budget.note_429(now_ts);
            }
            self.trace.fetches.push(f);
        }
        for (key, c, is_active) in reads {
            let i = self.idx(&key);
            self.store(i, c, is_active);
        }
        self.check_budget(now);

        // 4. Execute.
        let assessed = assess_all(&snap);
        let mut swapped = false;
        if let Action::Switch { to, kind } = &d.action {
            let src = assessed
                .iter()
                .find(|a| a.key.eq_ignore_ascii_case(&self.active))
                .cloned()
                .unwrap();
            let tgt = assessed
                .iter()
                .find(|a| a.key.eq_ignore_ascii_case(to))
                .cloned()
                .unwrap();
            let from = self.active.clone();
            self.trace.switches.push(SimSwitch {
                t: now,
                from: from.clone(),
                to: to.clone(),
                kind: *kind,
                target_fetched_at: tgt.fetched_at,
                target_left_at: self.memory.left_at(to),
                target: tgt,
                source: src.clone(),
                servable_existed: assessed.iter().any(|a| !a.is_active && a.servable),
            });
            if let Some(l) = src.latch(now) {
                match kind {
                    Kind::Trigger => self.memory.latch_trigger(&from, l),
                    Kind::Drain | Kind::Serve => self.memory.latch_drained(&from, l),
                    _ => {}
                }
            }
            self.note_switch(&from, to, now, ChangeKind::Auto);
            self.manual = None;
            swapped = true;
        } else {
            let active = self.active.clone();
            if super::shell::stuck_update(&mut self.memory, &d, &active) {
                self.trace.notices += 1;
            }
        }
        if d.urgency == Urgency::Blocked
            && !matches!(d.action, Action::Switch { .. })
            && assessed.iter().any(|a| !a.is_active && a.servable)
        {
            self.trace.violations.push(format!(
                "{now}: I1 — stayed on a blocked active with a servable account"
            ));
        }
        self.trace.decisions.push((now, d.clone()));
        (d, swapped, rate_limited)
    }

    fn check_budget(&mut self, now: DateTime<Utc>) {
        let counted: Vec<&(DateTime<Utc>, String, String, &str)> = self
            .trace
            .fetches
            .iter()
            .filter(|f| f.3 != "poll" && (now - f.0).num_seconds() < 60)
            .collect();
        if counted.len() > policy::PROVIDER_REQ_PER_MIN && self.trace.budget_overrun.is_none() {
            self.trace.budget_overrun =
                Some(format!("{} requests in 60 s at {now}", counted.len()));
        }
        let hour = self
            .trace
            .fetches
            .iter()
            .filter(|f| f.3 == "verify" && (now - f.0).num_seconds() < 3600)
            .count();
        if hour > policy::VERIFY_PER_HOUR && self.trace.budget_overrun.is_none() {
            self.trace.budget_overrun =
                Some(format!("{hour} verification reads in an hour at {now}"));
        }
    }
}

/// Run a scenario.
pub fn run(sc: Scenario) -> Trace {
    let start = sc.start;
    let end = sc.end;
    let accts = sc.accounts.clone();
    let mut cache = Vec::new();
    for a in &accts {
        cache.push(a.read_at.map(|t| Cached {
            s: a.has_session.then(|| floor_int(a.s)),
            s_reset: a.s_reset,
            w: floor_int(a.w),
            w_reset: a.w_reset,
            fetched_at: t.timestamp(),
            credits: a.credits,
        }));
    }
    let n = accts.len();
    let mut w = World {
        rng: rand::rngs::StdRng::seed_from_u64(sc.seed),
        active: sc.active.clone(),
        accts,
        cache,
        history: vec![Vec::new(); n],
        memory: SwapMemory {
            last_seen_active: Some(sc.active.clone()),
            ..Default::default()
        },
        manual: None,
        budget: Budget::default(),
        cur: 180,
        last_active_attempt: None,
        left_lag: None,
        first_after_wake: false,
        sc,
        trace: Trace::default(),
    };
    let mut t = start;
    let mut next_poll = start;
    let mut just_switched = false;
    let mut was_asleep = false;
    let mut manual = std::mem::take(&mut w.sc.manual);
    let mut external = std::mem::take(&mut w.sc.external);
    manual.sort_by_key(|m| m.0);
    external.sort_by_key(|m| m.0);
    while t <= end {
        w.accrue(t);
        // Manual and external events.
        while manual.first().is_some_and(|m| m.0 <= t) {
            let (_, key) = manual.remove(0);
            let from = w.active.clone();
            let i = w.idx(&key);
            let settled = w.cache[i]
                .as_ref()
                .map(|c| crate::tier_for_pct(c.s.unwrap_or(0.0).max(c.w), w.sc.trigger))
                .unwrap_or(Tier::Ready);
            w.note_switch(&from, &key, t, ChangeKind::Manual);
            w.manual = Some(ManualPick {
                key: key.clone(),
                at: t.timestamp(),
                settled,
                settled_by_fresh: false,
            });
            just_switched = true;
            next_poll = t + Duration::seconds(5);
        }
        while external.first().is_some_and(|m| m.0 <= t) {
            let (_, key) = external.remove(0);
            w.active = key; // outside usagio's switch paths: no note
            w.last_active_attempt = None;
            next_poll = t + Duration::seconds(5);
        }
        let asleep = w.asleep(t);
        if was_asleep && !asleep {
            w.first_after_wake = true;
            next_poll = t;
        }
        was_asleep = asleep;
        if !asleep && t >= next_poll {
            // Same-account token adoption happens every cycle: a no-op.
            let before = (w.memory.clone(), w.manual.clone());
            w.memory.note_active_change(
                Some(&w.active.clone()),
                &w.active.clone(),
                t.timestamp(),
                ChangeKind::Auto,
            );
            let adopted_same = w.memory.left_at == before.0.left_at;
            if !adopted_same {
                w.trace
                    .violations
                    .push(format!("{t}: adoption changed left_at"));
            }
            // (note_active_change for the same key only refreshes
            // `last_seen_active`; restore the rest exactly.)
            w.memory = before.0;
            w.manual = before.1;

            let (d, swapped, rate_limited) = w.cycle(t, just_switched);
            just_switched = false;
            let max_pct = {
                let i = w.idx(&w.active.clone());
                w.cache[i].as_ref().map(|c| c.s.unwrap_or(0.0).max(c.w))
            };
            let sleep = if swapped {
                just_switched = true;
                w.cur = policy::ACTIVE_TIGHT_FLOOR_S;
                5
            } else {
                w.cur = crate::next_interval_v2(
                    w.cur,
                    180,
                    rate_limited,
                    d.urgency,
                    max_pct,
                    w.sc.trigger,
                    d.actionable,
                );
                let mut s = w.cur as i64;
                if let Some(at) = d.next_check_at {
                    s = s.min((at - t).num_seconds().max(15));
                }
                // Reset-boundary wake (+5 s settle).
                for a in &w.accts {
                    for r in [a.s_reset, a.w_reset] {
                        let secs = (r - t).num_seconds() + 5;
                        if secs > 0 {
                            s = s.min(secs.max(5));
                        }
                    }
                }
                s.max(5)
            };
            next_poll = t + Duration::seconds(sleep);
        }
        t += Duration::seconds(STEP_S);
    }
    w.trace
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn ymd(mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, mo, d, h, mi, s).unwrap()
}

fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    ymd(10, 25, h, m, s)
}

/// The fleet of 2026-10-25 at 16:20 (§7.1): dev4 active 4/89, dev8 0/80
/// (session just reset), dev6/dev7/others weekly-blocked overnight.
fn today_1620() -> Vec<SimAccount> {
    let dev4_w = ymd(10, 31, 12, 0, 0);
    vec![
        SimAccount::new("dev4", 4.0, 89.0, at(20, 0, 0), dev4_w).read(at(16, 20, 4)),
        SimAccount::new(
            "dev8",
            0.0,
            80.0,
            at(21, 19, 59),
            dev4_w + Duration::hours(5),
        )
        .read(at(16, 20, 5)),
        SimAccount::new("dev6", 10.0, 100.0, at(19, 0, 0), ymd(10, 26, 4, 59, 59))
            .read(at(15, 0, 0)),
        SimAccount::new("dev7", 10.0, 100.0, at(19, 0, 0), ymd(10, 26, 7, 59, 59))
            .read(at(15, 0, 0)),
        SimAccount::new("dev3", 10.0, 100.0, at(19, 0, 0), ymd(10, 27, 10, 0, 0))
            .read(at(15, 0, 0)),
    ]
}

/// The workload observed that afternoon: the 16:20 burst, ~0.71/0.18 until
/// 17:10, then ~1.8/0.47.
fn today_workload() -> Vec<(DateTime<Utc>, f64, f64)> {
    vec![
        (at(16, 20, 0), 2.67, 0.67),
        (at(16, 23, 0), 0.71, 0.18),
        (at(17, 11, 0), 1.8, 0.47),
    ]
}

/// 429s on alternate read attempts of `key` within `[from, to)` (stateful:
/// every other attempt fails, as observed 16:03–16:21).
fn alternate_429(key: &'static str, from: DateTime<Utc>, to: DateTime<Utc>) -> Mask {
    let n = std::cell::Cell::new(0u32);
    Box::new(move |k, t| {
        if k != key || t < from || t >= to {
            return false;
        }
        n.set(n.get() + 1);
        n.get() % 2 == 1
    })
}

fn switch_count(tr: &Trace) -> usize {
    tr.switches.len()
}

#[test]
fn s1_today_v2_path_from_1620() {
    let mut sc = Scenario::new(at(16, 20, 10), at(18, 0, 0), "dev4", today_1620());
    sc.workload = today_workload();
    // §7: 429s on alternate polls as observed 16:03–16:21.
    sc.mask_429 = alternate_429("dev4", at(16, 20, 0), at(16, 21, 30));
    let tr = run(sc);
    let path: Vec<String> = tr
        .switches
        .iter()
        .map(|s| {
            format!(
                "{} {}→{} {:?}",
                s.t.format("%H:%M:%S"),
                s.from,
                s.to,
                s.kind
            )
        })
        .collect();
    assert!(switch_count(&tr) <= 5, "{path:?}");
    // The §7.1 rule chain: Trigger → dev8, Drain → dev4, Serve → dev8
    // (the drained latch yields to I1), Prepare → dev6 (earliest reset).
    let chain: Vec<(&str, Kind)> = tr
        .switches
        .iter()
        .map(|s| (s.to.as_str(), s.kind))
        .collect();
    assert_eq!(
        chain,
        vec![
            ("dev8", Kind::Trigger),
            ("dev4", Kind::Drain),
            ("dev8", Kind::Serve),
            ("dev6", Kind::Prepare),
        ],
        "{path:?}"
    );
    assert!(
        tr.blocked_minutes() <= 3.0,
        "blocked {} min: {path:?}",
        tr.blocked_minutes()
    );
    // The first move is the Trigger off dev4 to dev8 around 16:45 (±2 min of
    // the expected trace, widened for the burst + 429 shape).
    let first = &tr.switches[0];
    assert_eq!(
        (first.from.as_str(), first.to.as_str()),
        ("dev4", "dev8"),
        "{path:?}"
    );
    assert_eq!(first.kind, Kind::Trigger, "{path:?}");
    assert!(
        first.t >= at(16, 40, 0) && first.t <= at(16, 50, 0),
        "{path:?}"
    );
    assert!(tr.budget_overrun.is_none(), "{:?}", tr.budget_overrun);
    assert!(tr.violations.is_empty(), "{:?}", tr.violations);
}

#[test]
fn s2_today_from_v1s_1711_state() {
    let dev4_w = ymd(10, 31, 12, 0, 0);
    let accts = vec![
        SimAccount::new(
            "dev8",
            40.0,
            90.0,
            at(21, 0, 0),
            dev4_w + Duration::hours(5),
        )
        .read(at(17, 10, 43)),
        SimAccount::new("dev4", 16.0, 92.0, at(20, 0, 0), dev4_w).read(at(16, 33, 48)),
        SimAccount::new("dev6", 10.0, 100.0, at(19, 0, 0), ymd(10, 26, 4, 59, 59))
            .read(at(15, 0, 0)),
    ];
    let mut sc = Scenario::new(at(17, 11, 14), at(18, 0, 0), "dev8", accts);
    sc.workload = vec![(at(17, 11, 0), 1.8, 0.47)];
    sc.mask_429 = Box::new(|k, t| k == "dev8" && t < at(17, 11, 30));
    let tr = run(sc);
    let real95 = tr
        .first_real_at_trigger
        .iter()
        .find(|(k, _)| k == "dev8")
        .map(|(_, t)| *t)
        .expect("dev8 reaches a Real 95");
    assert!(
        tr.switches.iter().all(|s| s.t >= real95),
        "no switch before dev8 reads 95: {:?}",
        tr.switches
            .iter()
            .map(|s| (s.t, &s.to, s.kind))
            .collect::<Vec<_>>()
    );
}

#[test]
fn s3_v087_thrash_oct1_seeds() {
    let start = ymd(10, 1, 7, 0, 0);
    let wr = ymd(10, 2, 18, 0, 0);
    let accts = vec![
        SimAccount::new("dev@", 16.0, 82.0, ymd(10, 1, 9, 0, 0), wr).read(start),
        SimAccount::new(
            "dev4",
            58.0,
            40.0,
            ymd(10, 1, 10, 0, 0),
            ymd(10, 4, 0, 0, 0),
        )
        .read(start),
        SimAccount::new(
            "dev3",
            30.0,
            50.0,
            ymd(10, 1, 8, 30, 0),
            ymd(10, 5, 0, 0, 0),
        )
        .read(start),
        SimAccount::new(
            "matthew@",
            20.0,
            60.0,
            ymd(10, 1, 11, 0, 0),
            ymd(10, 6, 0, 0, 0),
        )
        .read(start),
    ];
    let mut sc = Scenario::new(start, ymd(10, 1, 10, 0, 0), "dev@", accts);
    sc.trigger = 85.0;
    sc.workload = vec![(start, 0.08, 0.03)];
    sc.correlated = Some((0.5, Box::new(|_| true)));
    let tr = run(sc);
    let exits = tr.switches.iter().filter(|s| s.from == "dev@").count();
    assert!(
        exits <= 1,
        "{:?}",
        tr.switches
            .iter()
            .map(|s| (s.t, &s.from, &s.to))
            .collect::<Vec<_>>()
    );
    let aba = tr
        .switches
        .windows(2)
        .any(|w| w[0].from == w[1].to && w[0].to == w[1].from && w[0].kind == Kind::Trigger);
    assert!(!aba, "no A→B→A");
    assert!(tr.budget_overrun.is_none(), "{:?}", tr.budget_overrun);
}

/// dev@ at session 93 (trigger 95), every active read 429s for `streak_min`.
fn blind_run(streak_min: i64) -> (Trace, DateTime<Utc>) {
    let start = ymd(10, 1, 9, 0, 0);
    let accts = vec![
        SimAccount::new(
            "dev@",
            93.0,
            40.0,
            start + Duration::hours(3),
            start + Duration::days(4),
        )
        .read(start - Duration::seconds(30)),
        SimAccount::new(
            "dev4",
            10.0,
            20.0,
            start + Duration::hours(4),
            start + Duration::days(5),
        )
        .read(start - Duration::minutes(3)),
    ];
    let mut sc = Scenario::new(start, start + Duration::minutes(40), "dev@", accts);
    sc.workload = vec![(start, 1.0, 0.1)];
    let until = start + Duration::minutes(streak_min);
    sc.mask_429 = Box::new(move |k, t| k == "dev@" && t < until);
    let tr = run(sc);
    // True crossing of 95 at 1.0/min from 93: 2 min in.
    (tr, start + Duration::minutes(2))
}

#[test]
fn s4_v087_blind_run_6_min() {
    let (tr, crossing) = blind_run(6);
    let sw = tr.switches.first().expect("switched");
    assert_eq!(sw.kind, Kind::Trigger);
    assert!(
        sw.t <= crossing + Duration::minutes(2),
        "switched at {}",
        sw.t
    );
}

#[test]
fn s4_prime_blind_run_25_min() {
    let (tr, crossing) = blind_run(25);
    let sw = tr.switches.first().expect("switched");
    assert!(
        sw.t <= crossing + Duration::minutes(2),
        "switched at {}",
        sw.t
    );
    assert!(tr.budget_overrun.is_none());
}

/// B3 burst: idle 30 min, then 0.6/min weekly; `streak` makes the active
/// account 429 (p = 0.5 per read, or every read for S5′) in band.
fn burst(streak: Option<i64>) -> Trace {
    let start = at(15, 50, 0);
    let accts = vec![
        SimAccount::new("dev4", 12.0, 91.0, at(19, 0, 0), ymd(10, 31, 12, 0, 0)).read(start),
        SimAccount::new("dev8", 0.0, 60.0, at(20, 0, 0), ymd(10, 31, 17, 0, 0)).read(start),
    ];
    let mut sc = Scenario::new(start, start + Duration::minutes(60), "dev4", accts);
    sc.workload = vec![(start, 0.0, 0.0), (start + Duration::minutes(30), 1.0, 0.6)];
    match streak {
        Some(min) => {
            let from = start + Duration::minutes(30);
            let to = from + Duration::minutes(min);
            sc.mask_429 = Box::new(move |k, t| k == "dev4" && t >= from && t < to);
        }
        None => {
            let from = start + Duration::minutes(30);
            sc.correlated = Some((0.5, Box::new(move |t| t >= from)));
        }
    }
    run(sc)
}

#[test]
fn s5_b3_burst_after_idle() {
    let tr = burst(None);
    assert!(
        tr.blocked_minutes() <= 2.0,
        "blocked {}",
        tr.blocked_minutes()
    );
    assert!(!tr.switches.is_empty());
    assert!(tr.budget_overrun.is_none(), "{:?}", tr.budget_overrun);
}

#[test]
fn s5_prime_burst_with_25_min_streak() {
    let tr = burst(Some(25));
    assert!(
        tr.blocked_minutes() <= 2.0,
        "blocked {}",
        tr.blocked_minutes()
    );
    let crossing = at(15, 50, 0) + Duration::minutes(30) + Duration::seconds(400); // 91→95 at 0.6/min
    let sw = tr.switches.first().expect("switched");
    assert!(
        sw.t <= crossing + Duration::minutes(2),
        "switched at {}",
        sw.t
    );
}

#[test]
fn s6_b2_1536_1623_streak_no_switch() {
    let dev4_w = ymd(10, 31, 12, 0, 0);
    let accts = vec![
        SimAccount::new("dev4", 4.0, 89.0, at(20, 0, 0), dev4_w).read(at(15, 36, 59)),
        SimAccount::new(
            "dev8",
            100.0,
            80.0,
            at(16, 19, 59),
            dev4_w + Duration::hours(5),
        )
        .read(at(15, 30, 0)),
        SimAccount::new("dev6", 10.0, 100.0, at(19, 0, 0), ymd(10, 26, 4, 59, 59))
            .read(at(15, 0, 0)),
        SimAccount::new("dev7", 10.0, 100.0, at(19, 0, 0), ymd(10, 26, 7, 59, 59))
            .read(at(15, 0, 0)),
    ];
    let mut sc = Scenario::new(at(15, 37, 0), at(16, 23, 0), "dev4", accts);
    sc.workload = vec![(at(15, 37, 0), 0.05, 0.02)];
    sc.mask_429 = Box::new(|k, t| k == "dev4" && t >= at(15, 38, 0) && t < at(15, 59, 0));
    let tr = run(sc);
    assert_eq!(
        switch_count(&tr),
        0,
        "{:?}",
        tr.switches
            .iter()
            .map(|s| (s.t, &s.to, s.kind))
            .collect::<Vec<_>>()
    );
    assert_eq!(tr.notices, 0);
}

fn two_at_99(poll_tight_429: bool) -> Trace {
    let start = at(17, 25, 30);
    let accts = vec![
        SimAccount::new("A", 43.0, 99.0, at(20, 0, 0), ymd(10, 31, 12, 0, 0)).read(start),
        SimAccount::new("B", 30.0, 99.0, at(20, 0, 0), ymd(10, 31, 14, 0, 0))
            .read(start - Duration::seconds(20)),
    ];
    let mut sc = Scenario::new(start, start + Duration::minutes(20), "A", accts);
    sc.workload = vec![(start, 1.8, 0.47)];
    if poll_tight_429 {
        sc.mask_429 = Box::new(|_, t| (t.timestamp() / 30) % 2 == 0);
    }
    run(sc)
}

#[test]
fn s7_b1_two_accounts_at_99() {
    for flaky in [false, true] {
        let tr = two_at_99(flaky);
        // Before both are blocked: at most the one Serve A → B.
        let both_blocked_at = tr
            .switches
            .iter()
            .find(|s| s.kind == Kind::Prepare)
            .map(|s| s.t)
            .unwrap_or(at(23, 0, 0));
        let before = tr.switches.iter().filter(|s| s.t < both_blocked_at).count();
        assert!(
            before <= 1,
            "{:?}",
            tr.switches
                .iter()
                .map(|s| (s.t, &s.to, s.kind))
                .collect::<Vec<_>>()
        );
    }
}

fn overnight(pick_dev8: bool) -> Trace {
    let start = ymd(10, 26, 2, 0, 0);
    let accts = vec![
        SimAccount::new(
            "dev8",
            30.0,
            100.0,
            ymd(10, 26, 3, 0, 0),
            ymd(10, 26, 21, 0, 0),
        )
        .read(ymd(10, 25, 23, 0, 0)),
        SimAccount::new(
            "dev6",
            10.0,
            100.0,
            ymd(10, 26, 3, 30, 0),
            ymd(10, 26, 4, 59, 59),
        )
        .read(ymd(10, 25, 22, 0, 0)),
        SimAccount::new(
            "dev7",
            10.0,
            100.0,
            ymd(10, 26, 3, 30, 0),
            ymd(10, 26, 7, 59, 59),
        )
        .read(ymd(10, 25, 22, 0, 0)),
        SimAccount::new(
            "dev3",
            10.0,
            100.0,
            ymd(10, 26, 3, 30, 0),
            ymd(10, 27, 9, 0, 0),
        )
        .read(ymd(10, 25, 22, 0, 0)),
    ];
    let mut sc = Scenario::new(start, ymd(10, 26, 5, 10, 0), "dev8", accts);
    sc.workload = vec![(start, 0.5, 0.1)];
    // An active 429 streak all night (every dev8 fetch 429s).
    sc.mask_429 = Box::new(|k, _| k == "dev8");
    if pick_dev8 {
        sc.manual = vec![(start + Duration::minutes(1), "dev8".to_string())];
    }
    run(sc)
}

fn active_at(tr: &Trace, start_active: &str, t: DateTime<Utc>) -> String {
    tr.switches
        .iter()
        .rfind(|s| s.t <= t)
        .map(|s| s.to.clone())
        .unwrap_or_else(|| start_active.to_string())
}

#[test]
fn s8_all_blocked_overnight_resets() {
    let tr = overnight(false);
    assert_eq!(
        active_at(&tr, "dev8", ymd(10, 26, 5, 5, 0)),
        "dev6",
        "{:?}",
        tr.switches
            .iter()
            .map(|s| (s.t, &s.to, s.kind))
            .collect::<Vec<_>>()
    );
    assert!(tr.budget_overrun.is_none(), "{:?}", tr.budget_overrun);
}

#[test]
fn s8b_overnight_with_a_manual_pick_on_dev8() {
    let tr = overnight(true);
    assert_eq!(active_at(&tr, "dev8", ymd(10, 26, 5, 5, 0)), "dev6");
    let pick_end = tr
        .switches
        .first()
        .map(|s| s.t)
        .unwrap_or(ymd(10, 27, 0, 0, 0));
    assert!(
        !tr.switches
            .iter()
            .any(|s| s.kind == Kind::Prepare && s.t < pick_end),
        "no Prepare while the pick is active"
    );
    let dev8_fetches = tr
        .fetches
        .iter()
        .filter(|f| f.1 == "dev8" && f.0 > ymd(10, 26, 2, 1, 30) && f.0 < pick_end)
        .count();
    assert_eq!(dev8_fetches, 0, "0 dev8 fetches while it is lb-blocked");
}

#[test]
fn s9_credits_plan_exhausted_then_a_reset() {
    let start = at(12, 0, 0);
    let reset = at(13, 0, 0);
    let mut cr =
        SimAccount::new("cred", 20.0, 100.0, at(16, 0, 0), ymd(10, 30, 0, 0, 0)).read(start);
    cr.credits = Some(80_000);
    let accts = vec![
        SimAccount::new("act", 20.0, 99.0, at(16, 0, 0), ymd(10, 30, 0, 0, 0)).read(start),
        cr,
        SimAccount::new("plan", 10.0, 100.0, at(16, 0, 0), reset).read(start - Duration::hours(1)),
    ];
    let mut sc = Scenario::new(start, at(13, 30, 0), "act", accts);
    sc.workload = vec![(start, 1.0, 0.3)];
    sc.use_credits = true;
    let tr = run(sc);
    let onto = tr
        .switches
        .iter()
        .find(|s| s.kind == Kind::Credits)
        .expect("onto credits");
    assert_eq!(onto.to, "cred");
    let off = tr
        .switches
        .iter()
        .find(|s| s.from == "cred")
        .expect("leaves credits");
    assert_eq!(off.to, "plan");
    assert!(
        off.t <= reset + Duration::minutes(4),
        "left credits at {}",
        off.t
    );
}

fn manual_pick_at(pct: f64) -> Trace {
    let start = at(12, 0, 0);
    let accts = vec![
        SimAccount::new("other", 10.0, 20.0, at(16, 0, 0), ymd(10, 30, 0, 0, 0)).read(start),
        SimAccount::new("pick", 10.0, pct, at(16, 0, 0), ymd(10, 30, 0, 0, 0))
            .read(start - Duration::seconds(30)),
        SimAccount::new("tgt", 10.0, 30.0, at(16, 0, 0), ymd(10, 31, 0, 0, 0)).read(start),
    ];
    let mut sc = Scenario::new(start, at(13, 30, 0), "other", accts);
    sc.workload = vec![(start, 0.5, 0.3)];
    sc.manual = vec![(start + Duration::minutes(1), "pick".to_string())];
    // 429s on alternate pick reads.
    sc.mask_429 = alternate_429("pick", start, at(23, 0, 0));
    run(sc)
}

#[test]
fn s10_manual_picks_at_79_96_100() {
    // 79: leaves at a Real 95.
    let tr = manual_pick_at(79.0);
    let out = tr
        .switches
        .iter()
        .find(|s| s.from == "pick")
        .expect("leaves");
    assert_eq!(out.kind, Kind::Trigger);
    let real95 = tr
        .first_real_at_trigger
        .iter()
        .find(|(k, _)| k == "pick")
        .unwrap()
        .1;
    assert!(out.t >= real95);
    // 96: holds to a Real 99.5.
    let tr = manual_pick_at(96.0);
    let out = tr
        .switches
        .iter()
        .find(|s| s.from == "pick")
        .expect("leaves");
    assert_eq!(out.kind, Kind::Serve, "held until lb-blocked");
    // 100: leaves for a verified server, never prepared away.
    let tr = manual_pick_at(100.0);
    let out = tr
        .switches
        .iter()
        .find(|s| s.from == "pick")
        .expect("leaves");
    assert_eq!(out.kind, Kind::Serve);
    assert!(!tr
        .switches
        .iter()
        .any(|s| s.kind == Kind::Prepare && s.from == "pick"));
}

#[test]
fn s11_idle_overnight_then_resume() {
    let start = ymd(10, 25, 22, 0, 0);
    let accts = vec![
        SimAccount::new(
            "act",
            30.0,
            88.0,
            ymd(10, 26, 1, 0, 0),
            ymd(10, 30, 0, 0, 0),
        )
        .read(start),
        // In the band (3 pts under the trigger) with its reset 5 h out: never a
        // flip target (12 min of floored runway < 15).
        SimAccount::new(
            "near",
            20.0,
            92.0,
            ymd(10, 26, 1, 0, 0),
            ymd(10, 26, 3, 0, 0),
        )
        .read(start),
        SimAccount::new(
            "roomy",
            20.0,
            40.0,
            ymd(10, 26, 1, 0, 0),
            ymd(10, 31, 0, 0, 0),
        )
        .read(start),
    ];
    let mut sc = Scenario::new(start, ymd(10, 26, 9, 0, 0), "act", accts);
    sc.workload = vec![(start, 0.0, 0.0), (ymd(10, 26, 8, 0, 0), 0.6, 0.1)];
    let tr = run(sc);
    assert!(
        tr.switches.iter().all(|s| s.t >= ymd(10, 26, 3, 0, 0)),
        "no switch while idle: {:?}",
        tr.switches
            .iter()
            .map(|s| (s.t, &s.to, s.kind))
            .collect::<Vec<_>>()
    );
    // Never a flip into the band (a Rebalance target must be Ready).
    assert!(tr
        .switches
        .iter()
        .filter(|s| s.kind == Kind::Rebalance)
        .all(|s| s.target.ready_basis && s.to != "near"));
}

#[test]
fn s12_post_leave_lag_no_return_on_a_pre_lag_reading() {
    let mut sc = Scenario::new(at(16, 20, 10), at(18, 0, 0), "dev4", today_1620());
    sc.workload = today_workload();
    sc.post_leave_lag = true;
    let tr = run(sc);
    for s in &tr.switches {
        if let (Some(left), Some(read)) = (s.target_left_at, s.target_fetched_at) {
            assert!(
                read >= left + policy::POST_LEAVE_SETTLE_S || s.kind == Kind::Prepare,
                "returned to {} on a pre-lag reading at {}",
                s.to,
                s.t
            );
        }
    }
}

#[test]
fn s13_skipped_integer_values_98_to_100() {
    for rate in [1.0, 0.75] {
        let start = at(17, 50, 0);
        let accts = vec![
            SimAccount::new("dev8", 70.0, 97.5, at(20, 0, 0), ymd(10, 31, 12, 0, 0)).read(start),
            SimAccount::new("dev4", 20.0, 93.0, at(20, 0, 0), ymd(10, 31, 14, 0, 0))
                .read(start - Duration::seconds(30)),
        ];
        let mut sc = Scenario::new(start, start + Duration::minutes(30), "dev8", accts);
        sc.workload = vec![(start, 1.0, rate)];
        let tr = run(sc);
        assert!(
            tr.blocked_minutes() <= 3.0,
            "blocked {}",
            tr.blocked_minutes()
        );
        for s in tr.switches.iter().filter(|s| s.kind == Kind::Serve) {
            assert!(s.source.blocked, "no Serve before lb ≥ 99.5");
        }
    }
}

#[test]
fn s14_correlated_429_storm_in_band() {
    let start = at(15, 0, 0);
    let accts = vec![
        SimAccount::new("act", 30.0, 90.0, at(19, 0, 0), ymd(10, 31, 12, 0, 0)).read(start),
        SimAccount::new("b", 10.0, 30.0, at(19, 0, 0), ymd(10, 31, 14, 0, 0)).read(start),
        SimAccount::new("c", 10.0, 60.0, at(19, 0, 0), ymd(10, 31, 16, 0, 0)).read(start),
    ];
    let mut sc = Scenario::new(start, start + Duration::minutes(40), "act", accts);
    sc.workload = vec![(start, 1.0, 0.47)];
    let until = start + Duration::minutes(25);
    sc.correlated = Some((0.5, Box::new(move |t| t < until)));
    let tr = run(sc);
    assert!(
        tr.blocked_minutes() <= 2.0,
        "blocked {}",
        tr.blocked_minutes()
    );
    assert!(tr.budget_overrun.is_none(), "{:?}", tr.budget_overrun);
}

#[test]
fn s15_sleep_8h_first_fetch_after_wake_fails() {
    let start = ymd(10, 25, 23, 0, 0);
    let accts = vec![
        SimAccount::new(
            "act",
            30.0,
            92.0,
            ymd(10, 26, 2, 0, 0),
            ymd(10, 30, 0, 0, 0),
        )
        .read(start - Duration::seconds(30)),
        SimAccount::new("b", 10.0, 20.0, ymd(10, 26, 2, 0, 0), ymd(10, 31, 0, 0, 0)).read(start),
    ];
    let mut sc = Scenario::new(start, ymd(10, 26, 7, 30, 0), "act", accts);
    sc.workload = vec![(start, 0.0, 0.0)];
    sc.sleep = vec![(start + Duration::minutes(1), ymd(10, 26, 7, 1, 0))];
    let tr = run(sc);
    let first_ok = tr
        .fetches
        .iter()
        .find(|f| f.1 == "act" && f.2 == "ok" && f.0 > ymd(10, 26, 7, 0, 0))
        .map(|f| f.0);
    let first_switch = tr.switches.first().map(|s| s.t);
    if let Some(sw) = first_switch {
        assert!(
            first_ok.is_some_and(|ok| sw >= ok),
            "switched at {sw} before a read"
        );
    }
    assert_eq!(tr.switches.len(), 0, "idle at 92 is no reason to move");
}

#[test]
fn s16_external_change_to_a_blocked_account_mid_drain() {
    let start = at(17, 30, 0);
    let accts = vec![
        SimAccount::new("A", 30.0, 97.0, at(20, 0, 0), ymd(10, 31, 12, 0, 0)).read(start),
        SimAccount::new("X", 30.0, 100.0, at(20, 0, 0), ymd(10, 31, 13, 0, 0))
            .read(start - Duration::minutes(30)),
        SimAccount::new("B", 10.0, 40.0, at(20, 0, 0), ymd(10, 31, 14, 0, 0))
            .read(start - Duration::seconds(30)),
    ];
    let mut sc = Scenario::new(start, start + Duration::minutes(20), "A", accts);
    sc.workload = vec![(start, 1.0, 0.2)];
    let ext = start + Duration::minutes(3);
    sc.external = vec![(ext, "X".to_string())];
    let tr = run(sc);
    let off_x = tr
        .switches
        .iter()
        .find(|s| s.from == "X")
        .expect("served off X");
    assert_eq!(off_x.kind, Kind::Serve);
    let cycles = tr
        .decisions
        .iter()
        .filter(|(t, _)| *t >= ext && *t <= off_x.t)
        .count();
    assert!(cycles <= 2, "{cycles} cycles");
    assert!(tr.violations.is_empty(), "{:?}", tr.violations);
}

// ---------------------------------------------------------------------------
// Randomized invariants (the design's proptest, on the crate's own `rand`)
// ---------------------------------------------------------------------------

#[test]
fn random_fleets_hold_i1_i2_i4_and_never_nan() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(0x0122);
    for case in 0..120 {
        let start = at(10, 0, 0);
        let n = rng.gen_range(2..=9);
        let mut accts = Vec::new();
        for i in 0..n {
            let s = rng.gen_range(0.0..100.0f64).floor();
            let w = rng.gen_range(50.0..100.0f64).floor();
            let sr = start + Duration::minutes(rng.gen_range(5..300));
            let wr = start + Duration::minutes(rng.gen_range(30..10_000));
            let age = rng.gen_range(0..3_000);
            let mut a = SimAccount::new(&format!("a{i}"), s, w, sr, wr)
                .read(start - Duration::seconds(age));
            if rng.gen_bool(0.25) {
                a.credits = Some(rng.gen_range(1_000..100_000));
            }
            accts.push(a);
        }
        let mut sc = Scenario::new(start, start + Duration::hours(3), "a0", accts);
        sc.use_credits = rng.gen_bool(0.5);
        sc.workload = vec![(start, rng.gen_range(0.0..2.0), rng.gen_range(0.0..0.6))];
        let masks: Vec<f64> = (0..n).map(|_| rng.gen_range(0.0..0.6)).collect();
        let seed: u64 = rng.gen();
        sc.mask_429 = Box::new(move |k, t| {
            let i: usize = k[1..].parse().unwrap_or(0);
            let h = (t.timestamp() as u64 / 60)
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add(seed ^ i as u64);
            ((h % 1000) as f64 / 1000.0) < masks[i]
        });
        sc.seed = seed;
        let tr = run(sc);
        assert!(tr.violations.is_empty(), "case {case}: {:?}", tr.violations);
        let mut last_switch: Option<DateTime<Utc>> = None;
        for s in &tr.switches {
            // No NaN anywhere in the ranking inputs.
            for a in [&s.source, &s.target] {
                assert!(
                    !a.soft_runway.is_nan() && !a.hard_runway.is_nan(),
                    "case {case}"
                );
            }
            // I7: credits only when no plan account can serve.
            if s.kind == Kind::Credits {
                assert!(
                    !s.servable_existed,
                    "case {case}: credits over a servable plan at {}",
                    s.t
                );
            }
            // I2: forced moves don't downgrade.
            if matches!(s.kind, Kind::Trigger | Kind::Drain) {
                assert!(
                    s.target.hard_runway >= s.source.hard_runway,
                    "case {case}: I2 at {}",
                    s.t
                );
            }
            // I4: latches stop re-entry within the epoch…
            if matches!(s.kind, Kind::Trigger | Kind::Rebalance) {
                assert!(!s.target.trigger_latched, "case {case}: trigger latch");
            }
            if s.kind == Kind::Drain {
                assert!(!s.target.drained_latched, "case {case}: drained latch");
            }
            // …and optional moves are at least the cooldown apart.
            if s.kind.optional() {
                if let Some(prev) = last_switch {
                    assert!(
                        (s.t - prev).num_seconds() >= policy::SWAP_COOLDOWN_S,
                        "case {case}: optional move inside the cooldown"
                    );
                }
            }
            last_switch = Some(s.t);
        }
        assert!(
            tr.budget_overrun.is_none(),
            "case {case}: {:?}",
            tr.budget_overrun
        );
    }
}
