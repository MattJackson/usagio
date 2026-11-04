use super::*;
use crate::cli::capture::capture_current_generic;
use crate::poll::claude::{refresh_usage_cache, MAX_LOCKED_STALENESS_SECS};
use crate::poll::plan::SUBSCRIPTION_RECHECK_SECS;
use crate::poll::providers::refresh_provider_usage_caches;
use crate::providers;
use crate::providers::CLAUDE_SLUG;
use crate::rows::{row_from_account, row_from_provider_account};
use crate::store::{CachedUsage, State};
use crate::switching::claude::apply_account;
use crate::switching::provider::switch_to_provider_account;
use crate::test_support::{
    account_with_cache, acct_with_cache, at_stable_second, claude_account_with_identity,
    codex_account, codex_auth_json, cu, cu_at, escalation_test_email, fetches, hit_count,
    seed_claude_state, spawn_mock_usage_server, uniq, usage_at, with_claude_home, Fx,
    POLL_ACTIVE_LOCK,
};
use crate::watch::cadence::WATCH_INTERVAL_SECS;
use chrono::{Duration, Utc};

// --- v0.8.5: active account gets the request budget; inactive backs off alone ---

#[test]
fn inactive_fetch_backoff_doubles_from_floor_and_caps() {
    assert_eq!(inactive_fetch_backoff_secs(1), INACTIVE_FETCH_FLOOR_SECS);
    assert_eq!(
        inactive_fetch_backoff_secs(2),
        INACTIVE_FETCH_FLOOR_SECS * 2
    );
    assert_eq!(
        inactive_fetch_backoff_secs(3),
        INACTIVE_FETCH_FLOOR_SECS * 4
    );
    assert_eq!(
        inactive_fetch_backoff_secs(4),
        INACTIVE_FETCH_MAX_BACKOFF_SECS
    );
    assert_eq!(
        inactive_fetch_backoff_secs(u32::MAX),
        INACTIVE_FETCH_MAX_BACKOFF_SECS
    );
}

#[test]
fn inactive_fetch_failure_sets_backoff_and_success_clears_it() {
    let email = escalation_test_email("inactive-backoff");
    let now_ts = 1_000_000;
    record_inactive_fetch_failure(&email, now_ts);
    record_inactive_fetch_failure(&email, now_ts);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 2);
    assert_eq!(
        t.backoff_until_ts,
        Some(now_ts + inactive_fetch_backoff_secs(2) as i64)
    );
    record_usage_fetch_success(&email);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 0);
    assert_eq!(t.backoff_until_ts, None);
    reset_usage_fetch_tracker(&email);
}

/// Mock usage endpoint that answers every request with HTTP 429 and counts hits.
fn spawn_mock_429_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock 429 server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                    break;
                }
            }
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

/// The v0.8.5 incident: a 429 on an INACTIVE account set the global
/// `rate_limited` flag, pushing the loop to 1200s so the active account ran
/// out between polls. Now it must not slow the loop, and must back that
/// account off on its own so the next cycle doesn't fetch it again.
#[test]
fn inactive_429_does_not_rate_limit_loop_and_backs_off_that_account() {
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    use crate::store::{Account, ScopedConfigDir};

    let _g = ScopedConfigDir::new();
    let (base_url, hits) = spawn_mock_429_server();
    crate::endpoints::set_test_override(
        crate::endpoints::Endpoint::ClaudeUsage,
        Some(&format!("{base_url}/api/oauth/usage")),
    );

    let email = escalation_test_email("inactive-429");
    let fresh = Utc::now().timestamp_millis() + 3_600_000;
    let mut inactive = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"inactive-at","refreshToken":"inactive-rt","expiresAt":{fresh}}}}}"#
    ))
    .unwrap();
    inactive.email = Some(email.clone());
    let mut seed = State::default();
    seed.accounts.push(inactive);
    seed.active = None;
    seed.save().unwrap();

    let first = refresh_usage_cache(false);
    let second = refresh_usage_cache(false);
    crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeUsage, None);

    assert!(
        !first.rate_limited,
        "an inactive 429 must not slow the global loop"
    );
    assert!(!second.rate_limited);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the backed-off inactive account must not be re-fetched next cycle"
    );
    assert!(get_usage_fetch_tracker(&email).backoff_until_ts.is_some());
    reset_usage_fetch_tracker(&email);
}

