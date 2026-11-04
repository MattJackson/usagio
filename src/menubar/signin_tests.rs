use super::*;
use crate::menubar::actions::run_ui_tasks;
use crate::menubar::test_support::{codex_state_acct, restore_test_acct};
use crate::providers::CLAUDE_SLUG;
use crate::store::State;
use std::time::Duration;

#[test]
fn renew_login_store_only_uses_claude_store_for_claude() {
    let _g = crate::store::ScopedConfigDir::new();
    let mut st = State::default();
    let mut a = restore_test_acct("me@x.com");
    a.login_store = Some("s-abc".into());
    st.accounts.push(a);
    assert_eq!(renew_login_store(&st, CLAUDE_SLUG, "me@x.com"), "s-abc");
    assert_eq!(renew_login_store(&st, "codex", "me@x.com"), "me@x.com");
}

#[test]
fn a_finished_browser_login_is_not_superseded_and_leaves_the_registry() {
    let done = std::sync::Arc::new(crate::login::Progress::default());
    track_login("t-login-done", done.clone());
    done.mark_finished();
    assert!(
        supersede_previous_login("t-login-done").is_none(),
        "a finished flow holds no listener: the next sign-in may use the window"
    );
    assert!(supersede_previous_login("t-login-done").is_none());
}

#[test]
fn tracking_a_new_browser_login_supersedes_the_one_it_replaces() {
    let old = std::sync::Arc::new(crate::login::Progress::default());
    let new = std::sync::Arc::new(crate::login::Progress::default());
    track_login("t-login-replace", old.clone());
    track_login("t-login-replace", new.clone());
    assert!(
        old.is_superseded(),
        "a replaced flow must not keep its listener untracked"
    );
    assert!(!new.is_cancelled());
    let prev = supersede_previous_login("t-login-replace").expect("tracked");
    assert!(std::sync::Arc::ptr_eq(&prev, &new));
}

/// Serializes the tests that drain the shared UI task queue.
static UI_TASKS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Drain the UI queue on this (the "UI") thread until `flag` is set.
fn drain_until(flag: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        run_ui_tasks();
        std::thread::sleep(Duration::from_millis(10));
    }
    flag.load(Ordering::SeqCst)
}

#[test]
fn a_newer_sign_in_click_wins_over_a_worker_still_waiting() {
    let _home = crate::store::ScopedConfigDir::new();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let _ui = UI_TASKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Click 2 supersedes A and waits on a worker; click 3 finds nothing
    // tracked and starts inline. The waiting worker must not start B.
    let a = Arc::new(crate::login::Progress::default());
    track_login("t-login-gen", a.clone());
    let started_b = Arc::new(AtomicBool::new(false));
    let b = Arc::clone(&started_b);
    begin_login_after_superseding("t-login-gen", move || b.store(true, Ordering::SeqCst));
    let started_c = Arc::new(AtomicBool::new(false));
    let c = Arc::clone(&started_c);
    begin_login_after_superseding("t-login-gen", move || c.store(true, Ordering::SeqCst));
    assert!(
        started_c.load(Ordering::SeqCst),
        "nothing to supersede: runs inline"
    );
    a.mark_finished();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        run_ui_tasks();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !started_b.load(Ordering::SeqCst),
        "an older click's worker started after a newer click"
    );
    // Control: with no newer click the worker does start its flow.
    let d = Arc::new(crate::login::Progress::default());
    track_login("t-login-gen", d.clone());
    let started_e = Arc::new(AtomicBool::new(false));
    let e = Arc::clone(&started_e);
    begin_login_after_superseding("t-login-gen", move || e.store(true, Ordering::SeqCst));
    d.mark_finished();
    assert!(
        drain_until(&started_e),
        "the only waiting click never started"
    );
    supersede_previous_login("t-login-gen");
}

#[test]
fn a_superseding_sign_in_starts_on_the_ui_thread_after_the_port_is_free() {
    let _home = crate::store::ScopedConfigDir::new();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let _ui = UI_TASKS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = Arc::new(crate::login::Progress::default());
    track_login("t-login-ui", old.clone());
    let started = Arc::new(AtomicBool::new(false));
    let ran_on = Arc::new(Mutex::new(None));
    let (s, r, o) = (Arc::clone(&started), Arc::clone(&ran_on), Arc::clone(&old));
    begin_login_after_superseding("t-login-ui", move || {
        // Only once the old flow released its listener.
        assert!(
            o.is_finished(),
            "started before the old listener was released"
        );
        *r.lock().unwrap() = Some(std::thread::current().id());
        s.store(true, Ordering::SeqCst);
    });
    assert!(old.is_superseded(), "the old flow is stopped at once");
    assert!(!started.load(Ordering::SeqCst), "the UI thread never waits");
    std::thread::sleep(Duration::from_millis(100));
    run_ui_tasks();
    assert!(!started.load(Ordering::SeqCst), "old listener still held");
    old.mark_finished();
    assert!(drain_until(&started), "the new sign-in never started");
    assert_eq!(
        *ran_on.lock().unwrap(),
        Some(std::thread::current().id()),
        "the new flow must start on the UI thread (it may open a window)"
    );
}

