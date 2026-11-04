use super::*;
use crate::poll::trackers::reset_usage_fetch_tracker;
use crate::store::State;
use crate::test_support::{
    account_with_cache, cu, hit_count, seed_claude_state, spawn_mock_usage_server, uniq,
    usage_body, with_claude_home, POLL_ACTIVE_LOCK,
};

#[test]
fn list_with_a_refresh_flag_fetches_once_and_without_it_never_does() {
    use crate::providers::claude::usage;
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let email = uniq("list");
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(200, usage_body(10.0, 10.0));
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        seed_claude_state(
            vec![account_with_cache(&email, Some(cu(10.0, 10.0, 2000)))],
            None,
        );
        cmd_list(&[]).unwrap();
        assert_eq!(hit_count(&hits), 0, "plain list reads the cache");
        cmd_list(&["--refresh".to_string()]).unwrap();
        assert_eq!(hit_count(&hits), 1);
        let st = State::load().unwrap();
        st.find(&email).unwrap();
        // The short flag is the same request, and it forces a fetch even
        // though the cache is now fresh.
        cmd_list(&["-r".to_string()]).unwrap();
        usage::set_usage_url_override(None);
        assert_eq!(hit_count(&hits), 2);
    });
    reset_usage_fetch_tracker(&email);
}