#[test]
fn fetch_tracker_keys_are_namespaced_per_provider() {
    assert_eq!(fetch_tracker_key(CLAUDE_SLUG, "a@e.com"), "a@e.com");
    assert_eq!(fetch_tracker_key("codex", "a@e.com"), "codex/a@e.com");
}

#[test]
fn subscription_check_is_claimed_once_per_recheck_window() {
    let email = escalation_test_email("claim");
    let t0 = 1_000_000;
    assert!(claim_subscription_check(&email, t0, false), "first is due");
    assert!(!claim_subscription_check(&email, t0 + 1, false));
    let edge = SUBSCRIPTION_RECHECK_SECS as i64;
    assert!(!claim_subscription_check(&email, t0 + edge - 1, false));
    // A forced check is always due, even right after one.
    assert!(claim_subscription_check(&email, t0 + 2, true));
    // ...and restarts the window from the forced check.
    assert!(!claim_subscription_check(&email, t0 + 2 + edge - 1, false));
    assert!(claim_subscription_check(&email, t0 + 2 + edge, false));
    reset_usage_fetch_tracker(&email);
}

#[test]
fn clearing_inactive_backoff_keeps_the_429_count() {
    let email = escalation_test_email("clear-backoff");
    record_usage_fetch_429(&email);
    record_inactive_fetch_failure(&email, 1_000);
    record_inactive_fetch_failure(&email, 1_000);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 2);
    assert!(t.backoff_until_ts.is_some());
    clear_inactive_fetch_backoff(&email);
    let t = get_usage_fetch_tracker(&email);
    assert_eq!(t.consecutive_failures, 0);
    assert_eq!(t.backoff_until_ts, None);
    assert_eq!(t.consecutive_429s, 1);
    // An account never seen is a no-op, not an entry.
    clear_inactive_fetch_backoff(&escalation_test_email("never-seen"));
    reset_usage_fetch_tracker(&email);
}

#[test]
fn non_429_error_only_resets_an_existing_counter() {
    let email = escalation_test_email("non429");
    record_usage_fetch_429(&email);
    record_usage_fetch_429(&email);
    assert_eq!(get_usage_fetch_tracker(&email).consecutive_429s, 2);
    record_usage_fetch_non_429(&email);
    assert_eq!(get_usage_fetch_tracker(&email).consecutive_429s, 0);
    reset_usage_fetch_tracker(&email);
}

#[test]
fn note_polled_active_reports_only_a_change_of_active_account() {
    let _l = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Other (unlocked) tests may poll concurrently; accept the first clean pass.
    for _ in 0..50 {
        note_polled_active(Some("a@e.com"));
        let same = note_polled_active(Some("a@e.com"));
        let switched = note_polled_active(Some("b@e.com"));
        let same_again = note_polled_active(Some("b@e.com"));
        let cleared = note_polled_active(None);
        let cleared_again = note_polled_active(None);
        if !same && switched && !same_again && cleared && !cleared_again {
            return;
        }
    }
    panic!("note_polled_active never behaved like a change detector");
}