#[test]
fn a_sign_in_shown_in_the_window_is_tracked_and_superseded() {
    let _home = crate::store::ScopedConfigDir::new();
    let started = crate::login::Started {
        url: "https://auth.example.com/".into(),
        progress: std::sync::Arc::new(crate::login::Progress::default()),
    };
    let mut browser_opened = false;
    present_sign_in(
        "t-login-window",
        crate::login::store_id("t-login-window", "s"),
        &started,
        Some(|| Ok(())),
        || {
            browser_opened = true;
            Ok(())
        },
    );
    assert!(!browser_opened, "the window opened: no browser");
    let prev = supersede_previous_login("t-login-window")
        .expect("a window flow must be tracked so a newer sign-in can stop it");
    assert!(std::sync::Arc::ptr_eq(&prev, &started.progress));
    assert!(started.progress.is_superseded());
}

#[test]
fn a_sign_in_falls_back_to_the_browser_and_is_tracked() {
    let _home = crate::store::ScopedConfigDir::new();
    let started = crate::login::Started {
        url: "https://auth.example.com/".into(),
        progress: std::sync::Arc::new(crate::login::Progress::default()),
    };
    let mut browser_opened = false;
    present_sign_in(
        "t-login-fallback",
        crate::login::store_id("t-login-fallback", "s"),
        &started,
        Some(|| Err(anyhow::anyhow!("no window here"))),
        || {
            browser_opened = true;
            Ok(())
        },
    );
    assert!(browser_opened);
    assert!(supersede_previous_login("t-login-fallback").is_some());
}

#[test]
fn every_saved_account_keeps_the_sign_in_store_its_renew_opens() {
    let mut st = State::default();
    let mut added = restore_test_acct("added@x.io");
    added.login_store = Some("new:abc".into());
    st.upsert(added);
    st.upsert(restore_test_acct("plain@x.io"));
    st.providers
        .entry("codex".into())
        .or_default()
        .accounts
        .push(codex_state_acct("c@x.io"));
    let keep = sign_in_stores_to_keep(&st);
    assert!(keep.contains(&crate::login::store_id(CLAUDE_SLUG, "new:abc")));
    assert!(
        !keep.contains(&crate::login::store_id(CLAUDE_SLUG, "added@x.io")),
        "its renew opens the add's store, not one keyed by email"
    );
    assert!(keep.contains(&crate::login::store_id(CLAUDE_SLUG, "plain@x.io")));
    assert!(keep.contains(&crate::login::store_id("codex", "c@x.io")));
    assert!(!keep.contains(&crate::login::store_id("codex", "removed@x.io")));
}

#[test]
fn a_running_flows_store_is_kept_until_the_flow_ends() {
    let st = State::default();
    let id = crate::login::store_id("t-flow-store", "new:running");
    let progress = std::sync::Arc::new(crate::login::Progress::default());
    track_flow_store(id, progress.clone());
    assert!(
        sign_in_stores_to_keep(&st).contains(&id),
        "a closed window's flow may still save its account"
    );
    progress.mark_finished();
    assert!(
        !sign_in_stores_to_keep(&st).contains(&id),
        "a failed add's store is no longer wanted"
    );
}

#[test]
fn an_unreadable_state_never_sweeps_the_stores() {
    let _home = crate::store::ScopedConfigDir::new();
    let path = crate::store::state_json_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ not json").unwrap();
    let mut swept = false;
    sweep_sign_in_stores_with(|_| swept = true);
    assert!(
        !swept,
        "an unreadable state would read as no accounts: keep all"
    );
    let mut st = State::default();
    st.upsert(restore_test_acct("kept@x.io"));
    std::fs::remove_file(&path).unwrap();
    st.save().unwrap();
    let mut kept = None;
    sweep_sign_in_stores_with(|keep| kept = Some(keep));
    assert!(kept
        .expect("a readable state sweeps")
        .contains(&crate::login::store_id(CLAUDE_SLUG, "kept@x.io")));
}

#[test]
fn a_new_browser_login_cancels_the_previous_one_for_that_provider() {
    let old = std::sync::Arc::new(crate::login::Progress::default());
    let other = std::sync::Arc::new(crate::login::Progress::default());
    track_login("t-login-a", old.clone());
    track_login("t-login-b", other.clone());
    let prev = supersede_previous_login("t-login-a").expect("tracked");
    assert!(std::sync::Arc::ptr_eq(&prev, &old));
    assert!(
        old.is_cancelled() && old.is_superseded(),
        "previous flow ends quietly"
    );
    assert!(!other.is_cancelled(), "other providers are untouched");
}
