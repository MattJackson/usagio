//! Display/decision rows built from stored accounts and their cached usage.

use chrono::{DateTime, Utc};

use crate::display::humanize_until;
use crate::poll::trackers::{fetch_tracker_key, last_refresh_error};
use crate::providers::claude::usage;
use crate::providers::trait_def::UsageSnapshot;
use crate::providers::CLAUDE_SLUG;
use crate::store::{Account, CachedUsage, ProviderAccount, State};
use crate::{countdown, providers};

/// True iff the provider registered under `slug` is eligible as an auto-swap
/// candidate — i.e. it exposes both a usage signal (so we can compare
/// candidates) and a way to switch to it. An unknown slug returns `false`:
/// the safest thing when a Row's `provider_id` doesn't correspond to any
/// registered provider is to leave it out of the auto-swap decision.
/// The swap-decision rows for one provider's captured accounts. Claude keeps
/// its dedicated `State::accounts` slot; every other provider lives in
/// `State::providers[slug]`.
pub(crate) fn provider_rows(state: &State, slug: &str) -> Vec<Row> {
    if slug == CLAUDE_SLUG {
        state.accounts.iter().map(row_from_account).collect()
    } else {
        state
            .provider_accounts(slug)
            .map(|pa| {
                pa.accounts
                    .iter()
                    .map(|a| row_from_provider_account(slug, a))
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Usage rows + auto-pick
// ---------------------------------------------------------------------------

pub(crate) struct Row {
    /// Slug of the provider that owns this row's account (matches a registered
    /// `Provider::provider_id`). Threaded through swap decisions so we can
    /// consult `providers::get(...).capabilities()` without re-reading state.
    /// In v1 every row is `"claude"`; a Row whose slug isn't registered is
    /// simply skipped as an auto-swap candidate.
    pub(crate) provider_id: String,
    /// True when the OAuth token endpoint has permanently rejected this
    /// account's refresh token. Filtered out of the auto-swap picker and
    /// surfaced by the menu as a re-login row.
    pub(crate) needs_relogin: bool,
    /// The account's Claude subscription has lapsed (see
    /// `Account::no_subscription`). Rendered as the plan instead of usage,
    /// never a swap target, and a reason to swap AWAY when active.
    pub(crate) no_subscription: bool,
    /// Short plan label ("Max 20x", "Pro", "Free", …) when known.
    pub(crate) plan: Option<String>,
    pub(crate) email: String,
    pub(crate) session: Cell,
    pub(crate) weekly: Cell,
    pub(crate) opus: Option<Cell>,
    pub(crate) error: Option<String>,
    /// Unix epoch seconds when the cached usage was fetched (None = no data yet).
    pub(crate) fetched_at: Option<i64>,
    /// Credits / breakdown / scoped limits the provider reported (never estimates).
    pub(crate) reported: providers::trait_def::ReportedUsage,
}

pub(crate) struct Cell {
    pub(crate) pct: Option<f64>,
    pub(crate) resets_at: Option<DateTime<Utc>>,
}

impl Cell {
    pub(crate) fn resets_in(&self) -> String {
        match self.resets_at {
            Some(dt) => humanize_until(dt),
            None => String::new(),
        }
    }
}

/// Short plan label from the captured `oauthAccount` ("Ultra", "Max 20x", "Pro", …).
/// A lapsed subscription is always "Free", whatever was captured.
pub(crate) fn claude_plan_label(a: &Account) -> Option<String> {
    if a.no_subscription {
        return Some(a.plan.clone().unwrap_or_else(|| "Free".to_string()));
    }
    let get = |k: &str| a.oauth_account.as_ref()?.get(k)?.as_str();
    let inferred = usage::plan_label(
        None,
        get("organizationType"),
        get("organizationRateLimitTier"),
    );
    // A profile response without a display name only repeats the old tier
    // mapping. It must not mask the vendor's explicit name saved by Claude.
    if a.plan.is_some() && a.plan != inferred {
        return a.plan.clone();
    }
    usage::plan_label(
        get("planDisplayName"),
        get("organizationType"),
        get("organizationRateLimitTier"),
    )
}

/// Build a display row from an account's cached usage (never fetches).
pub(crate) fn row_from_account(a: &Account) -> Row {
    let c = a.cached_usage.as_ref();
    Row {
        // `State::accounts` is Claude's dedicated slot (state v2 keeps it
        // that way rather than folding it into `State::providers` — see
        // `store.rs`'s `State` doc), so every row built from it is Claude's.
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: a.needs_relogin,
        no_subscription: a.no_subscription,
        plan: claude_plan_label(a),
        email: a.key().to_string(),
        session: cell_from_parts(
            c.and_then(|c| c.session_pct),
            c.and_then(|c| c.session_reset.as_deref()),
        ),
        weekly: cell_from_parts(
            c.and_then(|c| c.weekly_pct),
            c.and_then(|c| c.weekly_reset.as_deref()),
        ),
        opus: c.and_then(|c| {
            c.opus_pct
                .map(|p| cell_from_parts(Some(p), c.opus_reset.as_deref()))
        }),
        error: last_refresh_error(&fetch_tracker_key(CLAUDE_SLUG, a.key())),
        fetched_at: c.map(|c| c.fetched_at),
        reported: c.map(|c| c.reported.clone()).unwrap_or_default(),
    }
}

/// Build a display row from a non-Claude provider's captured account
/// (`State::providers[slug]`). Mirrors `row_from_account` exactly, just
/// reading `ProviderAccount`'s generic `cached_usage` instead of `Account`'s.
/// No `opus` window — that's a Claude-specific bucket (see `window_order`
/// filtering it back out for providers that don't declare it).
pub(crate) fn row_from_provider_account(slug: &str, a: &ProviderAccount) -> Row {
    let c = a.cached_usage.as_ref();
    Row {
        provider_id: slug.to_string(),
        needs_relogin: a.needs_relogin,
        no_subscription: a.no_subscription,
        plan: match (&a.plan, a.no_subscription) {
            (Some(p), _) => Some(p.clone()),
            (None, true) => Some("Free".to_string()),
            (None, false) => None,
        },
        email: a.key.clone(),
        session: cell_from_parts(
            c.and_then(|c| c.session_pct),
            c.and_then(|c| c.session_reset.as_deref()),
        ),
        weekly: cell_from_parts(
            c.and_then(|c| c.weekly_pct),
            c.and_then(|c| c.weekly_reset.as_deref()),
        ),
        opus: c.and_then(|c| {
            c.opus_pct
                .map(|p| cell_from_parts(Some(p), c.opus_reset.as_deref()))
        }),
        error: last_refresh_error(&fetch_tracker_key(slug, &a.key)),
        fetched_at: c.map(|c| c.fetched_at),
        reported: c.map(|c| c.reported.clone()).unwrap_or_default(),
    }
}

/// Convert a fetched `Usage` into the cacheable snapshot.
pub(crate) fn cached_from_usage(u: &usage::Usage) -> CachedUsage {
    let parts = |w: &Option<usage::Window>| {
        w.as_ref()
            .map(|w| (w.utilization, w.resets_at.clone()))
            .unwrap_or((None, None))
    };
    let (session_pct, session_reset) = parts(&u.five_hour);
    let (weekly_pct, weekly_reset) = parts(&u.seven_day);
    let (opus_pct, opus_reset) = parts(&u.seven_day_opus);
    CachedUsage {
        session_pct,
        weekly_pct,
        session_reset,
        weekly_reset,
        opus_pct,
        opus_reset,
        fetched_at: Utc::now().timestamp(),
        // Same provider-agnostic shape every provider's snapshot carries.
        reported: usage::reported_usage(u),
    }
}

/// How long a prepaid wallet reading is reused before `prepaid/credits` is
/// asked again. The balance only moves on top-ups and credit spend, so one
/// request per account every few minutes is plenty; without this every poll
/// (down to 30 s) cost an extra request per credits-enabled account.
pub(crate) const PREPAID_REFRESH_SECS: i64 = 10 * 60;

/// Per-account `(attempted_at, fetched balance)` of the last `prepaid/credits`
/// request. A failed attempt is recorded too (as `None`), so an erroring
/// endpoint is not retried every poll.
pub(crate) type PrepaidCache =
    std::collections::HashMap<String, (i64, Option<providers::trait_def::Money>)>;
pub(crate) static PREPAID_CACHE: std::sync::Mutex<Option<PrepaidCache>> =
    std::sync::Mutex::new(None);

/// Fill in the real prepaid wallet balance for an account with credits on.
/// `/usage` reports `spend.balance: null` even when the wallet holds money, so
/// the balance comes from the org's `prepaid/credits` endpoint, at most once
/// per [`PREPAID_REFRESH_SECS`] per account. Best-effort: on a failed fetch
/// keep the balance from the previous snapshot rather than blanking it.
pub(crate) fn attach_prepaid_balance(cu: &mut CachedUsage, acct: &Account) {
    attach_prepaid_balance_with(cu, acct, Utc::now().timestamp(), |token, org| {
        usage::fetch_prepaid_balance(token, org)
    });
}

pub(crate) fn attach_prepaid_balance_with(
    cu: &mut CachedUsage,
    acct: &Account,
    now: i64,
    fetch: impl FnOnce(&str, &str) -> Option<providers::trait_def::Money>,
) {
    let Some(credits) = cu.reported.credits.as_mut() else {
        return;
    };
    // Credits off: no wallet to show. A balance `/usage` reported itself
    // needs no second request.
    if !credits.enabled || credits.balance.is_some() {
        return;
    }
    let key = acct.key().to_ascii_lowercase();
    let previous = || {
        acct.cached_usage
            .as_ref()?
            .reported
            .credits
            .as_ref()?
            .balance
            .clone()
    };
    let recent = {
        let guard = PREPAID_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .as_ref()
            .and_then(|m| m.get(&key))
            .filter(|(at, _)| (0..PREPAID_REFRESH_SECS).contains(&(now - at)))
            .map(|(_, m)| m.clone())
    };
    if let Some(cached) = recent {
        credits.balance = cached.or_else(previous);
        return;
    }
    let org = acct
        .oauth_account
        .as_ref()
        .and_then(|o| o.get("organizationUuid"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let Some(org) = org else {
        credits.balance = previous();
        return;
    };
    let fetched = fetch(&acct.access_token, org);
    PREPAID_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(Default::default)
        .insert(key, (now, fetched.clone()));
    credits.balance = fetched.or_else(previous);
}

impl Row {
    /// True once we have a cached usage sample to reason about. A refresh
    /// error does not discard it: the engine keeps judging the cached sample
    /// by its age, and the menu shows the error beside it.
    pub(crate) fn has_data(&self) -> bool {
        self.fetched_at.is_some()
    }

    /// Money the account can still spend on usage credits — see
    /// `Credits::spendable`. `None` means it can't run on credits.
    pub(crate) fn spendable_credits(&self) -> Option<providers::trait_def::Money> {
        self.reported.credits.as_ref()?.spendable()
    }

    /// Remaining percent on the tightest of session/weekly.
    pub(crate) fn headroom(&self) -> f64 {
        100.0 - self.max_pct()
    }

    /// Utilization of the tightest of session/weekly.
    pub(crate) fn max_pct(&self) -> f64 {
        self.session
            .pct
            .unwrap_or(0.0)
            .max(self.weekly.pct.unwrap_or(0.0))
    }
}

pub(crate) fn cell_from_parts(pct: Option<f64>, reset: Option<&str>) -> Cell {
    Cell {
        pct,
        resets_at: reset
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
    }
}

/// Map a provider [`UsageSnapshot`] into the stored [`CachedUsage`] shape the
/// menu/list render from. Providers normalize durations to `session`/`weekly`;
/// `primary`/`secondary` remain aliases for providers using the older contract.
pub(crate) fn cached_from_usage_snapshot(snap: &UsageSnapshot) -> CachedUsage {
    let mut cu = CachedUsage {
        session_pct: None,
        weekly_pct: None,
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: snap.fetched_at.timestamp(),
        reported: snap.reported.clone(),
    };
    for w in &snap.windows {
        let reset = w.resets_at.map(|dt| dt.to_rfc3339());
        match w.id.as_str() {
            "primary" | "session" => {
                cu.session_pct = w.utilization;
                cu.session_reset = reset;
            }
            "secondary" | "weekly" => {
                cu.weekly_pct = w.utilization;
                cu.weekly_reset = reset;
            }
            _ => {}
        }
    }
    cu
}

// ---------------------------------------------------------------------------
// watch — auto-swap daemon
// ---------------------------------------------------------------------------

/// Fold a `Row`'s cached usage into `countdown::AccountUsage` so
/// `countdown` can reason about its reset instants.
pub(crate) fn row_to_account_usage(r: &Row) -> countdown::AccountUsage {
    countdown::AccountUsage {
        session_pct: r.session.pct,
        session_reset: r.session.resets_at,
        weekly_pct: r.weekly.pct,
        weekly_reset: r.weekly.resets_at,
        fetched_at: r.fetched_at.and_then(|t| DateTime::from_timestamp(t, 0)),
    }
}

/// Parse a cached-usage snapshot into the `countdown::AccountUsage` the lock
/// predicates operate on (RFC3339 reset strings → `DateTime<Utc>`, epoch
/// seconds → `DateTime`). Used by `refresh_usage_cache` to decide, per
/// account, whether the account is locked-until-reset and can skip its network
/// refresh this cycle.
pub(crate) fn cached_to_account_usage(cu: &CachedUsage) -> countdown::AccountUsage {
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    };
    countdown::AccountUsage {
        session_pct: cu.session_pct,
        session_reset: parse(&cu.session_reset),
        weekly_pct: cu.weekly_pct,
        weekly_reset: parse(&cu.weekly_reset),
        fetched_at: DateTime::from_timestamp(cu.fetched_at, 0),
    }
}

#[cfg(test)]
#[path = "rows_tests.rs"]
mod tests;
