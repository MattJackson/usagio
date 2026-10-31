//! Fetch and model the /api/oauth/usage response.

use serde::Deserialize;

use super::config;
use crate::providers::trait_def::{
    Credits, Money, ReportedUsage, ResetGrant, UsageShare, UsageWindow,
};

/// Why a usage fetch failed. Lets callers keep the last-known cache on transient
/// failures (and back off on rate limits) instead of surfacing a hard error.
#[derive(Debug)]
pub enum FetchError {
    /// HTTP 429 — we're being rate limited; back off and reuse the cache.
    RateLimited,
    /// HTTP 401 — token expired or revoked.
    Auth,
    /// HTTP 403 — the token is valid but the account may not use the endpoint.
    /// Seen when the account's Claude subscription has lapsed; the caller
    /// confirms via the profile (`Provider::check_plan`) before treating it as that.
    Forbidden,
    /// Network / transport failure (no HTTP status).
    Transient(String),
    /// Any other non-success status or a parse failure.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Auth => write!(f, "unauthorized (token expired or revoked)"),
            FetchError::Forbidden => write!(f, "forbidden (HTTP 403)"),
            FetchError::Transient(e) => write!(f, "transient error: {e}"),
            FetchError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    /// Percent of the limit used (e.g. 9.0 == 9%).
    #[serde(default)]
    pub utilization: Option<f64>,
    /// ISO-8601 timestamp when this window resets.
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    /// Rolling session limit (the ~5-hour window).
    pub five_hour: Option<Window>,
    /// Weekly (7-day) all-models limit.
    pub seven_day: Option<Window>,
    /// Weekly Opus-scoped limit, when present.
    pub seven_day_opus: Option<Window>,
    /// Extra-usage (pay-as-you-go credits) settings and state. Kept as raw
    /// JSON and read leniently by [`reported_usage`] so a renamed or retyped
    /// field can never fail the whole usage parse.
    #[serde(default)]
    pub extra_usage: Option<serde_json::Value>,
    /// Money actually spent on extra usage (`{used, limit, balance, …}`).
    #[serde(default)]
    pub spend: Option<serde_json::Value>,
    /// Where this week's usage went (`{rows: [{key, display_name, percent}]}`).
    #[serde(default)]
    pub seven_day_breakdown: Option<serde_json::Value>,
    /// Every limit the account has, including per-model scoped weekly ones.
    #[serde(default)]
    pub limits: Option<serde_json::Value>,
    /// The `cedar_ember` reset-grant block, present only when the request asks
    /// for it (`?cedar_ember=1`) with a current `claude-cli` user-agent. Kept
    /// as raw JSON and read leniently by [`reset_grants`] so a shape change
    /// can never fail the whole usage parse.
    #[serde(default)]
    pub cedar_ember: Option<serde_json::Value>,
}

/// Parse a money object `{amount_minor, currency, exponent}`; `None` for null
/// or any other shape.
fn money(v: Option<&serde_json::Value>) -> Option<Money> {
    let o = v?.as_object()?;
    let amount = o.get("amount_minor")?.as_i64()?;
    let exponent = u32::try_from(o.get("exponent").and_then(|x| x.as_u64()).unwrap_or(0)).ok()?;
    let unit = o
        .get("currency")
        .and_then(|x| x.as_str())
        .unwrap_or(Money::CREDITS);
    Money::new(amount, exponent, unit)
}

fn get<'a>(o: Option<&'a serde_json::Value>, k: &str) -> Option<&'a serde_json::Value> {
    o.and_then(|o| o.get(k))
}

