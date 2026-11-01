//! Table tests on `decide` (design §8.3): one test per row, `now` injected,
//! times UTC 2026-10-25 unless noted.

use super::assess::{rank_cmp, Assessed};
use super::memory::{ChangeKind, Latch, SwapMemory};
use super::rates::{rates_from_points, AccountRate, Point};
use super::runway::{binding_window, time_to, BindingShown};
use super::shell::{decide_with_reads, Budget, DecisionLog, Defer, ReadResult};
use super::*;
use chrono::{Duration, TimeZone};

// --- fixtures ---------------------------------------------------------------

fn ymd(mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, mo, d, h, mi, s).unwrap()
}

/// 2026-10-25 at h:m:s UTC.
fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    ymd(10, 25, h, m, s)
}

/// A weekly reset far enough out to never matter (Oct 31).
fn far() -> DateTime<Utc> {
    ymd(10, 31, 3, 0, 0)
}

/// An account read at `read` with session/weekly pct; the session resets 4 h
/// after the reading, the weekly at `far()`; default (no-data) rates.
fn acct(key: &str, s: f64, w: f64, read: DateTime<Utc>) -> AccountIn {
    AccountIn {
        session: Some(WindowIn {
            pct: s,
            resets_at: Some(read + Duration::hours(4)),
        }),
        weekly: Some(WindowIn {
            pct: w,
            resets_at: Some(far()),
        }),
        fetched_at: Some(read.timestamp()),
        ..AccountIn::new(key)
    }
}

trait AcctExt {
    fn rate(self, s: f64, w: f64) -> Self;
    fn peak(self, s: f64, w: f64) -> Self;
    fn sreset(self, t: Option<DateTime<Utc>>) -> Self;
    fn wreset(self, t: Option<DateTime<Utc>>) -> Self;
    fn backoff(self) -> Self;
    fn failed_read(self) -> Self;
    fn credits(self, minor: i64) -> Self;
}

impl AcctExt for AccountIn {
    fn rate(mut self, s: f64, w: f64) -> Self {
        self.rate = AccountRate {
            session: Some(s),
            weekly: Some(w),
            ..self.rate
        };
        self
    }
    fn peak(mut self, s: f64, w: f64) -> Self {
        self.rate.peak_session = Some(s);
        self.rate.peak_weekly = Some(w);
        self
    }
    fn sreset(mut self, t: Option<DateTime<Utc>>) -> Self {
        if let Some(w) = self.session.as_mut() {
            w.resets_at = t;
        }
        self
    }
    fn wreset(mut self, t: Option<DateTime<Utc>>) -> Self {
        if let Some(w) = self.weekly.as_mut() {
            w.resets_at = t;
        }
        self
    }
    fn backoff(mut self) -> Self {
        self.in_backoff = true;
        self
    }
    fn failed_read(mut self) -> Self {
        self.attempted = true;
        self.unverified = true;
        self
    }
    fn credits(mut self, minor: i64) -> Self {
        self.credits = Money::new(minor, 2, "USD");
        self
    }
}

fn snap(now: DateTime<Utc>, active: &str, accounts: Vec<AccountIn>) -> Snapshot {
    Snapshot {
        now,
        provider: "claude".to_string(),
        active: active.to_string(),
        accounts,
        manual: None,
        memory: SwapMemory::default(),
        policy: Policy::new(95.0, 95.0),
        sleep: Vec::new(),
    }
}

fn pick(key: &str, at: DateTime<Utc>, settled: Tier) -> Option<ManualPick> {
    Some(ManualPick {
        key: key.to_string(),
        at: at.timestamp(),
        settled,
        settled_by_fresh: true,
    })
}

fn switched(d: &Decision) -> Option<(&str, Kind)> {
    match &d.action {
        Action::Switch { to, kind } => Some((to.as_str(), *kind)),
        _ => None,
    }
}

fn assessed(s: &Snapshot, key: &str) -> Assessed {
    assess_all(s)
        .into_iter()
        .find(|a| a.key == key)
        .expect("account in snapshot")
}

fn latch(window: WindowId, resets_at: Option<DateTime<Utc>>, pct: f64, at: DateTime<Utc>) -> Latch {
    Latch {
        window,
        resets_at: resets_at.map(|r| r.timestamp()),
        pct,
        at: at.timestamp(),
    }
}

// --- incident and history rows ----------------------------------------------

#[test]
fn t1_today_1711_429_on_fresh_reading_stays() {
    let s = snap(
        at(17, 11, 14),
        "dev8",
        vec![
            acct("dev8", 40.0, 90.0, at(17, 10, 43))
                .rate(0.71, 0.18)
                .backoff(),
            acct("dev4", 16.0, 92.0, at(17, 5, 0)),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::Stay, "{d:?}");
    assert_eq!(assessed(&s, "dev4").tier, Tier::Draining);
}

#[test]
fn t2_today_1623_429_31s_old_stays() {
    let s = snap(
        at(16, 23, 37),
        "dev4",
        vec![
            acct("dev4", 12.0, 91.0, at(16, 23, 6))
                .rate(0.71, 0.18)
                .backoff(),
            acct("dev8", 0.0, 80.0, at(16, 20, 5)),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::Stay, "{d:?}");
    assert!(d.need_reading.is_empty());
}

#[test]
fn t3_today_1728_real_block_serves_immediately() {
    let mut s = snap(
        at(17, 28, 40),
        "dev4",
        vec![
            acct("dev4", 47.0, 100.0, at(17, 28, 31)),
            acct("dev8", 41.0, 90.0, at(17, 22, 27)).wreset(Some(far() + Duration::hours(5))),
        ],
    );
    s.memory
        .left_at
        .insert("dev8".to_string(), at(17, 11, 14).timestamp());
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev8", Kind::Serve)), "{d:?}");
    assert!(d.need_reading.is_empty(), "no read required");
}

#[test]
fn t4_today_session_vs_weekly_ranking() {
    let now = at(17, 0, 0);
    let s = snap(
        now,
        "dev6",
        vec![
            acct("dev6", 10.0, 100.0, now),
            acct("dev4", 16.0, 92.0, now),
            acct("dev8", 40.0, 90.0, now),
        ],
    );
    let mut c: Vec<Assessed> = assess_all(&s)
        .into_iter()
        .filter(|a| !a.is_active)
        .collect();
    c.sort_by(|a, b| rank_cmp(a, b, now, None));
    assert_eq!(c[0].key, "dev8", "40/90 ranks above 16/92");
    assert_eq!(c[1].key, "dev4");
}

#[test]
fn t19_rebalance_horizon_strict_and_6h() {
    let now = at(12, 0, 0);
    let mk = |reset: DateTime<Utc>| {
        snap(
            now,
            "act",
            vec![
                acct("act", 30.0, 50.0, now - Duration::seconds(30)),
                acct("cand", 10.0, 20.0, now - Duration::seconds(30)).wreset(Some(reset)),
            ],
        )
    };
    let d = decide(&mk(now + Duration::hours(6) - Duration::minutes(1)));
    assert_eq!(switched(&d), Some(("cand", Kind::Rebalance)), "{d:?}");
    let d = decide(&mk(now + Duration::hours(6)));
    assert_eq!(d.action, Action::Stay, "exactly 6 h is outside the horizon");
}

#[test]
fn t20_ready_requires_floored_runway() {
    let now = at(12, 0, 0);
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now),
            acct("idle", 10.0, 94.9, now).rate(0.0, 0.0),
        ],
    );
    let c = assessed(&s, "idle");
    assert_eq!(c.tier, Tier::Draining, "B7: an idle 94.9 is not Ready");
    assert!(!c.ready_basis);
    assert!(c.soft_runway.is_finite() && !c.soft_runway.is_nan());
    assert!(!c.hard_runway.is_nan());
    assert!((c.soft_runway - 0.4).abs() < 1e-6, "0.1 pt at 0.25/min");
}

