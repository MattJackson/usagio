use super::*;
use crate::menubar::model::AcctView;
use crate::menubar::test_support::{acct, claude_section, rated, test_now};

#[test]
fn title_shows_the_binding_window_with_a_w_suffix() {
    // 17:16 dev4 29/96 at 1.8/0.47: weekly blocks first → `96%w`, not `29%`.
    let a = rated(
        acct("dev4", Some(29.0), Some(96.0), true),
        Some(1.8),
        Some(0.47),
    );
    let sec = claude_section();
    let shown = shown_binding(&sec, &a, test_now(), 95.0);
    assert_eq!(binding_title(&a, shown), "96%w");
    // 16:30 dev8 12/83 at 0.71/0.18: 124 vs 94 min → weekly `83%w`.
    let mut b = rated(
        acct("dev8-title", Some(12.0), Some(83.0), true),
        Some(0.71),
        Some(0.18),
    );
    b.key = "dev8-title".into();
    let shown = shown_binding(&sec, &b, test_now(), 95.0);
    assert_eq!(binding_title(&b, shown), "83%w");
    // A session that blocks first shows plain `47%`.
    let mut c = rated(
        acct("busy-title", Some(47.0), Some(30.0), true),
        Some(2.0),
        Some(0.1),
    );
    c.key = "busy-title".into();
    let shown = shown_binding(&sec, &c, test_now(), 95.0);
    assert_eq!(binding_title(&c, shown), "47%");
}

/// Opt this thread into `shown_binding`'s real hysteresis, starting from
/// an empty `BINDING_SHOWN` (cleared again on drop). Held tests run one at
/// a time: they share that map.
struct RealBindingHysteresis {
    _serial: std::sync::MutexGuard<'static, ()>,
}

static REAL_BINDING_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl RealBindingHysteresis {
    fn new() -> Self {
        let serial = REAL_BINDING_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        *BINDING_SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
        REAL_BINDING_HYSTERESIS.with(|c| c.set(true));
        Self { _serial: serial }
    }
}

impl Drop for RealBindingHysteresis {
    fn drop(&mut self) {
        REAL_BINDING_HYSTERESIS.with(|c| c.set(false));
        *BINDING_SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Session 40% at 1.0/min (60 min to block) against weekly 50% at `rw`.
fn binding_fixture(key: &str, rw: f64) -> AcctView {
    let mut a = rated(acct(key, Some(40.0), Some(50.0), true), Some(1.0), Some(rw));
    a.key = key.into();
    a
}

#[test]
fn the_shown_binding_window_dwells_before_switching() {
    let _real = RealBindingHysteresis::new();
    let sec = claude_section();
    let t0 = test_now();
    // Weekly 125 min out: the session binds first.
    let a = binding_fixture("dwell@x", 0.4);
    assert_eq!(binding_title(&a, shown_binding(&sec, &a, t0, 95.0)), "40%");
    // Weekly now 41.7 min out (over the 20% margin), but only a minute in:
    // the shown window persists instead of flapping.
    let b = binding_fixture("dwell@x", 1.2);
    let t1 = t0 + chrono::Duration::seconds(60);
    assert_eq!(binding_title(&b, shown_binding(&sec, &b, t1, 95.0)), "40%");
    // Past the dwell it switches.
    let t2 = t0 + chrono::Duration::seconds(301);
    assert_eq!(binding_title(&b, shown_binding(&sec, &b, t2, 95.0)), "50%w");
    // And stays switched.
    assert_eq!(
        binding_title(
            &b,
            shown_binding(&sec, &b, t2 + chrono::Duration::seconds(1), 95.0)
        ),
        "50%w"
    );
}

#[test]
fn binding_hysteresis_is_kept_per_provider_and_account() {
    let _real = RealBindingHysteresis::new();
    let claude = claude_section();
    let mut other = claude_section();
    other.provider_id = "codex";
    let t0 = test_now();
    let t1 = t0 + chrono::Duration::seconds(60);
    let a = binding_fixture("same@x", 0.4);
    assert_eq!(
        binding_title(&a, shown_binding(&claude, &a, t0, 95.0)),
        "40%"
    );
    // The same key under another provider, and another key under the same
    // provider, carry no state from the first: each shows its own fresh
    // binding window at once.
    let b = binding_fixture("same@x", 1.2);
    assert_eq!(
        binding_title(&b, shown_binding(&other, &b, t1, 95.0)),
        "50%w"
    );
    let c = binding_fixture("other@x", 1.2);
    assert_eq!(
        binding_title(&c, shown_binding(&claude, &c, t1, 95.0)),
        "50%w"
    );
    // The first still dwells on its own state.
    assert_eq!(
        binding_title(&b, shown_binding(&claude, &b, t1, 95.0)),
        "40%"
    );
}

#[test]
fn a_window_reaching_the_trigger_overrides_the_dwell() {
    let _real = RealBindingHysteresis::new();
    let sec = claude_section();
    let t0 = test_now();
    let a = binding_fixture("trigger@x", 0.4);
    assert_eq!(binding_title(&a, shown_binding(&sec, &a, t0, 95.0)), "40%");
    // Weekly crosses the trigger a minute later: shown at once.
    let mut b = binding_fixture("trigger@x", 0.4);
    b.windows[1].pct = Some(96.0);
    let t1 = t0 + chrono::Duration::seconds(60);
    assert_eq!(binding_title(&b, shown_binding(&sec, &b, t1, 95.0)), "96%w");
    // The trigger is the caller's. Session 96% at 0.01/min (400 min to
    // block) against weekly 10% at 5/min (18 min): over a 95 trigger the
    // session shows; under a 97 one the weekly binds first by rate.
    let hot = |key: &str| {
        let mut d = rated(
            acct(key, Some(96.0), Some(10.0), true),
            Some(0.01),
            Some(5.0),
        );
        d.key = key.into();
        d
    };
    let d = hot("trigger-95@x");
    assert_eq!(binding_title(&d, shown_binding(&sec, &d, t1, 95.0)), "96%");
    let e = hot("trigger-97@x");
    assert_eq!(binding_title(&e, shown_binding(&sec, &e, t1, 97.0)), "10%w");
}

#[test]
fn tooltip_names_the_binding_window_at_current_pace() {
    let a = rated(
        acct("dev4", Some(29.0), Some(96.0), true),
        Some(1.8),
        Some(0.47),
    );
    let t = binding_tooltip(&a, test_now(), 95.0);
    assert!(
        t.starts_with("dev4 — session 29%, weekly 96%w ← binds, blocks in ~9m at current pace"),
        "{t}"
    );
    // The engine's floored verdict, shown when stricter than the pace.
    let idle = rated(
        acct("idle", Some(5.0), Some(93.0), true),
        Some(0.0),
        Some(0.01),
    );
    let t = binding_tooltip(&idle, test_now(), 95.0);
    assert!(t.ends_with("auto-swap treats it as near limit"), "{t}");
}
