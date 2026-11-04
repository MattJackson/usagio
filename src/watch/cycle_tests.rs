use super::*;
use crate::cli::capture::capture_current_generic;
use crate::poll::trackers::{
    claim_subscription_check, fetch_tracker_key, reset_usage_fetch_tracker,
};
use crate::providers::CLAUDE_SLUG;
use crate::rows::{provider_rows, Row};
use crate::store::{CachedUsage, State};
use crate::switching::provider::provider_supports_swap;
use crate::test_support::{
    account_with_cache, claude_account_with_identity, codex_auth_json, cu, history_events,
    hit_count, prime_active_account, read_log, seed_claude_state, spawn_mock_usage_server, uniq,
    with_claude_home, POLL_ACTIVE_LOCK,
};
use crate::{autoswap, store};
use chrono::{Duration, Utc};

/// Two captured Codex accounts, `active` being the one logged in, with the
/// given cached readings (fresh enough that no poll would fetch them).
fn seed_codex_pair(dir: &std::path::Path, active: &str, readings: [(&str, CachedUsage); 2]) {
    // Capture the non-active one first so the active one is captured last.
    let order: Vec<&str> = if readings[0].0 == active {
        vec![readings[1].0, readings[0].0]
    } else {
        vec![readings[0].0, readings[1].0]
    };
    for who in order {
        let blob = codex_auth_json(
            who,
            4_000_000_000,
            &format!("at-{who}"),
            &format!("rt-{who}"),
        );
        std::fs::write(dir.join("auth.json"), blob).unwrap();
        capture_current_generic("codex").unwrap();
    }
    let mut st = State::load().unwrap();
    for (who, cache) in readings {
        st.find_provider_account_mut("codex", who)
            .unwrap()
            .cached_usage = Some(cache);
    }
    st.save().unwrap();
}

fn codex_rows() -> Vec<Row> {
    provider_rows(&State::load().unwrap(), "codex")
}

/// Edit `slug`'s persisted auto-swap v2 memory.
fn set_swap_memory(slug: &str, f: impl FnOnce(&mut autoswap::SwapMemory)) {
    autoswap::shell::update_memory(slug, f).unwrap();
}

#[test]
fn provider_swap_cycle_flip_back_is_labelled_proactive_and_not_announced_as_a_decision() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "later@example.com",
            [
                ("later@example.com", cu(40.0, 40.0, 10)),
                ("sooner@example.com", cu(5.0, 5.0, 10)),
            ],
        );
        let mut st = State::load().unwrap();
        let soon = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let later = (Utc::now() + Duration::days(5)).to_rfc3339();
        st.find_provider_account_mut("codex", "sooner@example.com")
            .unwrap()
            .cached_usage
            .as_mut()
            .unwrap()
            .weekly_reset = Some(soon);
        st.find_provider_account_mut("codex", "later@example.com")
            .unwrap()
            .cached_usage
            .as_mut()
            .unwrap()
            .weekly_reset = Some(later);
        st.save().unwrap();
        let mut rows = codex_rows();
        let mut guard = SwapGuard::default();
        let (swapped, _) = provider_swap_cycle(
            "codex",
            &mut rows,
            "later@example.com",
            95.0,
            95.0,
            &mut guard,
        )
        .unwrap();
        assert_eq!(
            swapped,
            Some((
                "later@example.com".to_string(),
                "sooner@example.com".to_string()
            ))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "proactive");
        assert!(!read_log().contains("action=switching"));
    });
}

#[test]
fn provider_swap_cycle_reports_a_cooldown_blocked_preparation() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(0.0, 100.0, 10)),
                ("b@example.com", cu(0.0, 100.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        let now = Utc::now();
        for r in rows.iter_mut() {
            r.weekly.resets_at = Some(if r.email == "a@example.com" {
                now + Duration::days(5)
            } else {
                now + Duration::hours(2)
            });
        }
        // A switch a moment ago: the cooldown (v2: persisted memory) blocks
        // the optional Prepare move.
        set_swap_memory("codex", |m| {
            m.last_switch_at = Some(Utc::now().timestamp());
            m.last_seen_active = Some("a@example.com".to_string());
        });
        let mut guard = SwapGuard::default();
        let (swapped, actionable) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        assert_eq!(swapped, None);
        assert!(actionable, "a cooldown-blocked target is still actionable");
        assert!(read_log().contains("target=b@example.com action=blocked reason=cooldown"));
    });
}