#[test]
fn t21_rate_includes_zero_deltas_and_skips_gaps() {
    let now = at(12, 0, 0);
    let t = |min_ago: i64| now.timestamp() - min_ago * 60;
    let flat: Vec<Point> = (0..=15)
        .map(|i| Point {
            t: t(30 - i * 2),
            session: Some(40.0),
            weekly: Some(70.0),
        })
        .collect();
    let r = rates_from_points(&flat, now);
    assert_eq!(r.session, Some(0.0), "flat data gives rate 0");
    assert_eq!(r.weekly, Some(0.0));
    // An 8 h gap is ignored; a drop (reset) is skipped.
    let gappy = vec![
        Point {
            t: t(8 * 60 + 10),
            session: Some(5.0),
            weekly: Some(10.0),
        },
        Point {
            t: t(10),
            session: Some(50.0),
            weekly: Some(60.0),
        },
        Point {
            t: t(5),
            session: Some(55.0),
            weekly: Some(61.0),
        },
        Point {
            t: t(3),
            session: Some(2.0),
            weekly: Some(61.0),
        },
    ];
    let r = rates_from_points(&gappy, now);
    assert_eq!(
        r.session,
        Some(1.0),
        "5 pts over 5 min; the drop is skipped"
    );
    assert!((r.weekly.unwrap() - 1.0 / 7.0).abs() < 1e-9);
    // Fewer than two readings: no rate (defaults apply).
    assert_eq!(rates_from_points(&gappy[..1], now).session, None);
}

#[test]
fn t22_credits_and_codex_logging() {
    let now = at(12, 0, 0);
    let mut a = acct("cx", 0.0, 100.0, now).credits(50_000);
    a.session = None; // Codex reports no session window
    let mut s = snap(now, "cx", vec![a]);
    s.provider = "codex".to_string();
    s.policy.use_credits = true;
    let mut log = DecisionLog::default();
    let mut lines = 0;
    for i in 0..10 {
        s.now = now + Duration::seconds(60 * i);
        let d = decide(&s);
        assert_eq!(d.reason, Reason::OnCredits);
        if log.should_log(&d) {
            lines += 1;
        }
    }
    assert_eq!(lines, 1, "one decision line, not ten");
}

#[test]
fn t23_prepare_requires_lb_block_and_known_until() {
    let now = at(12, 0, 0);
    let base = || {
        vec![
            acct("act", 0.0, 100.0, now).wreset(Some(now + Duration::days(5))),
            acct("soon", 0.0, 100.0, now).wreset(Some(now + Duration::hours(2))),
        ]
    };
    let d = decide(&snap(now, "act", base()));
    assert_eq!(switched(&d), Some(("soon", Kind::Prepare)), "{d:?}");
    // A row with no reading: not proof of all-blocked.
    let mut v = base();
    v.push(AccountIn::new("nodata"));
    let mut s = snap(now, "act", v);
    // (its read is attempted and fails, so only Prepare's precondition stops it)
    s.accounts[2].attempted = true;
    s.accounts[2].unverified = true;
    let d = decide(&s);
    assert!(switched(&d).is_none(), "{d:?}");
    // The active account's block has no known end.
    let mut v = base();
    v[0] = v[0].clone().wreset(None);
    assert!(switched(&decide(&snap(now, "act", v))).is_none());
    // The target's block has no known end.
    let mut v = base();
    v[1] = v[1].clone().wreset(None);
    assert!(switched(&decide(&snap(now, "act", v))).is_none());
}

#[test]
fn t24_drain_gain_rule_supersedes_5pt() {
    let now = at(12, 0, 0);
    let mk = |cand_w: f64| {
        snap(
            now,
            "act",
            vec![
                acct("act", 10.0, 99.0, now - Duration::seconds(20)),
                acct("cand", 10.0, cand_w, now - Duration::seconds(20)),
            ],
        )
    };
    let d = decide(&mk(98.0));
    assert!(switched(&d).is_none(), "98: 4 min of hard runway < 5");
    assert_eq!(d.reason, Reason::Draining);
    let d = decide(&mk(95.0));
    assert_eq!(switched(&d), Some(("cand", Kind::Drain)), "95: 16 min ≥ 5");
    let d = decide(&mk(90.0));
    assert_eq!(switched(&d), Some(("cand", Kind::Trigger)), "90: Ready");
}

// --- blocker scenarios from the review --------------------------------------

#[test]
fn t5_b1_two_at_real_99_no_pingpong() {
    let now = at(17, 25, 40);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 43.0, 99.0, at(17, 25, 29)),
            acct("devB", 30.0, 99.0, at(17, 25, 0)),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::Stay, "{d:?}");
    assert_eq!(d.reason, Reason::Draining);
    // Ride A to a real block, then Serve onto B.
    s.now = at(17, 28, 0);
    s.accounts[0] = acct("dev4", 47.0, 100.0, at(17, 27, 50));
    s.accounts[1].fetched_at = Some(at(17, 27, 0).timestamp());
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("devB", Kind::Serve)), "{d:?}");
}

#[test]
fn t6_b2_429_streak_no_phantom_prepare() {
    let now = at(16, 1, 0);
    let fleet = |backoff: bool| {
        let mut dev4 = acct("dev4", 4.0, 89.0, at(15, 36, 59));
        dev4.in_backoff = backoff;
        snap(
            now,
            "dev4",
            vec![
                dev4,
                acct("dev8", 100.0, 80.0, at(15, 50, 0)).sreset(Some(at(16, 19, 59))),
                acct("dev6", 10.0, 100.0, at(14, 0, 0)),
                acct("dev7", 10.0, 100.0, at(14, 0, 0)),
            ],
        )
    };
    let d = decide(&fleet(true));
    assert_eq!(d.action, Action::Stay);
    assert_eq!(d.reason, Reason::ActiveUnknown);
    assert!(d.need_reading.is_empty(), "none while in backoff");
    assert!(!d.stuck, "no notice");
    let d = decide(&fleet(false));
    assert_eq!(d.action, Action::NeedReading);
    assert_eq!(d.need_reading, vec!["dev4".to_string()]);
}

#[test]
fn t7_b2_sleep_8h_no_switch_on_no_info() {
    let now = at(8, 0, 0);
    let mut s = snap(
        now,
        "dev",
        vec![
            acct("dev", 30.0, 92.0, at(0, 0, 0)).backoff(),
            acct("tgt", 10.0, 10.0, at(7, 50, 0)).failed_read(),
        ],
    );
    s.sleep = vec![(at(0, 1, 0).timestamp(), at(7, 59, 0).timestamp())];
    let a = assessed(&s, "dev");
    assert!(a.blind_in_band);
    let proj = a.max_proj().unwrap();
    assert!(
        proj < 94.0,
        "sleep excluded: projection near 92, got {proj}"
    );
    let d = decide(&s);
    assert!(switched(&d).is_none(), "{d:?}");
    assert!(d.need_reading.is_empty());
}

