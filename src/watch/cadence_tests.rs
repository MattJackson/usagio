use super::*;
use crate::autoswap;
use crate::poll::trackers::reset_usage_fetch_tracker;
use crate::providers::CLAUDE_SLUG;
use crate::rows::Row;
use crate::test_support::{
    account_with_cache, at_stable_second, cu_at, fetches, read_log, row, uniq, Fx,
};
use crate::watch::cycle::{CycleOutcome, ProviderCycle};
use chrono::{Duration, Utc};

// --- next_interval (backoff + adaptive cadence) ---

const TRIGGER_FOR_TESTS: f64 = 95.0;

// All calls pass `actionable` explicitly. It only affects the at/above-trigger
// arm; below the trigger the value is irrelevant, so these pre-v0.5.17 cases
// pass `true` to preserve their original intent.
#[test]
fn next_interval_resets_to_base_when_not_limited_and_far_from_trigger() {
    // A clean cycle with everyone comfortably below the warning band
    // returns to the base cadence, even from a backed-off value.
    assert_eq!(
        next_interval(600, 60, false, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    assert_eq!(
        next_interval(60, 60, false, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // No usage data yet → also base cadence.
    assert_eq!(
        next_interval(60, 60, false, None, TRIGGER_FOR_TESTS, true),
        60
    );
}

#[test]
fn next_interval_doubles_on_rate_limit_capped() {
    // Rate-limit override wins over adaptive cadence.
    assert_eq!(
        next_interval(60, 60, true, Some(50.0), TRIGGER_FOR_TESTS, true),
        120
    );
    // Doubles the CURRENT interval, not base: one 429 at the 30s tier must
    // not jump to 2×base (the dev@ incident went 30s → 360s and ran out).
    assert_eq!(
        next_interval(30, 180, true, Some(93.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // Floored at the TIGHT interval if `current` was stale-small.
    assert_eq!(
        next_interval(1, 60, true, Some(50.0), TRIGGER_FOR_TESTS, true),
        60
    );
    // Capped at the max.
    assert_eq!(
        next_interval(
            WATCH_MAX_INTERVAL_SECS,
            60,
            true,
            Some(99.0),
            TRIGGER_FOR_TESTS,
            true
        ),
        WATCH_MAX_INTERVAL_SECS
    );
}

#[test]
fn next_interval_tightens_progressively_across_ramp_tiers() {
    // v0.7.3 replaced the binary WARNING band (30s inside 15pt of trigger)
    // with a 4-tier ramp. At trigger=95: TIGHT = [90, 95), MIDDLE = [85, 90),
    // RELAXED = [75, 85), BASE below. The v0.4.3 user miss (94% + 150s wait
    // → lock before next poll) still tightens to 30s TIGHT here.

    // TIGHT band [trigger-5, trigger): 30s regardless of `current`.
    assert_eq!(
        next_interval(180, 180, false, Some(94.9), TRIGGER_FOR_TESTS, true),
        WATCH_TIGHT_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(90.0), TRIGGER_FOR_TESTS, true),
        WATCH_TIGHT_INTERVAL_SECS,
        "trigger-5 lower edge is inside TIGHT"
    );

    // MIDDLE band [trigger-10, trigger-5): 60s. dev3 at 87% today — the
    // account whose stale 429-preserved cache silenced auto-swap in v0.7.2 —
    // lands here.
    assert_eq!(
        next_interval(180, 180, false, Some(89.9), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(87.0), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS,
        "the exact case from the v0.7.2 429 loop"
    );
    assert_eq!(
        next_interval(180, 180, false, Some(85.0), TRIGGER_FOR_TESTS, true),
        WATCH_MIDDLE_INTERVAL_SECS,
        "trigger-10 lower edge is inside MIDDLE"
    );

    // RELAXED band [trigger-20, trigger-10): 120s.
    assert_eq!(
        next_interval(180, 180, false, Some(84.9), TRIGGER_FOR_TESTS, true),
        WATCH_RELAXED_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(75.0), TRIGGER_FOR_TESTS, true),
        WATCH_RELAXED_INTERVAL_SECS,
        "trigger-20 lower edge is inside RELAXED"
    );

    // Below the ramp: BASE.
    assert_eq!(
        next_interval(180, 180, false, Some(74.9), TRIGGER_FOR_TESTS, true),
        180
    );
    assert_eq!(
        next_interval(180, 180, false, Some(20.0), TRIGGER_FOR_TESTS, true),
        180
    );

    // The whole ramp ignores `actionable` — tighten to catch the crossing
    // even when no swap target exists yet.
    assert_eq!(
        next_interval(180, 180, false, Some(90.0), TRIGGER_FOR_TESTS, false),
        WATCH_TIGHT_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(180, 180, false, Some(87.0), TRIGGER_FOR_TESTS, false),
        WATCH_MIDDLE_INTERVAL_SECS
    );
}

#[test]
fn next_interval_ramp_slides_with_custom_trigger() {
    // The whole ramp is defined as absolute percentage-point offsets from
    // the user-configurable trigger, so trigger=70 and trigger=98 both work
    // without a code change. This is why we care about the sliding property.

    // trigger=70: TIGHT [65,70), MIDDLE [60,65), RELAXED [50,60).
    assert_eq!(next_interval(180, 180, false, Some(66.0), 70.0, true), 30);
    assert_eq!(next_interval(180, 180, false, Some(62.0), 70.0, true), 60);
    assert_eq!(next_interval(180, 180, false, Some(55.0), 70.0, true), 120);
    assert_eq!(next_interval(180, 180, false, Some(45.0), 70.0, true), 180);

    // trigger=98: TIGHT [93,98), MIDDLE [88,93), RELAXED [78,88).
    assert_eq!(next_interval(180, 180, false, Some(95.0), 98.0, true), 30);
    assert_eq!(next_interval(180, 180, false, Some(90.0), 98.0, true), 60);
    assert_eq!(next_interval(180, 180, false, Some(80.0), 98.0, true), 120);
    assert_eq!(next_interval(180, 180, false, Some(60.0), 98.0, true), 180);
}

// --- per_account_fetch_floor_secs (v0.7.3 rate-limit guard) ---

#[test]
fn next_interval_tightens_to_backstop_at_or_above_trigger_when_actionable() {
    // At or above the trigger AND a swap is actionable: BACKSTOP cadence.
    // The auto-swap should already have fired; this makes sure a transient
    // failure or a soon-clearing cooldown doesn't leave us blind for a full
    // base cycle. Floor is 30s (never faster) — see WATCH_BACKSTOP_INTERVAL_SECS.
    assert_eq!(
        next_interval(150, 150, false, Some(95.0), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(150, 150, false, Some(99.9), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
    assert_eq!(
        next_interval(150, 150, false, Some(100.0), TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS
    );
}

#[test]
fn next_interval_stays_at_base_at_or_above_trigger_when_not_actionable() {
    // v0.5.17: a maxed ACTIVE account with NO eligible swap target (single
    // account, or every other account full/needs_relogin) must NOT pin the
    // 10s backstop — that only hammers the usage endpoint into HTTP 429 for a
    // week with nothing to catch. Fall back to BASE; the reset-boundary cap
    // still wakes us near the reset.
    assert_eq!(
        next_interval(150, 150, false, Some(95.0), TRIGGER_FOR_TESTS, false),
        150
    );
    assert_eq!(
        next_interval(150, 150, false, Some(100.0), TRIGGER_FOR_TESTS, false),
        150
    );
    // Rate-limit backoff still overrides regardless of actionable.
    assert_eq!(
        next_interval(150, 150, true, Some(100.0), TRIGGER_FOR_TESTS, false),
        300
    );
}

// --- cadence_max_pct (v0.5.13: cadence scoped to the active account) ---

/// Like `row()` but with a caller-chosen email so active-scoping can be tested.
fn row_for(email: &str, session: Option<f64>, weekly: Option<f64>) -> Row {
    Row {
        email: email.to_string(),
        ..row(session, weekly)
    }
}

#[test]
fn cadence_max_pct_folds_weekly_not_just_session() {
    // Regression case from the v0.5.2 addendum, preserved: the ACTIVE account
    // at session=0%, weekly=99%, trigger=95% must fold to Some(99.0) — a
    // weekly-only approach to the trigger must still tighten cadence.
    let rows = vec![row_for("active@e.com", Some(0.0), Some(99.0))];
    let m = cadence_max_pct(&rows, Some("active@e.com"));
    assert_eq!(m, Some(99.0));
    assert_eq!(
        next_interval(150, 150, false, m, TRIGGER_FOR_TESTS, true),
        WATCH_BACKSTOP_INTERVAL_SECS,
        "active session=0/weekly=99 at trigger=95 must hit BACKSTOP cadence"
    );
}

#[test]
fn cadence_max_pct_ignores_inactive_maxed_accounts() {
    // The v0.5.13 fix: a healthy ACTIVE account and an inactive account pinned
    // at 100% weekly must NOT pin the cadence at the 10s backstop. Only the
    // active account (15%) drives cadence → BASE, not BACKSTOP.
    let rows = vec![
        row_for("active@e.com", Some(15.0), Some(40.0)),
        row_for("maxed@e.com", Some(100.0), Some(100.0)),
    ];
    let m = cadence_max_pct(&rows, Some("active@e.com"));
    assert_eq!(m, Some(40.0));
    assert_eq!(
        next_interval(150, 150, false, m, TRIGGER_FOR_TESTS, true),
        150,
        "a healthy active account keeps BASE cadence despite an inactive 100% account"
    );
}

#[test]
fn cadence_max_pct_none_without_active_or_data() {
    let rows = vec![row_for("a@e.com", Some(80.0), Some(80.0))];
    // No active account set.
    assert_eq!(cadence_max_pct(&rows, None), None);
    // Active account present but no usage data yet.
    let mut no_data = row_for("a@e.com", Some(80.0), Some(80.0));
    no_data.fetched_at = None;
    assert_eq!(cadence_max_pct(&[no_data], Some("a@e.com")), None);
    // Active email doesn't match any row.
    assert_eq!(cadence_max_pct(&rows, Some("ghost@e.com")), None);
}

// --- one core, multiple providers (v0.8.8) ---

fn cycle(slug: &str, active: &str, max_pct: f64, rate_limited: bool) -> ProviderCycle {
    ProviderCycle {
        slug: slug.to_string(),
        active: Some(active.to_string()),
        swapped: None,
        rate_limited,
        fetched: 0,
        failed: 0,
        max_pct: Some(max_pct),
        actionable: true,
        urgency: None,
        next_check_at: None,
    }
}

#[test]
fn cadence_keeps_one_providers_429_from_slowing_another() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut c = Cadence::new(180);
    let out = CycleOutcome {
        providers: vec![
            cycle(CLAUDE_SLUG, "a@e.com", 93.0, false),
            cycle("codex", "x@e.com", 40.0, true),
        ],
    };
    // Claude near the trigger polls at 30s even though Codex is backing off.
    assert_eq!(c.advance(&out, 95.0), 30);
    assert_eq!(c.current_for("codex"), 360);
    assert_eq!(c.current_for(CLAUDE_SLUG), 30);
}

#[test]
fn cadence_restarts_backoff_tight_after_a_switch() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut c = Cadence::new(180);
    // Backed off on account a.
    let out = CycleOutcome {
        providers: vec![cycle(CLAUDE_SLUG, "a@e.com", 93.0, true)],
    };
    c.advance(&out, 95.0);
    c.advance(&out, 95.0);
    assert!(c.current_for(CLAUDE_SLUG) > 60);
    // Auto-swap to b this cycle: the 429 was a's; b starts from 30s.
    let mut swapped = cycle(CLAUDE_SLUG, "b@e.com", 20.0, true);
    swapped.swapped = Some(("a@e.com".into(), "b@e.com".into()));
    c.advance(
        &CycleOutcome {
            providers: vec![swapped],
        },
        95.0,
    );
    assert_eq!(c.current_for(CLAUDE_SLUG), WATCH_TIGHT_INTERVAL_SECS);
    // A first 429 on b only doubles that: 60s, not 360s.
    c.advance(
        &CycleOutcome {
            providers: vec![cycle(CLAUDE_SLUG, "b@e.com", 93.0, true)],
        },
        95.0,
    );
    assert_eq!(c.current_for(CLAUDE_SLUG), 60);
}

// --- mutation-hardening: cadence log, provider swap cycle, watch cycle ---

#[test]
fn cadence_logs_only_when_the_interval_actually_tightens() {
    let _g = crate::store::ScopedConfigDir::new();
    let count = || read_log().matches("event=cadence").count();
    let mut c = Cadence::new(180);
    let step = |c: &mut Cadence, pct: f64| {
        c.advance(
            &CycleOutcome {
                providers: vec![cycle(CLAUDE_SLUG, "a@e.com", pct, false)],
            },
            95.0,
        )
    };
    // 180s -> 30s as the account closes on the trigger: logged.
    assert_eq!(step(&mut c, 93.0), 30);
    assert_eq!(count(), 1);
    assert!(read_log().contains("event=cadence provider=claude prev=180s new=30s"));
    // Unchanged: nothing new to say.
    assert_eq!(step(&mut c, 93.0), 30);
    assert_eq!(count(), 1);
    // Relaxing back to the base interval is not a tightening.
    assert_eq!(step(&mut c, 10.0), 180);
    assert_eq!(count(), 1);
    // 180s -> 60s: tightening again.
    assert_eq!(step(&mut c, 85.0), 60);
    assert_eq!(count(), 2);
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn claude_active_tier_boundary_is_exact() {
    let e = uniq("boundary-active");
    let base = WATCH_INTERVAL_SECS as i64;
    let run = |age: i64| {
        at_stable_second(|t0| {
            fetches(Fx::new(
                vec![account_with_cache(&e, Some(cu_at(10.0, 10.0, t0 - age)))],
                Some(&e),
            ))
        })
    };
    assert_eq!(run(base), 1, "due exactly one interval after the reading");
    assert_eq!(run(base - 1), 0);
    reset_usage_fetch_tracker(&e);
}

// --- auto-swap v2 (0.12.2): counterparts of the flipped v1 tests (§8.4) ---

#[test]
fn next_interval_v2_holds_the_active_at_60s_in_the_tight_band() {
    // Flip of the `next_interval_*` 30 s TIGHT expectations: under v2 the
    // active poll floor in band is 60 s (it ships with the blind projection).
    use autoswap::Urgency;
    let t = TRIGGER_FOR_TESTS;
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Band, Some(94.9), t, true),
        60
    );
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Band, Some(90.0), t, false),
        60
    );
    // Draining/Blocked: TIGHT only with something to catch (actionable).
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Draining, Some(97.0), t, true),
        60
    );
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Draining, Some(97.0), t, false),
        180
    );
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Blocked, Some(100.0), t, false),
        180
    );
    // Calm: the ramp, with TIGHT at 60 s.
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Calm, Some(87.0), t, false),
        60
    );
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Calm, Some(80.0), t, false),
        120
    );
    assert_eq!(
        next_interval_v2(180, 180, false, Urgency::Calm, Some(40.0), t, false),
        180
    );
    // A 429 in band or Draining backs off at most to 120 s…
    assert_eq!(
        next_interval_v2(60, 180, true, Urgency::Band, Some(93.0), t, true),
        120
    );
    assert_eq!(
        next_interval_v2(120, 180, true, Urgency::Draining, Some(97.0), t, true),
        120
    );
    // …outside the band the doubling applies, up to the max.
    assert_eq!(
        next_interval_v2(180, 180, true, Urgency::Calm, Some(40.0), t, false),
        360
    );
    assert_eq!(
        next_interval_v2(
            WATCH_MAX_INTERVAL_SECS,
            180,
            true,
            Urgency::Calm,
            Some(40.0),
            t,
            false
        ),
        WATCH_MAX_INTERVAL_SECS
    );
}

#[test]
fn cadence_wakes_at_the_v2_next_check_time() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut c = Cadence::new(180);
    let mut p = cycle(CLAUDE_SLUG, "a@e.com", 40.0, false);
    p.urgency = Some(autoswap::Urgency::Calm);
    p.next_check_at = Some(Utc::now() + Duration::seconds(45));
    let secs = c.advance(&CycleOutcome { providers: vec![p] }, 95.0);
    assert!((40..=45).contains(&secs), "{secs}");
    // Never spins faster than the check floor.
    let mut p = cycle(CLAUDE_SLUG, "a@e.com", 40.0, false);
    p.urgency = Some(autoswap::Urgency::Calm);
    p.next_check_at = Some(Utc::now() + Duration::seconds(1));
    assert_eq!(
        c.advance(&CycleOutcome { providers: vec![p] }, 95.0),
        NEXT_CHECK_MIN_SECS
    );
}