#[test]
fn watch_cycle_polls_every_swappable_provider_and_swaps_the_exhausted_one() {
    let dir = tempfile::tempdir().unwrap();
    // Lock order everywhere: poll lock first, then the env lock.
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = with_claude_home("scratch-json@e.com", |_home| {
        // `scoped_env_var` is not reentrant and `with_claude_home` already
        // holds the env lock, so set the second variable by hand.
        struct RestoreCodexHome(Option<std::ffi::OsString>);
        impl Drop for RestoreCodexHome {
            fn drop(&mut self) {
                #[allow(clippy::disallowed_methods)]
                match self.0.take() {
                    Some(v) => std::env::set_var("CODEX_HOME", v),
                    None => std::env::remove_var("CODEX_HOME"),
                }
            }
        }
        #[allow(clippy::disallowed_methods)]
        let _restore = {
            let prev = std::env::var_os("CODEX_HOME");
            std::env::set_var("CODEX_HOME", dir.path());
            RestoreCodexHome(prev)
        };
        let _mark = crate::env_lock::mark_scoped("CODEX_HOME");
        {
            // No Claude account is active, so no keychain is involved; its two
            // accounts still feed the history log.
            let calm = claude_account_with_identity("calm@e.com", 3_600_000);
            let mut calm = calm;
            calm.cached_usage = Some(cu(20.0, 20.0, 10));
            seed_claude_state(vec![calm], None);
            seed_codex_pair(
                dir.path(),
                "hot@example.com",
                [
                    ("hot@example.com", cu(96.0, 40.0, 10)),
                    ("cool@example.com", cu(12.0, 7.0, 10)),
                ],
            );
            // The Codex CLI rotated auth.json behind usagio's back.
            // (A lower expiry than we hold: only the active-account follower adopts that.)
            let rotated = codex_auth_json("hot@example.com", 3_900_000_000, "cli-at", "cli-rt");
            std::fs::write(dir.path().join("auth.json"), rotated).unwrap();
            let mut guards = SwapGuards::default();
            let out = watch_cycle(95.0, 95.0, &mut guards, false).unwrap();
            // The rotation was absorbed before the swap (active at decision time).
            let st = State::load().unwrap();
            assert_eq!(
                st.find_provider_account("codex", "hot@example.com")
                    .unwrap()
                    .access_token,
                "cli-at"
            );
            // History records Claude's rows only.
            let history =
                std::fs::read_to_string(store::config_dir().unwrap().join("history.jsonl"))
                    .unwrap_or_default();
            assert!(history.contains("calm@e.com"));
            assert!(!history.contains("\"account\":\"hot@example.com\""));
            out
        }
    });
    // Claude has accounts but none active: reported, nothing to swap.
    let claude = outcome
        .providers
        .iter()
        .find(|p| p.slug == CLAUDE_SLUG)
        .unwrap();
    assert_eq!(claude.active, None);
    assert_eq!(claude.swapped, None);
    assert!(!claude.actionable);
    let codex = outcome
        .providers
        .iter()
        .find(|p| p.slug == "codex")
        .unwrap();
    assert_eq!(
        codex.swapped,
        Some((
            "hot@example.com".to_string(),
            "cool@example.com".to_string()
        ))
    );
    assert!(!codex.rate_limited);
    assert_eq!(codex.max_pct, Some(96.0));
    assert!(outcome
        .providers
        .iter()
        .all(|p| provider_supports_swap(&p.slug)));
}