#[test]
fn t8_b3_burst_after_idle_backstop() {
    let dev4 = || {
        acct("dev4", 12.0, 91.0, at(16, 23, 6))
            .rate(0.0, 0.0)
            .peak(0.0, 0.67)
            .backoff()
    };
    // Blind for 144 s: pre-verify dev8 (its reading is 3 min old).
    let s = snap(
        at(16, 25, 30),
        "dev4",
        vec![dev4(), acct("dev8", 0.0, 80.0, at(16, 22, 30))],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.reason, Reason::PreVerify);
    assert_eq!(d.need_reading, vec!["dev8".to_string()]);
    // Once the projection reaches 95 with dev8 verified: Trigger.
    let s = snap(
        at(16, 28, 30),
        "dev4",
        vec![dev4(), acct("dev8", 0.0, 80.0, at(16, 28, 0))],
    );
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev8", Kind::Trigger)), "{d:?}");
}

#[test]
fn t9_b4_reset_pending_verified_first() {
    let now = ymd(10, 26, 5, 0, 0);
    let s = snap(
        now,
        "dev8",
        vec![
            acct("dev8", 20.0, 100.0, ymd(10, 26, 4, 58, 0))
                .wreset(Some(ymd(10, 26, 21, 0, 0)))
                .backoff(),
            acct("dev7", 0.0, 100.0, at(23, 0, 0)).wreset(Some(ymd(10, 26, 7, 59, 59))),
            acct("dev6", 0.0, 100.0, at(23, 0, 0)).wreset(Some(ymd(10, 26, 4, 59, 59))),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.need_reading.first().map(String::as_str), Some("dev6"));
}

#[test]
fn t10_b4_unknown_active_keeps_weekly_block() {
    let now = at(19, 31, 0);
    let dev4 = || acct("dev4", 47.0, 100.0, at(19, 0, 0)).sreset(Some(at(19, 30, 0)));
    let d = decide(&snap(
        now,
        "dev4",
        vec![dev4(), acct("dev8", 50.0, 50.0, at(19, 0, 0))],
    ));
    assert_eq!(switched(&d), Some(("dev8", Kind::Serve)), "{d:?}");
    let d = decide(&snap(now, "dev4", vec![dev4()]));
    assert_eq!(d.reason, Reason::AllBlocked);
    assert!(
        !d.need_reading.contains(&"dev4".to_string()),
        "lb-blocked: not fetched"
    );
}

#[test]
fn t11_b4_credits_leave_after_reset_verified() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 10.0, 100.0, now - Duration::seconds(30)).credits(60_000),
            acct("dev6", 0.0, 100.0, at(6, 0, 0)).wreset(Some(now - Duration::minutes(10))),
        ],
    );
    s.policy.use_credits = true;
    let d = decide(&s);
    assert_eq!(d.active_tier, Tier::Credits);
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.need_reading, vec!["dev6".to_string()]);
    // The read shows the reset: off credits.
    s.accounts[1] = acct("dev6", 0.0, 0.0, now).wreset(Some(now + Duration::days(7)));
    s.accounts[1].attempted = true;
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev6", Kind::OffCredits)), "{d:?}");
}

#[test]
fn t12_b8_phantom_drift_no_aba() {
    let now = at(13, 0, 0);
    let left = at(12, 30, 0);
    let mut s = snap(
        now,
        "B",
        vec![
            // B crossed by drift: 94 read 5 min ago at 0.47/min.
            acct("B", 20.0, 94.0, now - Duration::minutes(5)).rate(1.0, 0.47),
            // A, Real, post-leave, 4 under the trigger.
            acct("A", 20.0, 91.0, now - Duration::seconds(30)),
        ],
    );
    s.memory.left_at.insert("a".to_string(), left.timestamp());
    s.memory
        .latch_trigger("A", latch(WindowId::Weekly, Some(far()), 91.0, left));
    let d = decide(&s);
    assert_eq!(d.active_tier, Tier::Draining);
    assert!(
        switched(&d).is_none(),
        "latched A is no Trigger target: {d:?}"
    );
    assert!(assessed(&s, "A").trigger_latched);
    // 5 under the trigger on a Real reading clears the latch.
    s.accounts[1] = acct("A", 20.0, 90.0, now - Duration::seconds(30));
    assert!(!assessed(&s, "A").trigger_latched);
}

#[test]
fn t13_b9_unsettled_pick_96_holds() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 20.0, 96.0, now - Duration::minutes(3)).backoff(),
            acct("dev8", 10.0, 10.0, now - Duration::seconds(30)),
        ],
    );
    s.manual = pick("dev4", now - Duration::minutes(3), Tier::Draining);
    let d = decide(&s);
    assert_eq!(d.reason, Reason::ManualHold, "{d:?}");
    assert_eq!(d.action, Action::Stay);
}

#[test]
fn t14_b9_pick_leaves_only_on_real_reading() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 20.0, 93.0, now - Duration::minutes(4)).rate(1.0, 0.5),
            acct("dev8", 10.0, 10.0, now - Duration::seconds(30)),
        ],
    );
    s.manual = pick("dev4", now - Duration::minutes(20), Tier::Ready);
    let d = decide(&s);
    assert!(switched(&d).is_none());
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.need_reading, vec!["dev4".to_string()]);
    // A Real 95: leaves at the trigger.
    s.accounts[0] = acct("dev4", 20.0, 95.0, now).rate(1.0, 0.5);
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev8", Kind::Trigger)), "{d:?}");
}

#[test]
fn t15_b9_held_pick_leaves_at_real_99_5() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 20.0, 97.0, now - Duration::minutes(4)).rate(1.0, 0.44),
            acct("dev8", 10.0, 10.0, now - Duration::minutes(30)),
        ],
    );
    s.manual = pick("dev4", now - Duration::hours(1), Tier::Draining);
    let d = decide(&s);
    assert!(assessed(&s, "dev4").max_est() > 99.0);
    assert_eq!(d.reason, Reason::ManualHold, "est 99.2: {d:?}");
    s.accounts[0] = acct("dev4", 20.0, 99.5, now);
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev8", Kind::Serve)), "{d:?}");
}

#[test]
fn t16_b10_serve_rejects_preleave_reading() {
    let now = at(17, 30, 0);
    let mut s = snap(
        now,
        "dev4",
        vec![
            acct("dev4", 47.0, 100.0, now - Duration::seconds(10)),
            acct("dev8", 41.0, 90.0, at(17, 0, 0)),
        ],
    );
    s.memory
        .left_at
        .insert("dev8".to_string(), at(17, 5, 0).timestamp());
    let d = decide(&s);
    assert!(switched(&d).is_none());
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.need_reading, vec!["dev8".to_string()]);
}

#[test]
fn t17_b15_post_leave_reading_needs_60s() {
    let now = at(16, 24, 0);
    let mut s = snap(
        now,
        "dev8",
        vec![
            acct("dev8", 0.0, 80.0, now),
            acct("dev4", 16.0, 92.0, at(16, 23, 50)),
        ],
    );
    s.memory
        .left_at
        .insert("dev4".to_string(), at(16, 23, 38).timestamp());
    let a = assessed(&s, "dev4");
    assert!(!a.post_leave_ok);
    assert!(!a.verified);
    assert!(!a.servable);
    assert_eq!(a.evidence, Evidence::Stale);
}