#[test]
fn switching_codex_resets_the_tracker_of_the_account_left_behind_only() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        for (who, at, rt) in [("a", "at-a", "rt-a"), ("b", "at-b", "rt-b")] {
            let blob = codex_auth_json(&format!("{who}@example.com"), 4_000_000_000, at, rt);
            std::fs::write(dir.path().join("auth.json"), blob).unwrap();
            capture_current_generic("codex").unwrap();
        }
        let ka = fetch_tracker_key("codex", "a@example.com");
        let kb = fetch_tracker_key("codex", "b@example.com");
        record_usage_fetch_429(&ka);
        record_usage_fetch_429(&kb);
        // b is active; moving to a forgets b's 429s, not a's.
        switch_to_provider_account("codex", "a@example.com", false).unwrap();
        assert_eq!(get_usage_fetch_tracker(&kb).consecutive_429s, 0);
        assert_eq!(get_usage_fetch_tracker(&ka).consecutive_429s, 1);
        // Re-selecting the active account is not a departure.
        switch_to_provider_account("codex", "a@example.com", false).unwrap();
        assert_eq!(get_usage_fetch_tracker(&ka).consecutive_429s, 1);
        reset_usage_fetch_tracker(&ka);
        reset_usage_fetch_tracker(&kb);
    });
}

/// A session-locked reading (100% until a reset three hours out), `age` old.
fn cu_locked(age: i64) -> CachedUsage {
    let mut c = cu(100.0, 10.0, age);
    c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
    c
}

#[test]
fn skipping_is_decided_per_poll_not_remembered_by_the_subscription_throttle() {
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let email = uniq("repoll");
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(500, String::new());
        crate::endpoints::set_test_override(
            crate::endpoints::Endpoint::ClaudeUsage,
            Some(&format!("{url}/api/oauth/usage")),
        );
        seed_claude_state(
            vec![account_with_cache(&email, Some(cu(10.0, 10.0, 2000)))],
            None,
        );
        note_polled_active(None);
        refresh_usage_cache(false);
        assert_eq!(hit_count(&hits), 1);
        // The failure put the account in backoff; forget that and poll again.
        // Nothing else stands in the way of fetching it again.
        clear_inactive_fetch_backoff(&email);
        refresh_usage_cache(false);
        crate::endpoints::set_test_override(crate::endpoints::Endpoint::ClaudeUsage, None);
        assert_eq!(hit_count(&hits), 2);
    });
    reset_usage_fetch_tracker(&email);
}

