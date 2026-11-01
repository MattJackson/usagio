//! Auto-swap engine v2: an evidence-gated runway engine
//! (`docs/design/autoswap-redesign.md`, v3.1).
//!
//! [`decide`] is pure: no clock, network or globals. It returns `Stay`,
//! `Switch(target, kind)` or `NeedReading(accounts)`; in the last case the
//! shell ([`shell`]) fetches those accounts (within the request budget) and
//! calls `decide` again.
//!
//! Model principle: usage only goes up within a window until that window
//! resets. A reading that says a window is full is a lower bound valid until
//! the window resets (at any age) or a newer reading arrives. Only readings
//! that say a window *has room* go out of date.

pub mod assess;
pub mod memory;
pub mod policy;
pub mod rates;
pub mod runway;
pub mod shell;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::trait_def::Money;
use assess::{assess, dominant_unit, rank_cmp, Assessed, Ctx};
pub use memory::SwapMemory;
pub use policy::Policy;
use policy::*;
pub use rates::AccountRate;

/// A usage window the engine reasons about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowId {
    Session,
    Weekly,
}

impl WindowId {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowId::Session => "session",
            WindowId::Weekly => "weekly",
        }
    }
}

/// One window's latest successful reading.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowIn {
    pub pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// One account as the engine sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountIn {
    pub key: String,
    /// `None` = the provider doesn't report it (Codex session).
    pub session: Option<WindowIn>,
    pub weekly: Option<WindowIn>,
    /// Last *successful* reading (unix secs).
    pub fetched_at: Option<i64>,
    pub rate: AccountRate,
    /// Spendable credits (matter only once the plan is exhausted).
    pub credits: Option<Money>,
    pub lapsed: bool,
    /// A `NeedReading` fetch failed (or was budget-deferred) this cycle.
    pub unverified: bool,
    /// A `NeedReading` fetch was attempted (or deferred) this cycle.
    pub attempted: bool,
    /// Active only: its fetch is in backoff or 429'd this cycle (no
    /// `NeedReading(active)`).
    pub in_backoff: bool,
}

impl AccountIn {
    pub fn new(key: &str) -> Self {
        AccountIn {
            key: key.to_string(),
            session: None,
            weekly: None,
            fetched_at: None,
            rate: AccountRate::default(),
            credits: None,
            lapsed: false,
            unverified: false,
            attempted: false,
            in_backoff: false,
        }
    }
}

/// Tiers, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Ready,
    Draining,
    Credits,
    Blocked,
    Unknown,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Ready => "ready",
            Tier::Draining => "draining",
            Tier::Credits => "credits",
            Tier::Blocked => "blocked",
            Tier::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Some(match s {
            "ready" => Tier::Ready,
            "draining" => Tier::Draining,
            "credits" => Tier::Credits,
            "blocked" => Tier::Blocked,
            "unknown" => Tier::Unknown,
            _ => return None,
        })
    }
}

/// Evidence class of a "has room" reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Real,
    Estimated,
    Stale,
}

/// The user's manual pick of the active account.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualPick {
    pub key: String,
    pub at: i64,
    /// Tier from the switch-time reading, re-settled by the first fresh one.
    pub settled: Tier,
    pub settled_by_fresh: bool,
}

/// Everything `decide` needs.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub provider: String,
    pub active: String,
    /// Switchable others plus the active account.
    pub accounts: Vec<AccountIn>,
    pub manual: Option<ManualPick>,
    pub memory: SwapMemory,
    pub policy: Policy,
    /// Detected sleep `(from, to)` intervals (unix secs).
    pub sleep: Vec<(i64, i64)>,
}

/// What kind of move a switch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Trigger,
    Drain,
    Serve,
    OffCredits,
    Credits,
    Prepare,
    Rebalance,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Trigger => "trigger",
            Kind::Drain => "drain",
            Kind::Serve => "serve",
            Kind::OffCredits => "off_credits",
            Kind::Credits => "credits",
            Kind::Prepare => "prepare_reset",
            Kind::Rebalance => "rebalance",
        }
    }

    /// Optional moves are cooldown-gated and off while a pick is active.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn optional(self) -> bool {
        matches!(self, Kind::Rebalance | Kind::Prepare)
    }
}

/// The decided action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Stay,
    Switch { to: String, kind: Kind },
    NeedReading,
}