#[test]
fn t18_reset_pending_by_reported_reset() {
    let now = at(5, 1, 0);
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now),
            acct("c", 0.0, 100.0, at(4, 0, 0)).wreset(Some(now - Duration::seconds(60))),
        ],
    );
    assert!(assessed(&s, "c").any_pending);
    // A re-read 60 s later still reports the old reset (fast local clock):
    // still pending, and re-read at most once per 120 s.
    let mut b = Budget::default();
    let t0 = now.timestamp();
    assert!(b.allow("c", t0, false, true, 0).is_ok());
    b.record_read("c", t0, false);
    assert_eq!(
        b.allow("c", t0 + 60, false, true, 0),
        Err(Defer::PerAccount)
    );
    assert!(b.allow("c", t0 + 120, false, true, 0).is_ok());
    // A reading that reports the next reset is post-reset.
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now),
            acct("c", 0.0, 0.0, now).wreset(Some(now + Duration::days(7))),
        ],
    );
    assert!(!assessed(&s, "c").any_pending);
}

// --- v3 additional table tests -------------------------------------------------

#[test]
fn t25_r2b1_old_full_reading_still_blocked() {
    let s = snap(
        at(17, 35, 11),
        "dev4",
        vec![
            acct("dev4", 47.0, 100.0, at(17, 28, 31)).backoff(),
            acct("dev8", 10.0, 93.0, at(17, 35, 0)),
        ],
    );
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev8", Kind::Serve)), "{d:?}");
}

#[test]
fn t26_r2b3_overnight_locked_rows_need_no_reads() {
    let now = ymd(10, 26, 2, 30, 0);
    let read = at(20, 30, 0);
    let mut v = vec![acct("dev6", 0.0, 100.0, read).wreset(Some(ymd(10, 26, 4, 59, 59)))];
    for i in 0..9 {
        v.push(acct(&format!("o{i}"), 0.0, 100.0, read).wreset(Some(ymd(
            10,
            26,
            6 + i as u32,
            0,
            0,
        ))));
    }
    let d = decide(&snap(now, "dev6", v));
    assert_eq!(d.action, Action::Stay);
    assert_eq!(d.reason, Reason::AllBlocked, "{d:?}");
    assert!(d.need_reading.is_empty());
    assert!(!d.actionable, "BASE cadence overnight");
}

#[test]
fn t27_r2b2_blind_projection_no_cutoff() {
    let read = at(12, 0, 0);
    let mk = |blind_min: i64| {
        let now = read + Duration::minutes(blind_min);
        snap(
            now,
            "act",
            vec![
                acct("act", 20.0, 90.0, read).rate(0.0, 0.0).backoff(),
                acct("tgt", 10.0, 10.0, now - Duration::seconds(30)),
            ],
        )
    };
    assert!(switched(&decide(&mk(5))).is_none(), "proj 94.2 at 5 min");
    assert_eq!(switched(&decide(&mk(7))), Some(("tgt", Kind::Trigger)));
    assert_eq!(
        switched(&decide(&mk(25))),
        Some(("tgt", Kind::Trigger)),
        "no age cutoff"
    );
}

#[test]
fn t28_r2m1_serve_accepts_postleave_reading_after_failed_verify() {
    let mut s = snap(
        at(17, 28, 45),
        "dev4",
        vec![
            acct("dev4", 47.0, 100.0, at(17, 28, 31)),
            acct("dev8", 41.0, 90.0, at(17, 22, 27)).failed_read(),
        ],
    );
    s.memory
        .left_at
        .insert("dev8".to_string(), at(17, 11, 14).timestamp());
    assert_eq!(switched(&decide(&s)), Some(("dev8", Kind::Serve)));
}

#[test]
fn t29_r2m1_trigger_still_needs_2x_landing_if_unverified() {
    let now = at(12, 0, 0);
    let mk = |w: f64| {
        snap(
            now,
            "act",
            vec![
                acct("act", 20.0, 95.0, now - Duration::seconds(30)),
                acct("tgt", 10.0, w, now - Duration::minutes(10)).failed_read(),
            ],
        )
    };
    assert!(switched(&decide(&mk(90.0))).is_none(), "20 min < 2×15");
    assert_eq!(
        switched(&decide(&mk(85.0))),
        Some(("tgt", Kind::Trigger)),
        "40 min ≥ 2×15 with a post-leave reading"
    );
}

#[test]
fn t30_r2m1_reset_pct_drop_counts_as_post_reset() {
    let now = ymd(10, 26, 5, 0, 30);
    let s = snap(
        now,
        "dev8",
        vec![
            acct("dev8", 10.0, 100.0, now - Duration::seconds(20))
                .wreset(Some(ymd(10, 26, 21, 0, 0))),
            acct("dev6", 0.0, 0.0, ymd(10, 26, 5, 0, 5)).wreset(Some(ymd(11, 2, 5, 0, 0))),
        ],
    );
    let a = assessed(&s, "dev6");
    assert!(!a.any_pending);
    assert_eq!(a.tier, Tier::Ready);
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev6", Kind::Serve)));
    assert!(d.need_reading.is_empty(), "no re-read");
}

#[test]
fn t31_r2m2_manual_switch_records_left_at() {
    let t = at(12, 0, 0);
    let mut m = SwapMemory::default();
    m.note_active_change(Some("A"), "B", t.timestamp(), ChangeKind::Manual);
    assert_eq!(m.left_at("A"), Some(t.timestamp()));
    let mut s = snap(
        t + Duration::seconds(10),
        "B",
        vec![
            acct("B", 10.0, 10.0, t),
            acct("A", 10.0, 50.0, t - Duration::seconds(30)),
        ],
    );
    s.memory = m;
    assert!(!assessed(&s, "A").post_leave_ok, "A's reading is pre-leave");
}

#[test]
fn t32_r3b1_token_adoption_is_not_an_active_change() {
    // Same-account adoption: nothing changes.
    let mut st = crate::store::State::default();
    st.swap_memory.insert(
        "claude".to_string(),
        SwapMemory {
            last_switch_at: Some(100),
            last_seen_active: Some("a".to_string()),
            ..Default::default()
        },
    );
    st.active = Some("a".to_string());
    st.manual_picks
        .insert("claude".to_string(), "a".to_string());
    let before = st.swap_memory["claude"].clone();
    st.note_active_change("claude", Some("a"), "a", ChangeKind::Auto);
    assert_eq!(st.swap_memory["claude"], before);
    assert_eq!(st.manual_pick("claude"), Some("a"));
    // External: the active key changed with no note.
    let mut m = before.clone();
    let left = m.detect_external(Some("b"), 5_000);
    assert_eq!(left.as_deref(), Some("a"));
    assert_eq!(m.left_at("a"), Some(5_000));
    assert_eq!(m.last_switch_at, Some(5_000), "cooldown restarts");
    assert_eq!(m.last_seen_active.as_deref(), Some("b"));
    // A memory that never saw an active key just starts tracking.
    let mut fresh = SwapMemory::default();
    assert!(fresh.detect_external(Some("a"), 5_000).is_none());
    assert!(fresh.last_switch_at.is_none());
}

#[test]
fn t32b_external_change_clears_the_pick_under_the_lock() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut st = crate::store::State::default();
    let pa = crate::store::ProviderAccounts {
        active: Some("b".to_string()),
        ..Default::default()
    };
    st.providers.insert("codex".to_string(), pa);
    st.manual_picks.insert("codex".to_string(), "b".to_string());
    st.manual_pick_tier
        .insert("codex".to_string(), "draining".to_string());
    st.swap_memory.insert(
        "codex".to_string(),
        SwapMemory {
            last_seen_active: Some("a".to_string()),
            ..Default::default()
        },
    );
    st.save().unwrap();
    let now = Utc::now();
    let st = super::shell::begin_cycle("codex", now).unwrap();
    let m = &st.swap_memory["codex"];
    assert!(m.left_at("a").is_some());
    assert_eq!(m.last_switch_at, Some(now.timestamp()));
    assert!(!st.manual_picks.contains_key("codex"), "pick cleared");
    assert!(!st.manual_pick_tier.contains_key("codex"));
}