#[test]
fn locked_inactive_accounts_are_skipped_until_stale_or_forced() {
    let e = uniq("locked");
    let one = |cache: CachedUsage, force: bool| {
        let fx = Fx::new(vec![account_with_cache(&e, Some(cache))], None);
        fetches(if force { fx.force() } else { fx })
    };
    let hour = 3600;
    // Locked until a reset hours away, read 5h ago: nothing to learn.
    assert_eq!(one(cu_locked(5 * hour), false), 0);
    // The same account read 13h ago may have been reset early: look again.
    assert_eq!(one(cu_locked(13 * hour), false), 1);
    // A manual refresh always looks.
    assert_eq!(one(cu_locked(5 * hour), true), 1);
    // Not locked, merely old: polled.
    assert_eq!(one(cu(10.0, 10.0, 5 * hour), false), 1);
    reset_usage_fetch_tracker(&e);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn the_active_account_is_polled_even_when_it_looks_locked() {
    // Flipped for auto-swap v2 (design §5.7, R3-B2): an lb-blocked active
    // account is NOT polled — its full reading is a lower bound valid until
    // the window resets. It is read again once the window is reset-pending,
    // at the 12 h staleness cap, or on a forced refresh.
    let e = uniq("locked-active");
    let one = |cache: CachedUsage, force: bool| {
        let fx = Fx::new(vec![account_with_cache(&e, Some(cache))], Some(&e));
        fetches(if force { fx.force() } else { fx })
    };
    assert_eq!(one(cu_locked(5 * 3600), false), 0, "lb-blocked: not polled");
    assert_eq!(one(cu_locked(13 * 3600), false), 1, "past the 12 h cap");
    assert_eq!(
        one(cu_locked(5 * 3600), true),
        1,
        "a forced refresh reads it"
    );
    let mut pending = cu_locked(5 * 3600);
    pending.session_reset = Some((Utc::now() - Duration::seconds(30)).to_rfc3339());
    assert_eq!(one(pending, false), 1, "reset-pending: read at once");
    reset_usage_fetch_tracker(&e);
}

#[test]
fn inactive_accounts_wait_out_their_floor_and_their_failure_backoff() {
    let e = uniq("floor");
    let one =
        |cache: CachedUsage| fetches(Fx::new(vec![account_with_cache(&e, Some(cache))], None));
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    assert_eq!(one(cu(10.0, 10.0, floor - 300)), 0, "read 5 minutes ago");
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1, "past the floor");
    // A clock that ran backwards (reading from the future) is not "fresh".
    assert_eq!(one(cu(10.0, 10.0, -100)), 1);
    // A reset crossed since the reading: recheck at once.
    let mut crossed = cu(10.0, 10.0, 300);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
    assert_eq!(one(crossed), 1);
    // Failure backoff holds a stale account off...
    record_inactive_fetch_failure(&e, Utc::now().timestamp());
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 0);
    // ...unless a reset crossing makes its reading untrustworthy.
    let mut crossed = cu(10.0, 10.0, floor + 100);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
    assert_eq!(one(crossed), 1);
    reset_usage_fetch_tracker(&e);
    // Once the backoff has lapsed it is polled again.
    record_inactive_fetch_failure(&e, Utc::now().timestamp() - 10_000);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    reset_usage_fetch_tracker(&e);
}

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
fn the_active_account_follows_its_own_tier_not_the_inactive_floor() {
    let e = uniq("active-floor");
    let one = |cache: CachedUsage, fx: fn(Fx) -> Fx| {
        fetches(fx(Fx::new(
            vec![account_with_cache(&e, Some(cache))],
            Some(&e),
        )))
    };
    let same = |fx: Fx| fx;
    // 93% of a 95% trigger polls every 30s: a 5-minute-old reading is stale
    // even though an inactive account would still be inside its floor.
    assert_eq!(one(cu(93.0, 10.0, 300), same), 1);
    // A comfortable account polls every 3 minutes.
    assert_eq!(one(cu(10.0, 10.0, 10), same), 0);
    assert_eq!(one(cu(10.0, 10.0, 1000), same), 1);
    // Reading from the future is not fresh.
    assert_eq!(one(cu(10.0, 10.0, -100), same), 1);
    // Manual refresh and the first poll after a switch bypass the floor.
    assert_eq!(one(cu(10.0, 10.0, 10), Fx::force), 1);
    assert_eq!(one(cu(10.0, 10.0, 10), Fx::just_switched), 1);
    // So does a crossed reset.
    let mut crossed = cu(10.0, 10.0, 10);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(5)).to_rfc3339());
    crossed.fetched_at = Utc::now().timestamp() - 60;
    assert_eq!(one(crossed, same), 1);
    reset_usage_fetch_tracker(&e);
}

/// `(token-endpoint, usage-endpoint)` requests made by one
/// `refresh_provider_usage_caches` pass over Codex `accounts`. Separate mock
/// servers, so a refresh POST and a usage GET can never be mistaken for each other.
fn codex_gate_split(
    accounts: Vec<crate::store::ProviderAccount>,
    active: Option<&str>,
    force: bool,
) -> (usize, usize) {
    let _cfg = crate::store::ScopedConfigDir::new();
    let (token_url, token_hits) = spawn_mock_usage_server(500, String::new());
    let (usage_url, usage_hits) = spawn_mock_usage_server(500, String::new());
    let mut st = State::default();
    for a in accounts {
        st.upsert_provider_account("codex", a);
    }
    st.provider_accounts_mut("codex").active = active.map(str::to_string);
    st.save().unwrap();
    crate::env_lock::scoped_env_var(
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        Some(&format!("{token_url}/oauth/token")),
        || {
            crate::endpoints::set_test_override(
                crate::endpoints::Endpoint::CodexUsage,
                Some(&format!("{usage_url}/usage")),
            );
            refresh_provider_usage_caches(force, 95.0);
            crate::endpoints::set_test_override(crate::endpoints::Endpoint::CodexUsage, None);
        },
    );
    (hit_count(&token_hits), hit_count(&usage_hits))
}

