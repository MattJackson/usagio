use super::*;
use serde_json::{json, Value};

fn account(email: &str) -> Account {
    let mut account = Account::from_keychain_blob(
        &json!({"claudeAiOauth": {
            "accessToken": email, "refreshToken": "kept-refresh",
            "expiresAt": (Utc::now().timestamp() + 3600) * 1000,
        }})
        .to_string(),
    )
    .unwrap();
    account.email = Some(email.into());
    account.plan = Some("Max 20x".into());
    account.oauth_account = Some(json!({
        "emailAddress": email, "accountUuid": format!("uuid-{email}"),
        "organizationType": "claude_max",
        "organizationRateLimitTier": "default_claude_max_20x",
    }));
    account
}

fn profile(email: &str) -> Value {
    json!({
        "account": {"email": email, "uuid": format!("uuid-{email}")},
        "organization": {"organization_type": "claude_max",
            "rate_limit_tier": "default_claude_max_20x",
            "plan_display_name": "Ultra", "subscription_status": "active"},
    })
}

fn seed(accounts: Vec<Account>) {
    State {
        accounts,
        active: Some("a@example.com".into()),
        ..Default::default()
    }
    .save()
    .unwrap();
}

#[test]
fn all_accounts_refresh_in_bounded_batches_even_when_usage_is_locked() {
    let _dir = crate::store::ScopedConfigDir::new();
    let mut accounts: Vec<_> = ["a@example.com", "b@example.com", "c@example.com"]
        .into_iter()
        .map(account)
        .collect();
    for account in &mut accounts {
        account.cached_usage = Some(crate::store::CachedUsage {
            weekly_pct: Some(100.0),
            fetched_at: Utc::now().timestamp(),
            weekly_reset: Some((Utc::now() + chrono::Duration::days(3)).to_rfc3339()),
            ..Default::default()
        });
    }
    let original = accounts.clone();
    seed(accounts);
    let mut requests = Vec::new();
    refresh_with(BATCH_SIZE, false, |token| {
        requests.push(token.to_string());
        Some(profile(token))
    });
    assert_eq!(requests.len(), 2);
    // Reloaded persisted scheduling leaves the third account next in line.
    refresh_with(BATCH_SIZE, false, |token| {
        requests.push(token.to_string());
        Some(profile(token))
    });
    assert_eq!(
        requests,
        ["a@example.com", "b@example.com", "c@example.com"]
    );
    refresh_with(BATCH_SIZE, false, |_| {
        panic!("fresh profiles are not fetched again")
    });
    let state = State::load().unwrap();
    assert_eq!(state.active.as_deref(), Some("a@example.com"));
    for (saved, old) in state.accounts.iter().zip(original) {
        assert_eq!(saved.plan.as_deref(), Some("Ultra"));
        assert_eq!(
            saved.oauth_account.as_ref().unwrap()["planDisplayName"],
            "Ultra"
        );
        assert_eq!(saved.keychain_blob, old.keychain_blob);
        assert_eq!(saved.cached_usage, old.cached_usage);
        assert!(saved.profile_refresh_after.unwrap() >= Utc::now().timestamp() + REFRESH_SECS - 2);
    }
}

#[test]
fn failures_retry_without_erasing_names_or_starving_other_accounts() {
    let _dir = crate::store::ScopedConfigDir::new();
    seed(vec![account("a@example.com"), account("b@example.com")]);
    refresh_with(1, false, |_| None);
    let state = State::load().unwrap();
    let failed = state.find("a@example.com").unwrap();
    assert_eq!(failed.plan.as_deref(), Some("Max 20x"));
    assert!(failed.profile_refresh_after.unwrap() >= Utc::now().timestamp() + RETRY_SECS - 2);
    refresh_with(1, false, |token| {
        assert_eq!(token, "b@example.com");
        Some(profile(token))
    });
    refresh_with(2, false, |_| panic!("retry window survives reload"));
    // A manual refresh bypasses the schedule.
    refresh_with(2, true, |token| Some(profile(token)));
    assert!(State::load()
        .unwrap()
        .accounts
        .iter()
        .all(|a| a.plan.as_deref() == Some("Ultra")));
}