/// Why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    Disabled,
    ActiveUnknown,
    AllBlocked,
    OnCredits,
    ManualHold,
    ManualPick,
    Draining,
    Ok,
    Cooldown,
    /// A switch or a read for that kind of move.
    Move(Kind),
    /// Pre-verify the best Ready candidate while the active is blind.
    PreVerify,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Disabled => "disabled",
            Reason::ActiveUnknown => "active_unknown",
            Reason::AllBlocked => "all_blocked",
            Reason::OnCredits => "on_credits",
            Reason::ManualHold => "manual_hold",
            Reason::ManualPick => "manual_pick",
            Reason::Draining => "draining",
            Reason::Ok => "ok",
            Reason::Cooldown => "cooldown",
            Reason::Move(k) => k.as_str(),
            Reason::PreVerify => "pre_verify",
        }
    }
}

/// How urgently the active account needs watching (feeds the cadence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    Calm,
    Band,
    Draining,
    Blocked,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Calm => "calm",
            Urgency::Band => "band",
            Urgency::Draining => "draining",
            Urgency::Blocked => "blocked",
        }
    }
}

/// The active account's numbers, for the decision log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diag {
    pub lb: f64,
    pub est: f64,
    pub proj: Option<f64>,
    pub soft_runway: f64,
    pub hard_runway: f64,
    pub rate_session: Option<f64>,
    pub rate_weekly: Option<f64>,
}

/// `decide`'s result.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    pub reason: Reason,
    /// Accounts to read before deciding again (`NeedReading`), best first.
    pub need_reading: Vec<String>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub urgency: Urgency,
    /// Draining/Blocked with any candidate that is verified, unknown or
    /// could be read (TIGHT cadence instead of BASE).
    pub actionable: bool,
    pub active_tier: Tier,
    /// The target an optional move would take but for the cooldown.
    pub blocked_target: Option<String>,
    /// This stay is "stuck" (all blocked, or draining with no drain target):
    /// the shell sends the stuck notice once.
    pub stuck: bool,
    /// The active account's tier is backed by a Real reading (re-arms the
    /// stuck notice on a change, §3.6).
    pub active_real: bool,
    pub diag: Diag,
}

impl Decision {
    pub fn target(&self) -> Option<&str> {
        match &self.action {
            Action::Switch { to, .. } => Some(to),
            _ => self.blocked_target.as_deref(),
        }
    }

    pub fn action_str(&self) -> &'static str {
        match &self.action {
            Action::Stay if self.reason == Reason::Cooldown => "blocked",
            Action::Stay => "stayed",
            Action::Switch { .. } => "switching",
            Action::NeedReading => "need_reading",
        }
    }
}

/// What an arm decided before the common fields are filled in.
enum Out {
    Stay(Reason),
    StayStuck(Reason),
    Blocked(String),
    Switch(String, Kind),
    Need(Vec<String>, Reason),
}

struct Decider<'a> {
    s: &'a Snapshot,
    act: Assessed,
    /// The active account's measured rates.
    act_rate: AccountRate,
    cands: Vec<Assessed>,
    unit: Option<String>,
    pick: Option<&'a ManualPick>,
}

impl<'a> Decider<'a> {
    fn sorted(&self, f: impl Fn(&Assessed) -> bool) -> Vec<&Assessed> {
        let mut v: Vec<&Assessed> = self.cands.iter().filter(|c| f(c)).collect();
        v.sort_by(|a, b| rank_cmp(a, b, self.s.now, self.unit.as_deref()));
        v
    }

    fn cooldown_active(&self) -> bool {
        self.s
            .memory
            .last_switch_at
            .is_some_and(|t| self.s.now.timestamp() - t < SWAP_COOLDOWN_S)
    }

    /// The evidence post-pass (§3.4) for a ranked candidate list: switch to
    /// the first verified one; ask for a read of the first unread one (and
    /// the next unread); a Trigger target whose read failed or was deferred
    /// still qualifies with a post-leave, post-reset reading and twice the
    /// landing runway.
    fn evidence_pick(&self, list: &[&Assessed], kind: Kind) -> Option<Out> {
        for (i, c) in list.iter().enumerate() {
            if c.verified {
                return Some(Out::Switch(c.key.clone(), kind));
            }
            if c.attempted || c.unverified {
                if kind == Kind::Trigger
                    && c.post_leave_ok
                    && !c.any_pending
                    && c.soft_runway >= 2.0 * MIN_LANDING_MIN
                {
                    return Some(Out::Switch(c.key.clone(), kind));
                }
                continue;
            }
            let mut need = vec![c.key.clone()];
            if let Some(n) = list[i + 1..]
                .iter()
                .find(|n| !n.verified && !n.attempted && !n.unverified)
            {
                need.push(n.key.clone());
            }
            return Some(Out::Need(need, Reason::Move(kind)));
        }
        None
    }