/// Total requests (token + usage) made by one pass: either proves the account
/// passed its scheduling gates.
fn codex_gate(
    accounts: Vec<crate::store::ProviderAccount>,
    active: Option<&str>,
    force: bool,
) -> usize {
    let (token, usage) = codex_gate_split(accounts, active, force);
    token + usage
}

fn codex_one(key: &str, cache: CachedUsage, active: bool, force: bool) -> usize {
    codex_gate(
        vec![codex_account(key, Some(cache))],
        active.then_some(key),
        force,
    )
}

#[test]
fn codex_account_past_every_skip_rule_gets_its_token_refreshed() {
    let k = uniq("codex-basic");
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 2000), false, false), 1);
    // The one request is the token refresh, not a usage call with a stale token;
    // the active account reaches usage directly and never spends its grant.
    let acct = || codex_account(&k, Some(cu(10.0, 10.0, 2000)));
    assert_eq!(codex_gate_split(vec![acct()], None, false), (1, 0));
    assert_eq!(codex_gate_split(vec![acct()], Some(&k), false), (0, 1));
    // A token inside the refresh skew counts as due, an unexpired one beyond it
    // would not (that case needs the real endpoint, so only the edge is pinned).
    let mut soon = codex_account(&k, Some(cu(10.0, 10.0, 2000)));
    soon.expires_at = Utc::now().timestamp() + 100;
    assert_eq!(codex_gate(vec![soon], None, false), 1);
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
}

#[test]
fn codex_dead_logins_are_never_polled() {
    let k = uniq("codex-dead");
    let mut flagged = codex_account(&k, Some(cu(10.0, 10.0, 2000)));
    flagged.needs_relogin = true;
    assert_eq!(codex_gate(vec![flagged], Some(&k), true), 0);
}

#[test]
fn codex_lapsed_accounts_are_rechecked_once_per_window_or_when_forced() {
    let k = uniq("codex-lapsed");
    let lapsed = |key: &str| {
        let mut a = codex_account(key, Some(cu(10.0, 10.0, 2000)));
        a.no_subscription = true;
        a
    };
    let tracker = fetch_tracker_key("codex", &k);
    // First look is due...
    assert_eq!(codex_gate(vec![lapsed(&k)], None, false), 1);
    // ...and then it is throttled for the recheck window.
    assert_eq!(codex_gate(vec![lapsed(&k)], None, false), 0);
    // A manual refresh is always due.
    assert_eq!(codex_gate(vec![lapsed(&k)], None, true), 1);
    reset_usage_fetch_tracker(&tracker);
}

#[test]
fn codex_locked_accounts_wait_for_their_reset_unless_stale_or_forced() {
    let k = uniq("codex-locked");
    let hour = 3600;
    assert_eq!(codex_one(&k, cu_locked(5 * hour), false, false), 0);
    assert_eq!(codex_one(&k, cu_locked(13 * hour), false, false), 1);
    assert_eq!(codex_one(&k, cu_locked(5 * hour), false, true), 1);
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 5 * hour), false, false), 1);
}

#[test]
fn codex_inactive_accounts_wait_out_their_floor_and_failure_backoff() {
    let k = uniq("codex-floor");
    let tracker = fetch_tracker_key("codex", &k);
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let one = |c: CachedUsage| codex_one(&k, c, false, false);
    assert_eq!(one(cu(10.0, 10.0, floor - 300)), 0);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    assert_eq!(one(cu(10.0, 10.0, -100)), 1);
    let crossed = |age: i64| {
        let mut c = cu(10.0, 10.0, age);
        c.weekly_reset = Some((Utc::now() - Duration::seconds(100)).to_rfc3339());
        c
    };
    assert_eq!(one(crossed(300)), 1);
    record_inactive_fetch_failure(&tracker, Utc::now().timestamp());
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 0);
    assert_eq!(one(crossed(floor + 100)), 1);
    reset_usage_fetch_tracker(&tracker);
    record_inactive_fetch_failure(&tracker, Utc::now().timestamp() - 10_000);
    assert_eq!(one(cu(10.0, 10.0, floor + 100)), 1);
    reset_usage_fetch_tracker(&tracker);
}