#[test]
fn t33_r2m2_memory_merge_keeps_cli_write() {
    let mut daemon = SwapMemory::default();
    daemon.left_at.insert("x".to_string(), 100);
    daemon.last_switch_at = Some(100);
    daemon.stuck_notified = true;
    let mut cli = SwapMemory::default();
    cli.left_at.insert("y".to_string(), 200);
    cli.left_at.insert("x".to_string(), 50);
    cli.last_switch_at = Some(200);
    daemon.merge_from(&cli);
    assert_eq!(daemon.left_at("x"), Some(100), "max per key");
    assert_eq!(daemon.left_at("y"), Some(200), "both entries survive");
    assert_eq!(daemon.last_switch_at, Some(200));
    assert!(daemon.stuck_notified, "the daemon's value");
    // Through the state lock: a CLI write between load and save survives.
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut st = crate::store::State::default();
    st.note_active_change("claude", Some("cli-left"), "z", ChangeKind::Manual);
    st.save().unwrap();
    super::shell::update_memory("claude", |m| {
        m.left_at.insert("daemon-left".to_string(), 7);
    })
    .unwrap();
    let m = crate::store::State::load().unwrap().swap_memory["claude"].clone();
    assert!(m.left_at("cli-left").is_some());
    assert!(m.left_at("daemon-left").is_some());
}

#[test]
fn t34_r2m2_dual_write_manual_holds() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let mut st = crate::store::State::default();
    let pa = crate::store::ProviderAccounts {
        active: Some("p@e.com".to_string()),
        ..Default::default()
    };
    st.providers.insert("codex".to_string(), pa);
    st.set_manual_pick("codex", Some("p@e.com"));
    st.set_manual_pick_tier("codex", Some("draining"));
    st.save().unwrap();
    super::shell::dual_write_hold("codex");
    let st = crate::store::State::load().unwrap();
    assert_eq!(
        st.manual_holds.get("codex").map(String::as_str),
        Some("p@e.com")
    );
    // After a toggle to the classic engine, v1 holds it.
    assert_eq!(st.manual_hold("codex"), Some("p@e.com"));
    // Settled Ready: no hold.
    let mut st = st;
    st.set_manual_pick_tier("codex", Some("ready"));
    st.save().unwrap();
    super::shell::dual_write_hold("codex");
    assert_eq!(
        crate::store::State::load().unwrap().manual_hold("codex"),
        None
    );
}

#[test]
fn t35_r2m4_budget_storm_guard() {
    let t0 = at(12, 0, 0).timestamp();
    let mut b = Budget::default();
    b.note_429(t0 - 30);
    assert_eq!(b.allow("x", t0, false, false, 0), Err(Defer::StormGuard));
    assert!(
        b.allow("x", t0, false, true, 0).is_ok(),
        "reset-pending exempt"
    );
    assert!(
        b.allow("x", t0 + 31, false, false, 0).is_ok(),
        "guard lapses"
    );
    // ≤ 3 requests per 60 s in total (active + verification).
    let mut b = Budget::default();
    b.note_request(t0);
    b.record_read("a", t0, false);
    b.record_read("b", t0, false);
    assert_eq!(b.allow("c", t0 + 1, false, true, 0), Err(Defer::PerMinute));
    assert_eq!(
        b.allow("act", t0 + 1, true, false, 0),
        Err(Defer::PerMinute)
    );
    // ≤ 2 verification reads per cycle, ≤ 20 per hour.
    let b2 = Budget::default();
    assert_eq!(b2.allow("x", t0, false, false, 2), Err(Defer::PerCycle));
    let mut b3 = Budget::default();
    for i in 0..20 {
        b3.record_read(&format!("k{i}"), t0 - 3000 + i * 150, false);
    }
    assert_eq!(b3.allow("y", t0, false, false, 0), Err(Defer::PerHour));

    // Through the read loop: an active Serve with one reset-pending candidate
    // and two pre-leave ones, right after a 429 — only the reset-pending one
    // is read.
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now),
            acct("pend", 0.0, 100.0, at(5, 0, 0)).wreset(Some(now - Duration::minutes(1))),
            acct("pre1", 10.0, 10.0, at(9, 0, 0)),
            acct("pre2", 10.0, 10.0, at(9, 0, 0)),
        ],
    );
    s.memory
        .left_at
        .insert("pre1".to_string(), at(10, 0, 0).timestamp());
    s.memory
        .left_at
        .insert("pre2".to_string(), at(10, 0, 0).timestamp());
    let mut b = Budget::default();
    b.note_429(now.timestamp() - 10);
    let mut asked = Vec::new();
    let (_d, log) = decide_with_reads(&mut s, &mut b, &mut |k, _| {
        asked.push(k.to_string());
        ReadResult::Failed
    });
    assert_eq!(asked, vec!["pend".to_string()]);
    assert!(log
        .iter()
        .all(|r| r.key == "pend" || r.outcome == "defer:storm_guard"));
    assert!(b.requests_last_minute(now.timestamp()) <= PROVIDER_REQ_PER_MIN);
}

#[test]
fn t36_r2b2_dev8_1752_skip_to_100_with_pick() {
    let others = |now: DateTime<Utc>| {
        let _ = now;
        vec![
            acct("dev4", 47.0, 100.0, at(17, 28, 31)),
            acct("dev6", 0.0, 100.0, at(17, 0, 0)).wreset(Some(ymd(10, 26, 4, 59, 59))),
            acct("dev7", 0.0, 100.0, at(17, 0, 0)).wreset(Some(ymd(10, 26, 7, 59, 59))),
        ]
    };
    let mut v = vec![acct("dev8", 70.0, 98.0, at(17, 52, 43)).wreset(Some(ymd(10, 26, 21, 0, 0)))];
    v.extend(others(at(17, 52, 50)));
    let mut s = snap(at(17, 52, 50), "dev8", v);
    s.manual = pick("dev8", at(17, 30, 55), Tier::Ready);
    let d = decide(&s);
    assert_eq!(d.reason, Reason::Draining, "{d:?}");
    assert!(d.stuck);
    assert!(d.need_reading.is_empty(), "0 verification reads");
    // 17:55:46 Real 100: the pick no longer prevents preparing an earlier reset.
    s.now = at(17, 55, 50);
    s.accounts[0] = acct("dev8", 76.0, 100.0, at(17, 55, 46)).wreset(Some(ymd(10, 26, 21, 0, 0)));
    let d = decide(&s);
    assert_eq!(switched(&d), Some(("dev6", Kind::Prepare)), "{d:?}");
    assert!(d.need_reading.is_empty(), "0 dev8 fetches afterwards");
}

// --- v3.1 round-3 table tests --------------------------------------------------

#[test]
fn t37_r3b3_claimed_reset_starts_new_epoch() {
    let now = at(12, 0, 0);
    let reset = ymd(11, 1, 3, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now - Duration::seconds(10)),
            acct("dev8", 0.0, 0.0, now - Duration::seconds(10)).wreset(Some(reset)),
        ],
    );
    let l = latch(
        WindowId::Weekly,
        Some(reset),
        100.0,
        now - Duration::hours(1),
    );
    s.memory.latch_trigger("dev8", l.clone());
    s.memory.latch_drained("dev8", l);
    let a = assessed(&s, "dev8");
    assert!(!a.blocked);
    assert!(!a.trigger_latched && !a.drained_latched, "latches void");
    assert_eq!(a.tier, Tier::Ready);
    // As the active account: no Serve away.
    s.active = "dev8".to_string();
    let d = decide(&s);
    assert!(switched(&d).is_none(), "{d:?}");
}