fn flag(o: Option<&serde_json::Value>, k: &str) -> bool {
    o.and_then(|o| o.get(k))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

/// Credits from `spend` (preferred — it carries exact money) and
/// `extra_usage` (flags, plus `used_credits` when it's an integer count of
/// minor units alongside `decimal_places`). `None` unless meaningful.
fn credits(u: &Usage) -> Option<Credits> {
    let spend = u.spend.as_ref().filter(|v| v.is_object());
    let extra = u.extra_usage.as_ref().filter(|v| v.is_object());
    let used = money(get(spend, "used")).or_else(|| {
        // Live responses send this as a float (`5463.0`) — `as_i64` alone
        // rejects that.
        let amount = get(extra, "used_credits")
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))?;
        let places = u32::try_from(get(extra, "decimal_places")?.as_u64()?).ok()?;
        let unit = get(extra, "currency")
            .and_then(|x| x.as_str())
            .unwrap_or(Money::CREDITS);
        Money::new(amount, places, unit)
    });
    let c = Credits {
        enabled: flag(spend, "enabled") || flag(extra, "is_enabled"),
        unlimited: false,
        limit_reached: flag(extra, "spend_limit_reached"),
        used,
        limit: money(get(spend, "limit")).or_else(|| money(get(spend, "cap"))),
        balance: money(get(spend, "balance")),
    };
    c.is_meaningful().then_some(c)
}

fn breakdown(u: &Usage) -> Vec<UsageShare> {
    let rows = u
        .seven_day_breakdown
        .as_ref()
        .and_then(|b| b.get("rows"))
        .and_then(|r| r.as_array());
    rows.into_iter()
        .flatten()
        .filter_map(|r| {
            let percent = r.get("percent")?.as_f64()?;
            let key = r.get("key").and_then(|x| x.as_str()).unwrap_or("");
            let label = r
                .get("display_name")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(key);
            (!label.is_empty()).then(|| UsageShare {
                key: key.to_string(),
                label: label.to_string(),
                percent,
            })
        })
        .collect()
}