    /// Reset-pending candidates not yet read this cycle, soonest reset first.
    fn reset_pending_unread(&self) -> Vec<String> {
        let mut v: Vec<&Assessed> = self
            .cands
            .iter()
            .filter(|c| c.any_pending && !c.blocked && !c.attempted)
            .collect();
        let soonest = |c: &Assessed| {
            c.windows
                .iter()
                .filter(|w| w.pending)
                .filter_map(|w| w.resets_at)
                .min()
                .unwrap_or(DateTime::<Utc>::MAX_UTC)
        };
        v.sort_by(|a, b| soonest(a).cmp(&soonest(b)).then(a.key.cmp(&b.key)));
        v.into_iter().map(|c| c.key.clone()).collect()
    }

    fn active_readable(&self) -> bool {
        !self.act.in_backoff && !self.act.attempted
    }

    fn arms(&self) -> Out {
        let act = &self.act;
        let s = self.s;

        // 0. Unknown / stale active (I8). A lower bound needs no read.
        let lb_known = act.blocked || act.tier == Tier::Credits;
        if !lb_known
            && (act.tier == Tier::Unknown
                || (act.evidence == Evidence::Stale && !act.blind_in_band))
        {
            let mut need = Vec::new();
            if self.active_readable() {
                need.push(act.key.clone());
            }
            need.extend(self.reset_pending_unread());
            return if need.is_empty() {
                Out::Stay(Reason::ActiveUnknown)
            } else {
                Out::Need(need, Reason::ActiveUnknown)
            };
        }

        // 1. Serve: the active account is lb-blocked or lapsed (I1) — also
        //    when it can run on credits: plan room always comes first (I7).
        let on_credits = act.tier == Tier::Credits;
        if act.tier == Tier::Blocked || on_credits || act.lapsed {
            let servable = self.sorted(|c| c.servable);
            let t = servable
                .iter()
                .find(|c| !c.drained_latched)
                .or(servable.first());
            if let Some(t) = t {
                let kind = if on_credits {
                    Kind::OffCredits
                } else {
                    Kind::Serve
                };
                return Out::Switch(t.key.clone(), kind);
            }
            let mut pending = self.reset_pending_unread();
            for c in self.sorted(|c| !c.blocked && !c.servable && !c.any_pending && !c.attempted) {
                pending.push(c.key.clone());
            }
            if !pending.is_empty() {
                return Out::Need(pending, Reason::Move(Kind::Serve));
            }
            // Nothing servable and every reset-pending account read: keep
            // working on credits; never prepared away from them.
            if on_credits {
                return Out::Stay(Reason::OnCredits);
            }
            if s.policy.use_credits {
                let credits = self.sorted(|c| c.tier == Tier::Credits);
                if let Some(o) = self.evidence_pick(&credits, Kind::Credits) {
                    return o;
                }
            }
            if let Some(o) = self.prepare() {
                return o;
            }
            return Out::StayStuck(Reason::AllBlocked);
        }

        // 3. Draining active.
        if act.tier == Tier::Draining {
            if let Some(p) = self.pick {
                if p.settled >= Tier::Draining {
                    return Out::Stay(Reason::ManualHold);
                }
                // A pick leaves at the trigger only on a Real reading at it,
                // never on an estimate or projection (0.10.4).
                let lb_over = act.windows.iter().any(|w| w.binds && w.lb >= w.limit);
                if act.evidence != Evidence::Real || !lb_over {
                    if self.active_readable() && act.evidence != Evidence::Real {
                        return Out::Need(vec![act.key.clone()], Reason::ManualPick);
                    }
                    return Out::Stay(Reason::ManualPick);
                }
            }
            let ready = self.sorted(|c| c.ready_basis && c.hard_runway >= act.hard_runway);
            if let Some(o) = self.evidence_pick(&ready, Kind::Trigger) {
                return o;
            }
            if act.lb_drain_eligible() {
                let need = MIN_DRAIN_LANDING_MIN.max(act.hard_runway + DRAIN_GAIN_MIN);
                let drain = self.sorted(|c| {
                    c.tier == Tier::Draining && !c.drained_latched && c.hard_runway >= need
                });
                if let Some(o) = self.evidence_pick(&drain, Kind::Drain) {
                    return o;
                }
            }
            // B4: a reset may have freed an account; read it (storm-guard
            // exempt) before riding on.
            let pending = self.reset_pending_unread();
            if !pending.is_empty() {
                return Out::Need(pending, Reason::Move(Kind::Trigger));
            }
            return Out::StayStuck(Reason::Draining);
        }

        // 3P. Blind in band: pre-verify the best Ready target (never a pick).
        if act.blind_in_band && self.pick.is_none() {
            let ready = self.sorted(|c| c.ready_basis);
            if let Some(best) = ready.first() {
                if !best.verified && !best.attempted && !best.unverified {
                    return Out::Need(vec![best.key.clone()], Reason::PreVerify);
                }
            }
        }

        // 4. Ready active.
        if self.pick.is_some() {
            return Out::Stay(Reason::ManualPick);
        }
        // A flip only pays for room that would otherwise expire; while the
        // user is measurably idle (both own rates 0) nothing is used either
        // way, so don't pay the cache rebuild (S11).
        let idle = [WindowId::Session, WindowId::Weekly]
            .iter()
            .all(|w| self.act_rate.own(*w).is_some_and(|r| r <= RATE_EPSILON));
        if let Some(act_reset) = act.weekly_reset.filter(|_| !idle) {
            let horizon = s.now + Duration::hours(REBALANCE_HORIZON_H);
            let rebalance = self.sorted(|c| {
                c.ready_basis
                    && !c.trigger_latched
                    && c.weekly_reset.is_some_and(|r| r < horizon && r < act_reset)
            });
            if let Some(best) = rebalance.first() {
                if self.cooldown_active() {
                    return Out::Blocked(best.key.clone());
                }
                if let Some(o) = self.evidence_pick(&rebalance, Kind::Rebalance) {
                    return o;
                }
            }
        }
        Out::Stay(Reason::Ok)
    }