#[test]
fn codex_active_account_follows_its_tier() {
    let k = uniq("codex-active");
    let one = |c: CachedUsage, force: bool| {
        reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
        // Not a freshly switched account: the v2 skip applies.
        note_polled_provider_active("codex", Some(&k));
        codex_one(&k, c, true, force)
    };
    assert_eq!(one(cu(93.0, 10.0, 300), false), 1, "93% polls every 30s");
    assert_eq!(one(cu(10.0, 10.0, 10), false), 0);
    assert_eq!(one(cu(10.0, 10.0, 1000), false), 1);
    assert_eq!(one(cu(10.0, 10.0, -100), false), 1);
    assert_eq!(one(cu(10.0, 10.0, 10), true), 1);
    let mut crossed = cu(10.0, 10.0, 60);
    crossed.weekly_reset = Some((Utc::now() - Duration::seconds(5)).to_rfc3339());
    assert_eq!(one(crossed, false), 1);
}

#[test]
fn claude_inactive_boundaries_are_exact() {
    let e = uniq("boundary");
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let run = |cache: &dyn Fn(i64) -> CachedUsage, prep: &dyn Fn(i64)| {
        at_stable_second(|t0| {
            reset_usage_fetch_tracker(&e);
            prep(t0);
            fetches(Fx::new(vec![account_with_cache(&e, Some(cache(t0)))], None))
        })
    };
    let nothing = |_: i64| {};
    // A reading exactly one floor old is due again.
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - floor), &nothing), 1);
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - floor + 1), &nothing), 0);
    // A backoff that ends exactly now is over.
    let until_now = |t0: i64| {
        record_inactive_fetch_failure(&e, t0 - inactive_fetch_backoff_secs(1) as i64);
    };
    assert_eq!(run(&|t| cu_at(10.0, 10.0, t - 2000), &until_now), 1);
    // A locked reading exactly 12h old is still trusted; one second older is not.
    let locked = |age: i64| {
        move |t: i64| {
            let mut c = cu_at(100.0, 10.0, t - age);
            c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
            c
        }
    };
    assert_eq!(run(&locked(MAX_LOCKED_STALENESS_SECS), &nothing), 0);
    assert_eq!(run(&locked(MAX_LOCKED_STALENESS_SECS + 1), &nothing), 1);
    reset_usage_fetch_tracker(&e);
}

#[test]
fn codex_boundaries_are_exact() {
    let k = uniq("codex-boundary");
    let tracker = fetch_tracker_key("codex", &k);
    let floor = INACTIVE_FETCH_FLOOR_SECS as i64;
    let inactive = |cache: &dyn Fn(i64) -> CachedUsage, prep: &dyn Fn(i64)| {
        at_stable_second(|t0| {
            reset_usage_fetch_tracker(&tracker);
            prep(t0);
            codex_one(&k, cache(t0), false, false)
        })
    };
    let nothing = |_: i64| {};
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - floor), &nothing), 1);
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - floor + 1), &nothing), 0);
    let until_now = |t0: i64| {
        record_inactive_fetch_failure(&tracker, t0 - inactive_fetch_backoff_secs(1) as i64);
    };
    assert_eq!(inactive(&|t| cu_at(10.0, 10.0, t - 2000), &until_now), 1);
    let locked = |age: i64| {
        move |t: i64| {
            let mut c = cu_at(100.0, 10.0, t - age);
            c.session_reset = Some((Utc::now() + Duration::hours(3)).to_rfc3339());
            c
        }
    };
    assert_eq!(inactive(&locked(MAX_LOCKED_STALENESS_SECS), &nothing), 0);
    assert_eq!(
        inactive(&locked(MAX_LOCKED_STALENESS_SECS + 1), &nothing),
        1
    );
    // The active account is due exactly one interval after its reading.
    let base = WATCH_INTERVAL_SECS as i64;
    let active = |age: i64| {
        at_stable_second(|t0| {
            reset_usage_fetch_tracker(&tracker);
            codex_one(&k, cu_at(10.0, 10.0, t0 - age), true, false)
        })
    };
    assert_eq!(active(base), 1);
    assert_eq!(active(base - 1), 0);
    reset_usage_fetch_tracker(&tracker);
}