#[test]
fn expired_revoked_and_lapsed_accounts_do_not_spend_profile_requests() {
    let _dir = crate::store::ScopedConfigDir::new();
    let mut expired = account("expired@example.com");
    expired.expires_at = 0;
    let mut revoked = account("revoked@example.com");
    revoked.needs_relogin = true;
    let mut lapsed = account("lapsed@example.com");
    lapsed.no_subscription = true;
    lapsed.plan = Some("Free".into());
    seed(vec![expired, revoked, lapsed]);
    refresh_with(10, true, |_| {
        panic!("requires valid access without rotating credentials")
    });
    assert!(State::load()
        .unwrap()
        .accounts
        .iter()
        .all(|a| a.profile_refresh_after.is_none()));
}

#[test]
fn wrong_identity_and_malformed_profiles_are_ignored() {
    let _dir = crate::store::ScopedConfigDir::new();
    let original = account("a@example.com");
    seed(vec![original.clone()]);
    for response in [
        profile("stranger@example.com"),
        json!({}),
        json!({"organization": {"organization_type": "claude_free"}}),
    ] {
        assert!(!commit(&original, &response).unwrap());
        let saved = State::load().unwrap().accounts.remove(0);
        assert_eq!(saved.oauth_account, original.oauth_account);
        assert_eq!(saved.plan, original.plan);
    }
}

#[test]
fn delayed_profiles_cannot_overwrite_newer_login_or_metadata() {
    let _dir = crate::store::ScopedConfigDir::new();
    for change in [
        "tokens", "identity", "plan", "lapse", "relogin", "schedule", "delete",
    ] {
        let original = account("a@example.com");
        seed(vec![original.clone()]);
        let mut state = State::load().unwrap();
        let newer = state.accounts.first_mut().unwrap();
        match change {
            "tokens" => newer.set_tokens("new-access".into(), "new-refresh".into(), 123),
            "identity" => {
                newer.oauth_account.as_mut().unwrap()["planDisplayName"] = "Newer plan".into()
            }
            "plan" => newer.plan = Some("Newer plan".into()),
            "lapse" => newer.no_subscription = true,
            "relogin" => newer.needs_relogin = true,
            "schedule" => newer.profile_refresh_after = Some(123),
            "delete" => {
                state.remove("a@example.com");
            }
            _ => unreachable!(),
        }
        state.save().unwrap();
        assert!(
            !commit(&original, &profile("a@example.com")).unwrap(),
            "{change}"
        );
        let after = State::load().unwrap();
        assert_eq!(
            serde_json::to_value(after.accounts).unwrap(),
            serde_json::to_value(state.accounts).unwrap(),
            "{change}"
        );
    }
}

#[test]
fn newer_usage_does_not_block_profile_commit() {
    let _dir = crate::store::ScopedConfigDir::new();
    let original = account("a@example.com");
    seed(vec![original.clone()]);
    let cache = crate::store::CachedUsage {
        weekly_pct: Some(42.0),
        fetched_at: Utc::now().timestamp(),
        ..Default::default()
    };
    let mut state = State::load().unwrap();
    state.accounts[0].cached_usage = Some(cache.clone());
    state.save().unwrap();
    assert!(commit(&original, &profile("a@example.com")).unwrap());
    let saved = State::load().unwrap().accounts.remove(0);
    assert_eq!(saved.plan.as_deref(), Some("Ultra"));
    assert_eq!(saved.cached_usage, Some(cache));
}

#[test]
fn tier_changes_drop_old_names_but_missing_names_on_the_same_tier_preserve_them() {
    let _dir = crate::store::ScopedConfigDir::new();
    let mut original = account("a@example.com");
    original.oauth_account.as_mut().unwrap()["planDisplayName"] = "Ultra".into();
    original.plan = Some("Ultra".into());
    seed(vec![original.clone()]);
    let mut response = profile("a@example.com");
    response["organization"]
        .as_object_mut()
        .unwrap()
        .remove("plan_display_name");
    assert!(commit(&original, &response).unwrap());
    assert_eq!(
        State::load().unwrap().accounts[0].plan.as_deref(),
        Some("Ultra")
    );
    let original = State::load().unwrap().accounts.remove(0);
    response["organization"]["organization_type"] = "claude_pro".into();
    response["organization"]["rate_limit_tier"] = "default_claude_pro".into();
    assert!(commit(&original, &response).unwrap());
    let saved = State::load().unwrap().accounts.remove(0);
    assert_eq!(saved.plan.as_deref(), Some("Pro"));
    assert!(saved
        .oauth_account
        .unwrap()
        .get("planDisplayName")
        .is_none());
}