    /// Prepare (all blocked, §3.6): every precondition, v1 parity (B12).
    fn prepare(&self) -> Option<Out> {
        let act = &self.act;
        let act_until = act.blocked_until?;
        // A confirmed block ends manual protection: prepare an earlier unlock
        // even after a manual pick, while retaining cooldown and evidence gates.
        if act.lapsed || act.tier == Tier::Credits {
            return None;
        }
        if self.cands.iter().any(|c| !c.has_reading) {
            return None;
        }
        if !self.reset_pending_unread().is_empty() {
            return None;
        }
        let targets = self
            .sorted(|c| c.tier == Tier::Blocked && c.blocked_until.is_some_and(|u| u < act_until));
        let best = targets.first()?;
        if self.cooldown_active() {
            return Some(Out::Blocked(best.key.clone()));
        }
        Some(Out::Switch(best.key.clone(), Kind::Prepare))
    }

    fn urgency(&self) -> Urgency {
        let act = &self.act;
        match act.tier {
            Tier::Blocked | Tier::Credits => Urgency::Blocked,
            _ if act.lapsed => Urgency::Blocked,
            Tier::Draining => Urgency::Draining,
            Tier::Ready
                if act.blind_in_band
                    || act
                        .windows
                        .iter()
                        .any(|w| w.binds && w.lb.max(w.est) >= w.limit - BLIND_BAND) =>
            {
                Urgency::Band
            }
            _ => Urgency::Calm,
        }
    }