#[test]
fn claude_self_switch_keeps_the_usage_fetch_tracker() {
    with_claude_home("old@e.com", |_| {
        let email = "selfswitch-tracker@e.com";
        let acct = claude_account_with_identity(email, 1_000);
        let identity = acct.oauth_account.clone().unwrap();
        record_usage_fetch_429(email);
        apply_account(
            providers::get(CLAUDE_SLUG).unwrap(),
            &acct,
            &identity,
            Some(email),
            email,
        )
        .unwrap();
        assert_eq!(get_usage_fetch_tracker(email).consecutive_429s, 1);
        reset_usage_fetch_tracker(email);
    });
}

#[test]
fn note_polled_provider_active_reports_only_a_change_per_slug() {
    let slug = "polled-active-test";
    assert!(note_polled_provider_active(slug, Some("a")));
    assert!(!note_polled_provider_active(slug, Some("a")));
    assert!(note_polled_provider_active(slug, Some("b")));
    assert!(!note_polled_provider_active("other-slug-test", None));
    assert!(note_polled_provider_active(slug, None));
}

#[test]
fn freshly_switched_provider_active_bypasses_the_fresh_cache_skip() {
    let k = uniq("codex-just-switched");
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
    // First poll with this active key: the cache (taken while inactive) is
    // fresh, yet must be re-fetched.
    note_polled_provider_active("codex", Some("someone-else"));
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 10), true, false), 1);
    // Next poll, same active: the normal skip applies again.
    assert_eq!(codex_one(&k, cu(10.0, 10.0, 10), true, false), 0);
}

#[test]
fn a_just_switched_to_locked_codex_account_is_refetched() {
    // Locked in its (pre-switch) cache, but it reset early and the user just
    // switched to it: the active account is never skipped by the lock guard.
    let k = uniq("codex-active-locked");
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
    note_polled_provider_active("codex", Some("someone-else"));
    assert_eq!(codex_one(&k, cu_locked(2 * 3600), true, false), 1);
    reset_usage_fetch_tracker(&fetch_tracker_key("codex", &k));
}

#[test]
fn a_refresh_failure_reaches_the_row_until_a_fetch_succeeds() {
    let email = uniq("row-refresh-error");
    let mut a = acct_with_cache(Some(usage_at(40.0)));
    a.email = Some(email.clone());
    assert_eq!(row_from_account(&a).error, None);
    note_refresh_error(&email, Some("token refresh failed: HTTP 500".into()));
    let row = row_from_account(&a);
    assert_eq!(row.error.as_deref(), Some("token refresh failed: HTTP 500"));
    assert!(row.has_data(), "the cached sample still counts");
    // Provider rows read their own namespaced key.
    let mut p = codex_account(&email, None);
    p.cached_usage = Some(usage_at(40.0));
    assert_eq!(row_from_provider_account("codex", &p).error, None);
    note_refresh_error(
        &fetch_tracker_key("codex", &email),
        Some("usage fetch failed: boom".into()),
    );
    assert_eq!(
        row_from_provider_account("codex", &p).error.as_deref(),
        Some("usage fetch failed: boom")
    );
    record_usage_fetch_success(&email);
    record_usage_fetch_success(&fetch_tracker_key("codex", &email));
    assert_eq!(row_from_account(&a).error, None);
    assert_eq!(row_from_provider_account("codex", &p).error, None);
}