#[test]
fn t38_r3b2_pick_on_blocked_dev8_reset_read_moves_user() {
    let now = ymd(10, 26, 5, 0, 5);
    let mut s = snap(
        now,
        "dev8",
        vec![
            acct("dev8", 50.0, 100.0, at(21, 0, 0)).wreset(Some(ymd(10, 26, 21, 0, 0))),
            acct("dev6", 0.0, 100.0, at(22, 0, 0)).wreset(Some(ymd(10, 26, 4, 59, 59))),
        ],
    );
    s.manual = pick("dev8", at(21, 0, 0), Tier::Blocked);
    let mut b = Budget::default();
    b.note_429(now.timestamp() - 20); // a 429 on dev8 within the last 60 s
    let mut asked = Vec::new();
    let (d, _) = decide_with_reads(&mut s, &mut b, &mut |k, _| {
        asked.push(k.to_string());
        assert_ne!(k, "dev8", "dev8 is never fetched while lb-blocked");
        let mut fresh = acct("dev6", 0.0, 0.0, now).wreset(Some(ymd(11, 2, 5, 0, 0)));
        fresh.key = k.to_string();
        ReadResult::Ok(Box::new(fresh))
    });
    assert_eq!(asked, vec!["dev6".to_string()], "despite the storm guard");
    assert_eq!(switched(&d), Some(("dev6", Kind::Serve)), "{d:?}");
}

#[test]
fn t39_r3m1_projection_never_drains() {
    let now = at(12, 0, 0);
    let act = || {
        acct("act", 20.0, 93.0, now - Duration::minutes(10))
            .rate(1.0, 0.47)
            .peak(2.0, 1.0)
            .backoff()
    };
    let s = snap(
        now,
        "act",
        vec![act(), acct("drn", 10.0, 97.0, now - Duration::seconds(30))],
    );
    assert!(assessed(&s, "act").max_proj().unwrap() >= 99.0);
    let d = decide(&s);
    assert!(switched(&d).is_none(), "no Drain on a projection: {d:?}");
    let s = snap(
        now,
        "act",
        vec![
            act(),
            acct("drn", 10.0, 97.0, now - Duration::seconds(30)),
            acct("rdy", 10.0, 10.0, now - Duration::seconds(30)),
        ],
    );
    assert_eq!(switched(&decide(&s)), Some(("rdy", Kind::Trigger)));
}

// --- invariants and v2 counterparts of flipped v1 tests (§8.4) ----------------

#[test]
fn a_429_near_the_trigger_moves_nobody() {
    // Flip of `a_429_near_the_trigger_does_not_move_a_manual_pick` (and of
    // `first_429_near_trigger_swaps_immediately_even_after_a_recent_swap`):
    // a 429 changes only reading age.
    let now = at(12, 0, 0);
    for manual in [None, pick("act", now - Duration::hours(1), Tier::Ready)] {
        let mut s = snap(
            now,
            "act",
            vec![
                acct("act", 20.0, 93.0, now - Duration::seconds(40)).backoff(),
                acct("tgt", 10.0, 10.0, now - Duration::seconds(30)),
            ],
        );
        s.manual = manual;
        s.memory.last_switch_at = Some((now - Duration::minutes(1)).timestamp());
        assert!(switched(&decide(&s)).is_none());
    }
}

#[test]
fn banded_account_that_resets_soonest_is_used_in_drain_mode() {
    // Flip of `reactive_swap_drains_a_banded_account_that_resets_soonest`: a
    // banded account (too close to the trigger to be Ready) is used by the
    // drain exit once the active account reads 99.
    let now = at(12, 0, 0);
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 99.0, now - Duration::seconds(30)),
            acct("banded", 20.0, 93.0, now - Duration::seconds(30))
                .wreset(Some(now + Duration::hours(3))),
        ],
    );
    assert_eq!(assessed(&s, "banded").tier, Tier::Draining);
    assert_eq!(switched(&decide(&s)), Some(("banded", Kind::Drain)));
}

#[test]
fn no_proactive_swap_without_a_reset_inside_the_horizon() {
    // Flips of `choose_swap_target_proactive_swap_beyond_headroom_margin` and
    // `proactive_flip_back_needs_a_full_margin_when_the_active_reset_is_unknown`.
    let now = at(12, 0, 0);
    let roomy = |reset: Option<DateTime<Utc>>| {
        acct("cand", 5.0, 5.0, now - Duration::seconds(30)).wreset(reset)
    };
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 60.0, 60.0, now - Duration::seconds(30)),
            roomy(Some(now + Duration::days(2))),
        ],
    );
    assert!(
        switched(&decide(&s)).is_none(),
        "far more headroom is not a reason"
    );
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 60.0, 60.0, now - Duration::seconds(30)).wreset(None),
            roomy(Some(now + Duration::hours(1))),
        ],
    );
    assert!(
        switched(&decide(&s)).is_none(),
        "unknown active reset: no flip"
    );
}

#[test]
fn a_post_leave_reading_allows_returning() {
    // Flip of `choose_swap_target_excludes_recently_left_account`.
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 96.0, now - Duration::seconds(30)),
            acct("left", 10.0, 10.0, now - Duration::seconds(30)),
        ],
    );
    s.memory
        .left_at
        .insert("left".to_string(), (now - Duration::minutes(5)).timestamp());
    s.memory.last_switch_at = Some((now - Duration::minutes(5)).timestamp());
    assert_eq!(switched(&decide(&s)), Some(("left", Kind::Trigger)));
}

#[test]
fn hold_lets_go_exactly_at_99_5_on_a_real_reading() {
    let now = at(12, 0, 0);
    let mk = |w: f64| {
        let mut s = snap(
            now,
            "held",
            vec![
                acct("held", 20.0, w, now - Duration::seconds(20)),
                acct("tgt", 10.0, 10.0, now - Duration::seconds(30)),
            ],
        );
        s.manual = pick("held", now - Duration::hours(1), Tier::Draining);
        s
    };
    assert_eq!(decide(&mk(99.4)).reason, Reason::ManualHold);
    assert_eq!(switched(&decide(&mk(99.5))), Some(("tgt", Kind::Serve)));
}

#[test]
fn reset_clock_alone_never_clears_a_target_but_a_reading_at_the_reset_does() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 100.0, now - Duration::seconds(20)),
            acct("c", 0.0, 100.0, at(6, 0, 0)).wreset(Some(now - Duration::minutes(2))),
        ],
    );
    let d = decide(&s);
    assert!(switched(&d).is_none());
    assert_eq!(d.need_reading, vec!["c".to_string()]);
    s.accounts[1] = acct("c", 0.0, 0.0, now).wreset(Some(now + Duration::days(7)));
    assert_eq!(switched(&decide(&s)), Some(("c", Kind::Serve)));
}