/// A scope's display name: `scope.model.display_name`, else a surface's.
fn scope_name(scope: &serde_json::Value) -> Option<String> {
    let name_of = |v: Option<&serde_json::Value>| -> Option<String> {
        let v = v?;
        v.as_str()
            .or_else(|| v.get("display_name").and_then(|x| x.as_str()))
            .or_else(|| v.get("id").and_then(|x| x.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    name_of(scope.get("model")).or_else(|| name_of(scope.get("surface")))
}

/// Per-model/surface scoped weekly limits with a reported percent. An Opus
/// scope is skipped when `seven_day_opus` already carries it (it has its own
/// fixed window).
fn scoped_limits(u: &Usage) -> Vec<UsageWindow> {
    let has_opus_window = u
        .seven_day_opus
        .as_ref()
        .is_some_and(|w| w.utilization.is_some());
    let limits = u.limits.as_ref().and_then(|l| l.as_array());
    limits
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let kind = l.get("kind").and_then(|x| x.as_str()).unwrap_or("");
            let group = l.get("group").and_then(|x| x.as_str()).unwrap_or("");
            if kind != "weekly_scoped" && !(group == "weekly" && kind.ends_with("_scoped")) {
                return None;
            }
            let percent = l.get("percent")?.as_f64()?;
            let name = scope_name(l.get("scope")?)?;
            if has_opus_window && name.eq_ignore_ascii_case("opus") {
                return None;
            }
            let resets_at = l
                .get("resets_at")
                .and_then(|x| x.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            Some(UsageWindow {
                id: format!("scoped:{}", name.to_ascii_lowercase()),
                label: format!("{name} 7d"),
                utilization: Some(percent),
                resets_at,
            })
        })
        .collect()
}

/// The `cedar_ember` reset grants on this account, read leniently. A grant is
/// kept only when it still has a reset left to spend (`resets_left >= 1`), so a
/// spent or empty grant never surfaces. `usable_now` reflects the block's own
/// flag AND that the account is at a limit — a reset banked for later reads as
/// not-usable until you're actually blocked.
fn reset_grants(u: &Usage) -> Vec<ResetGrant> {
    let block = u.cedar_ember.as_ref().and_then(|v| v.as_object());
    let Some(block) = block else {
        return Vec::new();
    };
    let grants = block.get("grants").and_then(|g| g.as_array());
    grants
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let id = g
                .get("id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())?;
            let resets_left =
                u32::try_from(g.get("resets_left").and_then(|x| x.as_u64()).unwrap_or(0)).ok()?;
            if resets_left < 1 {
                return None;
            }
            let label = g
                .get("label")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let usable_now = g
                .get("usable_now")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let expires_at = g
                .get("ends_at")
                .and_then(|x| x.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            Some(ResetGrant {
                id: id.to_string(),
                label,
                resets_left,
                usable_now,
                expires_at,
            })
        })
        .collect()
}

/// Everything beyond the fixed windows that the usage response reported.
pub fn reported_usage(u: &Usage) -> ReportedUsage {
    ReportedUsage {
        credits: credits(u),
        breakdown: breakdown(u),
        scoped_limits: scoped_limits(u),
        reset_grants: reset_grants(u),
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override for the usage endpoint — see
    /// `oauth::TOKEN_URL_OVERRIDE` for the rationale (an in-process mock
    /// server standing in for the real Anthropic endpoint so tests never
    /// depend on outbound network access).
    static USAGE_URL_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Install (or clear) the current thread's usage-URL override. Test-only.
#[cfg(test)]
pub(crate) fn set_usage_url_override(url: Option<&str>) {
    USAGE_URL_OVERRIDE.with(|c| *c.borrow_mut() = url.map(String::from));
}

#[cfg(test)]
fn usage_url() -> String {
    USAGE_URL_OVERRIDE
        .with(|c| c.borrow().clone())
        .unwrap_or_else(|| config::USAGE_URL.to_string())
}

#[cfg(not(test))]
fn usage_url() -> String {
    config::USAGE_URL.to_string()
}

pub fn fetch(access_token: &str) -> std::result::Result<Usage, FetchError> {
    let resp = super::http_agent()
        .get(&usage_url())
        // Ask for the `cedar_ember` reset-grant block; the CLI user-agent is
        // what makes the account eligible to see it (see `config::CLI_USER_AGENT`).
        .query("cedar_ember", "1")
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("User-Agent", config::CLI_USER_AGENT)
        .set("Content-Type", "application/json")
        .call();
    match resp {
        Ok(r) => r
            .into_json::<Usage>()
            .map_err(|e| FetchError::Other(format!("parsing usage response: {e}"))),
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Auth),
        Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
        Err(ureq::Error::Status(code, _r)) => {
            // Don't fold the raw response body into the error — it can echo
            // account/request detail and ends up in the debug log (same hygiene
            // rule as oauth::post_token). The status code is enough.
            Err(FetchError::Other(format!(
                "usage endpoint returned HTTP {code}"
            )))
        }
        Err(e) => Err(FetchError::Transient(e.to_string())),
    }
}

/// What the reset-claim endpoint reported. The happy path is `Reset`; the rest
/// are benign "nothing happened" states the UI surfaces verbatim so the user
/// knows why a click did nothing (e.g. the grant was already spent, or the
/// account isn't actually at a limit yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The reset was applied — windows cleared.
    Reset,
    /// A non-success outcome the server named (`already_used`, `not_limited`,
    /// `cooldown`, `ineligible`, `unavailable`, or anything new).
    NoChange(String),
}

impl ClaimOutcome {
    fn from_status(s: &str) -> Self {
        if s == "reset" {
            ClaimOutcome::Reset
        } else {
            ClaimOutcome::NoChange(s.to_string())
        }
    }
}

/// A fresh idempotency id for a claim request. `rand` is already a dependency;
/// a UUID crate would be overkill for a single random token.
fn request_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("req_{hex}")
}

/// Claim a banked usage-limit reset for `org_uuid`'s `grant_id`. POSTs the
/// `cedar_ember` program to the organization's `reset_rate_limits` endpoint and
/// maps the response `status` to a [`ClaimOutcome`]. This SPENDS a one-shot
/// reset — only ever call it from an explicit user action, never automatically.
/// Not gated on `cfg(test)`: no test invokes it (so none hits the network), and
/// gating it out would break the test build of its `main::claim_claude_reset`
/// caller.
pub fn claim_reset(
    access_token: &str,
    org_uuid: &str,
    grant_id: &str,
) -> std::result::Result<ClaimOutcome, FetchError> {
    let body = serde_json::json!({
        "program": config::RESET_PROGRAM,
        "grant_id": grant_id,
        "request_id": request_id(),
    });
    let resp = super::http_agent()
        .post(&config::reset_claim_url(org_uuid))
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("User-Agent", config::CLI_USER_AGENT)
        .set("Content-Type", "application/json")
        .send_json(body);
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing claim response: {e}")))?;
            let status = v
                .get("status")
                .or_else(|| v.get("outcome"))
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            Ok(ClaimOutcome::from_status(status))
        }
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Auth),
        Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
        // Don't fold the response body into the error — same hygiene rule as
        // `fetch` (it can echo account/request detail into the debug log).
        Err(ureq::Error::Status(code, _)) => Err(FetchError::Other(format!(
            "reset claim returned HTTP {code}"
        ))),
        Err(e) => Err(FetchError::Transient(e.to_string())),
    }
}