#[test]
fn provider_swap_cycle_labels_urgent_and_prepared_moves_as_non_proactive() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Flipped for v2 (design §8.4): a 429 no longer counts as being at the
        // trigger; a Real reading at 96 makes the move a Trigger.
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(96.0, 40.0, 10)),
                ("b@example.com", cu(10.0, 10.0, 10)),
            ],
        );
        let key = fetch_tracker_key("codex", "a@example.com");
        let mut rows = codex_rows();
        let mut guard = SwapGuard::default();
        let (swapped, _) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        reset_usage_fetch_tracker(&key);
        assert_eq!(
            swapped,
            Some(("a@example.com".to_string(), "b@example.com".to_string()))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "trigger");
        assert!(read_log().contains("action=switching"));
    });
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        // Every account blocked: the move prepares the one that recovers first.
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(0.0, 100.0, 10)),
                ("b@example.com", cu(0.0, 100.0, 10)),
            ],
        );
        let mut rows = codex_rows();
        let now = Utc::now();
        for r in rows.iter_mut() {
            r.weekly.resets_at = Some(if r.email == "a@example.com" {
                now + Duration::days(5)
            } else {
                now + Duration::hours(2)
            });
        }
        let mut guard = SwapGuard::default();
        let (swapped, _) =
            provider_swap_cycle("codex", &mut rows, "a@example.com", 95.0, 95.0, &mut guard)
                .unwrap();
        assert_eq!(
            swapped,
            Some(("a@example.com".to_string(), "b@example.com".to_string()))
        );
        let ev = history_events()
            .into_iter()
            .find(|e| e["event"] == "swap")
            .unwrap();
        assert_eq!(ev["reason"], "prepare_reset");
        assert!(read_log().contains("action=switching"));
    });
}

#[cfg_attr(
    not(target_os = "macos"),
    ignore = "reads or writes the OS secret store, which is only hermetic (in-memory) on macOS"
)]
#[test]
fn watch_cycle_reports_the_claude_429_and_swaps_claude_accounts() {
    use crate::providers::claude::usage;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let outcome = with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(429, String::new());
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        let a = uniq("wc-active");
        let b = uniq("wc-target");
        // Throttle the lapse probe so the 429 never reaches the profile endpoint.
        for k in [&a, &b] {
            claim_subscription_check(k, Utc::now().timestamp(), false);
        }
        // Flipped for v2 (design §8.4, D1): the active reading is 61 s old
        // (due for a fetch, not yet blind), the fetch 429s. A 429 is no
        // information (I8): it is reported, and nobody moves.
        let active = account_with_cache(&a, Some(cu(93.0, 10.0, 61)));
        prime_active_account(&active);
        seed_claude_state(
            vec![active, account_with_cache(&b, Some(cu(10.0, 10.0, 10)))],
            Some(&a),
        );
        let out = watch_cycle(95.0, 95.0, &mut SwapGuards::default(), false).unwrap();
        usage::set_usage_url_override(None);
        assert_eq!(hit_count(&hits), 1);
        assert_eq!(State::load().unwrap().active.as_deref(), Some(a.as_str()));
        reset_usage_fetch_tracker(&a);
        reset_usage_fetch_tracker(&b);
        out
    });
    let claude = outcome
        .providers
        .iter()
        .find(|p| p.slug == CLAUDE_SLUG)
        .unwrap();
    assert!(claude.rate_limited);
    assert!(claude.swapped.is_none());
    assert_eq!(claude.urgency, Some(autoswap::Urgency::Band));
    let codex_limited = outcome
        .providers
        .iter()
        .any(|p| p.slug != CLAUDE_SLUG && p.rate_limited);
    assert!(!codex_limited);
}

#[test]
fn provider_swap_cycle_v2_logs_one_decision_line_per_change() {
    let _cfg = crate::store::ScopedConfigDir::new();
    let dir = tempfile::tempdir().unwrap();
    crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
        seed_codex_pair(
            dir.path(),
            "a@example.com",
            [
                ("a@example.com", cu(30.0, 30.0, 10)),
                ("b@example.com", cu(30.0, 30.0, 10)),
            ],
        );
        let mut guard = SwapGuard::default();
        for _ in 0..5 {
            provider_swap_cycle(
                "codex",
                &mut codex_rows(),
                "a@example.com",
                95.0,
                95.0,
                &mut guard,
            )
            .unwrap();
        }
        let lines = crate::usage_log::read_decisions();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["engine"], "v2");
        assert_eq!(lines[0]["reason"], "ok");
        assert_eq!(read_log().matches("action=stayed reason=ok").count(), 1);
    });
}