    fn next_check_at(&self, out: &Out) -> Option<DateTime<Utc>> {
        let now = self.s.now;
        let mut best: Option<DateTime<Utc>> = None;
        let mut take = |t: DateTime<Utc>| {
            if t > now {
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        };
        let act_in = self
            .s
            .accounts
            .iter()
            .find(|a| a.key.eq_ignore_ascii_case(&self.act.key));
        if let Some(a) = act_in.filter(|_| !self.act.blocked) {
            for w in self.act.windows.iter().filter(|w| w.binds) {
                let r = a.rate.own(w.id).unwrap_or(0.0);
                for limit in [w.limit, DRAIN_EXIT_PCT] {
                    let m = runway::time_to(w.est, limit, r);
                    if m.is_finite() && m > 0.0 {
                        take(now + Duration::seconds((m * 60.0) as i64));
                    }
                }
            }
        }
        for a in &self.s.accounts {
            for w in [a.session.as_ref(), a.weekly.as_ref()]
                .into_iter()
                .flatten()
            {
                if let Some(r) = w.resets_at {
                    take(r);
                }
            }
        }
        if let (Out::Blocked(_), Some(t)) = (out, self.s.memory.last_switch_at) {
            take(DateTime::from_timestamp(t + SWAP_COOLDOWN_S, 0).unwrap_or(now));
        }
        if self.act.blind_in_band {
            if let Some(f) = self
                .sorted(|c| c.ready_basis && c.verified)
                .first()
                .and_then(|c| c.fetched_at)
            {
                take(DateTime::from_timestamp(f + TARGET_VERIFY_MAX_AGE_S + 1, 0).unwrap_or(now));
            }
        }
        best
    }
}

/// The engine (§3.4). Pure.
pub fn decide(s: &Snapshot) -> Decision {
    let ctx = Ctx {
        now: s.now,
        policy: &s.policy,
        memory: &s.memory,
        sleep: &s.sleep,
    };
    let stay = |reason: Reason, tier: Tier| Decision {
        action: Action::Stay,
        reason,
        need_reading: Vec::new(),
        next_check_at: None,
        urgency: Urgency::Calm,
        actionable: false,
        active_tier: tier,
        blocked_target: None,
        stuck: false,
        active_real: false,
        diag: Diag::default(),
    };
    let Some(act_in) = s
        .accounts
        .iter()
        .find(|a| a.key.eq_ignore_ascii_case(&s.active))
    else {
        return stay(Reason::ActiveUnknown, Tier::Unknown);
    };
    if !s.policy.enabled || s.policy.env_override {
        return stay(Reason::Disabled, Tier::Unknown);
    }
    let act = assess(act_in, true, &ctx);
    let cands: Vec<Assessed> = s
        .accounts
        .iter()
        .filter(|a| !a.key.eq_ignore_ascii_case(&s.active))
        .map(|a| assess(a, false, &ctx))
        .collect();
    let mut all = cands.clone();
    all.push(act.clone());
    let unit = dominant_unit(&all);
    let pick = s
        .manual
        .as_ref()
        .filter(|m| m.key.eq_ignore_ascii_case(&act.key));
    let d = Decider {
        s,
        act,
        act_rate: act_in.rate,
        cands,
        unit,
        pick,
    };
    let out = d.arms();
    let urgency = d.urgency();
    let actionable = matches!(urgency, Urgency::Draining | Urgency::Blocked)
        && d.cands
            .iter()
            .any(|c| !c.blocked || c.tier == Tier::Unknown);
    let next_check_at = d.next_check_at(&out);
    let diag = Diag {
        lb: d.act.max_lb(),
        est: d.act.max_est(),
        proj: d.act.max_proj(),
        soft_runway: d.act.soft_runway,
        hard_runway: d.act.hard_runway,
        rate_session: act_in.rate.session,
        rate_weekly: act_in.rate.weekly,
    };
    let (action, reason, need_reading, blocked_target, stuck) = match out {
        Out::Stay(r) => (Action::Stay, r, Vec::new(), None, false),
        Out::StayStuck(r) => (Action::Stay, r, Vec::new(), None, true),
        Out::Blocked(t) => (Action::Stay, Reason::Cooldown, Vec::new(), Some(t), false),
        Out::Switch(to, kind) => (
            Action::Switch { to, kind },
            Reason::Move(kind),
            Vec::new(),
            None,
            false,
        ),
        Out::Need(keys, r) => {
            let mut v: Vec<String> = Vec::new();
            for k in keys {
                if !v.iter().any(|x| x.eq_ignore_ascii_case(&k)) {
                    v.push(k);
                }
            }
            (Action::NeedReading, r, v, None, false)
        }
    };
    Decision {
        action,
        reason,
        need_reading,
        next_check_at,
        urgency,
        actionable,
        active_tier: d.act.tier,
        blocked_target,
        stuck,
        active_real: d.act.evidence == Evidence::Real,
        diag,
    }
}

/// Assess every account of `s` (for logs, the shell and tests).
pub fn assess_all(s: &Snapshot) -> Vec<Assessed> {
    let ctx = Ctx {
        now: s.now,
        policy: &s.policy,
        memory: &s.memory,
        sleep: &s.sleep,
    };
    s.accounts
        .iter()
        .map(|a| assess(a, a.key.eq_ignore_ascii_case(&s.active), &ctx))
        .collect()
}