#[test]
fn credits_are_used_only_when_on_and_every_plan_is_blocked() {
    let now = at(12, 0, 0);
    let fleet = |use_credits: bool, roomy: bool| {
        let mut v = vec![
            acct("act", 10.0, 100.0, now - Duration::seconds(20)),
            acct("rich", 10.0, 100.0, now - Duration::seconds(20)).credits(90_000),
        ];
        if roomy {
            v.push(acct("plan", 10.0, 50.0, now - Duration::seconds(20)));
        }
        let mut s = snap(now, "act", v);
        s.policy.use_credits = use_credits;
        s
    };
    assert!(
        switched(&decide(&fleet(false, false))).is_none(),
        "credits off"
    );
    assert_eq!(
        switched(&decide(&fleet(true, false))),
        Some(("rich", Kind::Credits))
    );
    assert_eq!(
        switched(&decide(&fleet(true, true))),
        Some(("plan", Kind::Serve)),
        "plan room beats credits"
    );
    // On credits: never prepared away onto a blocked account.
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now - Duration::seconds(20))
                .credits(90_000)
                .wreset(Some(now + Duration::days(5))),
            acct("soon", 10.0, 100.0, now - Duration::seconds(20))
                .wreset(Some(now + Duration::hours(1))),
        ],
    );
    s.policy.use_credits = true;
    let d = decide(&s);
    assert_eq!(d.reason, Reason::OnCredits);
}

#[test]
fn serve_and_drain_latches_bound_churn() {
    // I4: an account left by Drain is not a drain target again in its epoch,
    // but the latch yields to I1 when every other account is blocked.
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 99.0, now - Duration::seconds(20)),
            acct("prev", 10.0, 94.0, now - Duration::seconds(20)),
        ],
    );
    s.memory.latch_drained(
        "prev",
        latch(
            WindowId::Weekly,
            Some(far()),
            94.0,
            now - Duration::hours(1),
        ),
    );
    let d = decide(&s);
    assert!(switched(&d).is_none(), "drained-latched: no drain target");
    s.accounts[0] = acct("act", 20.0, 100.0, now - Duration::seconds(20));
    assert_eq!(
        switched(&decide(&s)),
        Some(("prev", Kind::Serve)),
        "the latch yields to I1"
    );
}

#[test]
fn disabled_or_env_override_stays() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![acct("act", 20.0, 100.0, now), acct("c", 10.0, 10.0, now)],
    );
    s.policy = Policy::new(101.0, 101.0);
    assert_eq!(decide(&s).reason, Reason::Disabled);
    s.policy = Policy::new(95.0, 95.0);
    s.policy.env_override = true;
    assert_eq!(decide(&s).reason, Reason::Disabled);
}

#[test]
fn trigger_of_exactly_100_still_serves_off_a_full_account() {
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![acct("act", 20.0, 100.0, now), acct("c", 10.0, 10.0, now)],
    );
    s.policy = Policy::new(100.0, 100.0);
    assert_eq!(switched(&decide(&s)), Some(("c", Kind::Serve)));
}

#[test]
fn urgency_and_next_check_feed_the_cadence() {
    let now = at(12, 0, 0);
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 92.0, now - Duration::seconds(20)).rate(1.0, 0.5),
            acct("c", 10.0, 10.0, now - Duration::seconds(20)),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.urgency, Urgency::Band);
    let next = d.next_check_at.unwrap();
    assert!(next > now && next < now + Duration::minutes(10), "{next}");
    let s = snap(now, "act", vec![acct("act", 20.0, 40.0, now)]);
    assert_eq!(decide(&s).urgency, Urgency::Calm);
}

// --- binding display (§4) ----------------------------------------------------

fn display(s: f64, w: f64, rs: Option<f64>, rw: Option<f64>) -> AccountIn {
    let now = at(17, 16, 0);
    let mut a = acct("d", s, w, now);
    a.rate = AccountRate {
        session: rs,
        weekly: rw,
        ..Default::default()
    };
    a
}

#[test]
fn binding_1716_dev4_shows_weekly() {
    let b = binding_window(&display(29.0, 96.0, Some(1.8), Some(0.47)), at(17, 16, 0)).unwrap();
    assert_eq!(b.window, WindowId::Weekly);
    assert_eq!(b.pct, 96.0);
}

#[test]
fn binding_1630_dev8_weekly_binds_first() {
    let b = binding_window(&display(12.0, 83.0, Some(0.71), Some(0.18)), at(17, 16, 0)).unwrap();
    // 124 min (session) vs 94 min (weekly).
    assert_eq!(b.window, WindowId::Weekly);
    assert!((b.time_to_block.unwrap() - 17.0 / 0.18).abs() < 1e-6);
}

#[test]
fn binding_session_reset_in_10_min_does_not_bind() {
    let now = at(17, 16, 0);
    let a = display(95.0, 50.0, Some(0.2), Some(0.1)).sreset(Some(now + Duration::minutes(10)));
    assert_eq!(binding_window(&a, now).unwrap().window, WindowId::Weekly);
}

#[test]
fn binding_no_rate_uses_the_higher_pct() {
    let now = at(17, 16, 0);
    assert_eq!(
        binding_window(&display(70.0, 40.0, None, None), now)
            .unwrap()
            .window,
        WindowId::Session
    );
    assert_eq!(
        binding_window(&display(30.0, 40.0, None, None), now)
            .unwrap()
            .window,
        WindowId::Weekly
    );
}

#[test]
fn binding_reset_pending_window_is_skipped() {
    let now = at(17, 16, 0);
    let a = display(99.0, 40.0, None, None).sreset(Some(now - Duration::minutes(1)));
    assert_eq!(binding_window(&a, now).unwrap().window, WindowId::Weekly);
}

#[test]
fn binding_hysteresis_does_not_flicker_within_the_margin() {
    let t0 = at(17, 0, 0);
    // Session 100 min vs weekly ~95–105 min, alternating.
    let mut shown = BindingShown::update(None, &display(0.0, 0.0, Some(1.0), Some(1.0)), t0, 95.0);
    let first = shown.unwrap().window;
    for i in 1..10 {
        let t = t0 + Duration::seconds(30 * i);
        let w: f64 = if i % 2 == 0 { 5.0 } else { -5.0 };
        // weekly time to block = (100 − (w)) / 1.0 vs session 100.
        let a = display(
            0.0,
            w.max(0.0),
            Some(1.0),
            Some(if w < 0.0 { 0.95 } else { 1.0 }),
        );
        shown = BindingShown::update(shown, &a, t, 95.0);
        assert_eq!(shown.unwrap().window, first, "flickered at step {i}");
    }
    // A decisive lead after the dwell switches.
    let a = display(0.0, 60.0, Some(1.0), Some(1.0));
    let later = BindingShown::update(shown, &a, t0 + Duration::minutes(6), 95.0).unwrap();
    assert_eq!(later.window, WindowId::Weekly);
}

#[test]
fn binding_override_at_the_trigger_skips_the_dwell() {
    let t0 = at(17, 0, 0);
    let shown = BindingShown::update(None, &display(70.0, 50.0, None, None), t0, 95.0);
    assert_eq!(shown.unwrap().window, WindowId::Session);
    let a = display(70.0, 96.0, None, None);
    let next = BindingShown::update(shown, &a, t0 + Duration::seconds(10), 95.0).unwrap();
    assert_eq!(next.window, WindowId::Weekly, "96%w at once");
}

#[test]
fn binding_codex_weekly_only() {
    let now = at(17, 16, 0);
    let mut a = display(0.0, 1.0, None, None);
    a.session = None;
    let b = binding_window(&a, now).unwrap();
    assert_eq!(b.window, WindowId::Weekly);
    assert_eq!(b.pct, 1.0);
}