/// The prepaid wallet balance from a `prepaid/credits` response: `balance.money`
/// when present, else `balance.credits` / top-level `amount` (both minor units)
/// in the top-level `currency`. `None` for any other shape.
pub fn prepaid_balance(body: &serde_json::Value) -> Option<Money> {
    let balance = body.get("balance").filter(|v| v.is_object());
    if let Some(m) = money(get(balance, "money")) {
        return Some(m);
    }
    let unit = body
        .get("currency")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(Money::CREDITS);
    if let Some(c) = get(balance, "credits").and_then(|c| c.as_object()) {
        if let Some(amount) = c.get("amount_minor").and_then(|x| x.as_i64()) {
            let exponent = c.get("exponent").and_then(|x| x.as_u64()).unwrap_or(2);
            return Money::new(amount, u32::try_from(exponent).ok()?, unit);
        }
    }
    let amount = body.get("amount")?;
    let amount = amount
        .as_i64()
        .or_else(|| amount.as_f64().map(|f| f.round() as i64))?;
    Money::new(amount, 2, unit)
}

/// Best-effort prepaid wallet balance for `org_uuid` (see
/// [`config::prepaid_credits_url`]). `None` on any failure — the caller keeps
/// whatever it last knew.
pub fn fetch_prepaid_balance(access_token: &str, org_uuid: &str) -> Option<Money> {
    let body: serde_json::Value = super::http_agent()
        .get(&config::prepaid_credits_url(org_uuid))
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .set("User-Agent", config::CLI_USER_AGENT)
        .call()
        .ok()?
        .into_json()
        .ok()?;
    prepaid_balance(&body)
}