#[test]
fn time_to_never_nans() {
    assert_eq!(time_to(50.0, 95.0, 0.0), f64::INFINITY);
    assert_eq!(time_to(96.0, 95.0, 0.0), 0.0);
    assert_eq!(time_to(f64::NAN, 95.0, 1.0), 0.0);
    assert_eq!(time_to(90.0, 95.0, f64::NAN), f64::INFINITY);
    assert!((time_to(90.0, 95.0, 0.5) - 10.0).abs() < 1e-12);
}

// --- review fixes (0.12.2 review round) ------------------------------------

#[test]
fn rf1_blocked_active_with_credits_serves_before_staying_on_credits() {
    // I1 over I7: an account at 100% with credits must still Serve onto a
    // servable plan account (97% weekly, left long ago, 8 min of runway).
    let now = at(12, 0, 0);
    let mut s = snap(
        now,
        "act",
        vec![
            acct("act", 10.0, 100.0, now - Duration::seconds(20)).credits(90_000),
            acct("p97", 10.0, 97.0, now - Duration::minutes(40)).failed_read(),
        ],
    );
    s.policy.use_credits = true;
    s.memory
        .left_at
        .insert("p97".to_string(), (now - Duration::hours(3)).timestamp());
    let d = decide(&s);
    assert!(matches!(switched(&d), Some(("p97", _))), "{d:?}");
    assert_eq!(d.urgency, Urgency::Blocked, "credits polls like Blocked");
    // Nothing servable: stays on credits (never prepared away).
    s.accounts[1] = acct("p97", 10.0, 100.0, now - Duration::minutes(40))
        .wreset(Some(now + Duration::hours(1)));
    let d = decide(&s);
    assert_eq!(d.reason, Reason::OnCredits, "{d:?}");
    assert_eq!(d.urgency, Urgency::Blocked);
}

#[test]
fn rf2_flat_weekly_at_the_trigger_beats_a_slow_rated_session() {
    // Session 10→12 over 30 min (0.067/min), weekly flat at 97 → `97%w`.
    let now = at(17, 16, 0);
    let a = display(12.0, 97.0, Some(2.0 / 30.0), Some(0.0)).sreset(None);
    let b = super::runway::binding_window_at(&a, now, 95.0).unwrap();
    assert_eq!(b.window, WindowId::Weekly);
    // A no-rate window with the higher pct beats a slow rated one.
    let a = display(12.0, 90.0, Some(2.0 / 30.0), None).sreset(None);
    assert_eq!(
        super::runway::binding_window_at(&a, now, 95.0)
            .unwrap()
            .window,
        WindowId::Weekly
    );
}

#[test]
fn rf2_spec_70_96_shows_weekly_at_once_and_keeps_it() {
    let t0 = at(17, 0, 0);
    let shown = BindingShown::update(None, &display(70.0, 50.0, Some(1.0), Some(0.1)), t0, 95.0);
    assert_eq!(shown.unwrap().window, WindowId::Session);
    let a = display(70.0, 96.0, Some(1.0), Some(0.1));
    let next = BindingShown::update(shown, &a, t0 + Duration::seconds(10), 95.0);
    assert_eq!(next.unwrap().window, WindowId::Weekly, "96%w immediately");
    // Never switch away from a window at the trigger, even after the dwell
    // and with a faster session.
    let fast = display(80.0, 96.0, Some(3.0), Some(0.1));
    let later = BindingShown::update(next, &fast, t0 + Duration::minutes(10), 95.0);
    assert_eq!(later.unwrap().window, WindowId::Weekly);
}

#[test]
fn rf3_draining_active_reads_reset_pending_accounts() {
    let now = at(12, 0, 0);
    let s = snap(
        now,
        "act",
        vec![
            acct("act", 20.0, 96.0, now - Duration::seconds(20)),
            acct("pend", 0.0, 100.0, at(6, 0, 0)).wreset(Some(now - Duration::minutes(2))),
        ],
    );
    let d = decide(&s);
    assert_eq!(d.action, Action::NeedReading, "{d:?}");
    assert_eq!(d.need_reading, vec!["pend".to_string()]);
}

#[test]
fn rf4_budget_ignores_entries_from_the_future() {
    // A backward clock jump leaves timestamps "in the future": they must not
    // hold reads off.
    let t0 = at(12, 0, 0).timestamp();
    let mut b = Budget::default();
    for k in ["a", "b", "c"] {
        b.record_read(k, t0 + 600, false);
    }
    b.note_429(t0 + 600);
    assert_eq!(b.requests_last_minute(t0), 0);
    assert_eq!(b.verify_reads_last_hour(t0), 0);
    assert!(
        b.allow("a", t0, false, false, 0).is_ok(),
        "{:?}",
        b.allow("a", t0, false, false, 0)
    );
}

#[test]
fn rf5_stuck_notice_rearms_only_on_a_real_tier_change() {
    use super::shell::stuck_update;
    let now = at(12, 0, 0);
    let fleet = |w: f64, read: DateTime<Utc>, backoff: bool| {
        let mut a = acct("act", 20.0, w, read).rate(1.0, 1.0);
        a.in_backoff = backoff;
        snap(now, "act", vec![a])
    };
    let mut m = SwapMemory::default();
    let mut notices = 0;
    // Draining by drift (stuck) → a 429 streak makes it Stale under the band
    // → active_unknown → a Real reading at 95: Draining again.
    for (w, read, backoff) in [
        (89.0, now - Duration::minutes(5), false),
        (89.0, now - Duration::minutes(11), true),
        (89.0, now - Duration::minutes(12), true),
        (95.0, now - Duration::seconds(20), false),
    ] {
        let mut s = fleet(w, read, backoff);
        s.memory = m.clone();
        let d = decide(&s);
        assert!(
            matches!(d.reason, Reason::Draining | Reason::ActiveUnknown),
            "{d:?}"
        );
        if stuck_update(&mut m, &d, "act") {
            notices += 1;
        }
    }
    assert_eq!(notices, 1, "one notice for one stuck episode");
    // A Real Ready reading re-arms it.
    let mut s = snap(now, "act", vec![acct("act", 20.0, 50.0, now)]);
    s.memory = m.clone();
    assert!(!stuck_update(&mut m, &decide(&s), "act"));
    assert!(!m.stuck_notified);
}

#[test]
fn blocked_manual_pick_prepares_earlier_session_reset_after_cooldown() {
    let now = at(12, 0, 0);
    for settled in [Tier::Ready, Tier::Draining, Tier::Blocked] {
        let mut s = snap(
            now,
            "dev8",
            vec![
                acct("dev8", 100.0, 27.0, now).sreset(Some(now + Duration::hours(4))),
                acct("dev4", 100.0, 54.0, now - Duration::minutes(30))
                    .sreset(Some(now + Duration::hours(2))),
            ],
        );
        s.manual = pick("dev8", now - Duration::hours(1), settled);
        let d = decide(&s);
        assert_eq!(switched(&d), Some(("dev4", Kind::Prepare)), "{d:?}");
        s.memory.last_switch_at = Some(now.timestamp());
        let d = decide(&s);
        assert_eq!(d.reason, Reason::Cooldown, "{d:?}");
        assert!(switched(&d).is_none());
        s.memory.last_switch_at = None;
        s.accounts.swap(0, 1);
        s.active = "dev4".to_string();
        s.manual = pick("dev4", now - Duration::hours(1), settled);
        assert!(
            switched(&decide(&s)).is_none(),
            "never prepare to a later reset"
        );
    }
}