/// Best-effort account email from the profile endpoint.
pub fn fetch_email(access_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Account {
        email: Option<String>,
        email_address: Option<String>,
    }
    #[derive(Deserialize)]
    struct Profile {
        account: Option<Account>,
    }
    let resp = super::http_agent()
        .get(config::PROFILE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .call()
        .ok()?;
    let p: Profile = resp.into_json().ok()?;
    let a = p.account?;
    a.email.or(a.email_address)
}

/// Fetch the raw profile JSON (`account`, `organization`, ...).
pub fn fetch_profile(access_token: &str) -> Option<serde_json::Value> {
    super::http_agent()
        .get(config::PROFILE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", config::OAUTH_BETA)
        .set("anthropic-version", "2023-06-01")
        .call()
        .ok()?
        .into_json()
        .ok()
}

/// Whether a profile response shows a paid Claude plan. The profile endpoint
/// is separate from `/api/oauth/usage` and keeps answering for a lapsed
/// account, so it's the cheap way to both detect a lapse and notice a renewal
/// without spending usage-endpoint requests (and 429s). A lapsed plan shows up as the org
/// dropping to `claude_free` and/or a terminal `subscription_status`
/// (observed: `organization_type: claude_free`, `subscription_status:
/// canceled` after a Max plan expired).
pub fn subscription_active_from_profile(profile: &serde_json::Value) -> Option<bool> {
    let org = profile.get("organization")?.as_object()?;
    let org_type = org.get("organization_type").and_then(|v| v.as_str());
    let status = org.get("subscription_status").and_then(|v| v.as_str());
    let lapsed = org_type == Some("claude_free")
        || matches!(status, Some("canceled" | "unpaid" | "incomplete_expired"));
    Some(!lapsed)
}

/// The organization uuid from a profile response — the `{org}` segment the
/// reset-claim endpoint needs. `None` if the profile has no organization.
pub fn org_uuid_from_profile(profile: &serde_json::Value) -> Option<String> {
    profile
        .get("organization")?
        .get("uuid")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// What the user bought: Anthropic's own plan name ("Ultra") when the profile
/// carries one, else mapped from the org type / rate-limit tier.
pub fn plan_label(
    display_name: Option<&str>,
    org_type: Option<&str>,
    tier: Option<&str>,
) -> Option<String> {
    if let Some(name) = display_name.map(str::trim).filter(|n| !n.is_empty()) {
        return Some(name.to_string());
    }
    let label = match org_type? {
        "claude_max" => match tier.unwrap_or("") {
            t if t.ends_with("_20x") => "Max 20x",
            t if t.ends_with("_5x") => "Max 5x",
            _ => "Max",
        },
        "claude_pro" => "Pro",
        "claude_team" => "Team",
        "claude_enterprise" => "Enterprise",
        "claude_free" => "Free",
        _ => return None,
    };
    Some(label.to_string())
}

/// Plan label + lapsed/active from a profile response.
pub fn plan_status_from_profile(
    profile: &serde_json::Value,
) -> Option<crate::providers::trait_def::PlanStatus> {
    let active = subscription_active_from_profile(profile)?;
    let org = profile.get("organization");
    let get = |k: &str| org.and_then(|o| o.get(k)).and_then(|v| v.as_str());
    let label = if active {
        plan_label(
            get("plan_display_name"),
            get("organization_type"),
            get("rate_limit_tier"),
        )
    } else {
        Some("Free".to_string())
    };
    Some(crate::providers::trait_def::PlanStatus { label, active })
}

/// Build an `oauthAccount` object (the shape Claude Code stores in
/// `~/.claude.json`) from a profile response. Used to backfill the identity for
/// accounts captured before we started snapshotting it. Claude refreshes the
/// remaining fields (e.g. `profileFetchedAt`) on its next profile fetch.
pub fn oauth_account_from_profile(profile: &serde_json::Value) -> Option<serde_json::Value> {
    let acct = profile.get("account")?;
    let org = profile.get("organization");
    let get = |v: Option<&serde_json::Value>, k: &str| {
        v.and_then(|o| o.get(k))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    Some(serde_json::json!({
        "accountUuid": get(Some(acct), "uuid"),
        "emailAddress": get(Some(acct), "email"),
        "displayName": get(Some(acct), "display_name"),
        "fullName": get(Some(acct), "full_name"),
        "accountCreatedAt": get(Some(acct), "created_at"),
        "organizationUuid": get(org, "uuid"),
        "organizationName": get(org, "name"),
        "organizationType": get(org, "organization_type"),
        "organizationRateLimitTier": get(org, "rate_limit_tier"),
        "planDisplayName": get(org, "plan_display_name"),
        "billingType": get(org, "billing_type"),
        "hasExtraUsageEnabled": get(org, "has_extra_usage_enabled"),
        "subscriptionCreatedAt": get(org, "subscription_created_at"),
    }))
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
