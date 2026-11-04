//! The menu snapshot: per-provider sections and account views built from state.

use super::binding::cached_rates;
use crate::countdown::{AccountUsage, BlockingWindow, DisplayState};
use crate::display::age_str;
use crate::env_override::env_override_active;
use crate::providers::trait_def::ReportedUsage;
use crate::providers::{CaptureMode, Provider, SeverityBands, CLAUDE_SLUG};
use crate::rows::{row_from_account, row_from_provider_account, Row};
use crate::store::State;
use crate::watch::cadence::TRIGGER_PCT;
use crate::{countdown, providers};
use chrono::{DateTime, Utc};

/// One rate-limit / quota window as displayed under an account's submenu. `id`
/// is the provider's own window slug ("session"/"weekly"/"opus" for Claude);
/// `label` is the short human string ("5h"/"7d"/"Opus 7d").
#[derive(Clone)]
pub(super) struct WindowView {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) pct: Option<f64>,
    /// Human "resets in X" text, or empty when unknown.
    pub(super) reset: String,
}

/// One account as shown in the menu. Windows are ordered by
/// `provider.window_order()`; `has_data` gates the stat-row block.
pub(super) struct AcctView {
    pub(super) provider_id: &'static str,
    /// Stable key used inside click ids (`switch:claude:<key>` etc). In v1
    /// this is the account's email; later phases key on the provider's
    /// `account_identifier`.
    pub(super) key: String,
    /// Human-facing label rendered in the submenu title.
    pub(super) display: String,
    pub(super) windows: Vec<WindowView>,
    pub(super) updated: String,
    pub(super) active: bool,
    pub(super) has_data: bool,
    /// Absolute reset instants for the first two windows, threaded through so
    /// `countdown::compute_display` can decide whether the row is "locked"
    /// (session/weekly ≥99.5% and reset still in the future). The strings in
    /// `WindowView.reset` are for display only — the raw `DateTime` is needed
    /// to reason about the future.
    pub(super) session_reset_at: Option<DateTime<Utc>>,
    pub(super) weekly_reset_at: Option<DateTime<Utc>>,
    /// When this account's cached usage was fetched. Threaded through to
    /// `countdown::compute_display` so it can distinguish a genuinely fresh
    /// post-reset reading from a stale pre-reset one still sitting in the
    /// cache (`DisplayState::StaleAfterReset` — see `trailing_for_account`).
    pub(super) fetched_at: Option<DateTime<Utc>>,
    /// The OAuth token endpoint has permanently rejected this account's refresh
    /// token — it's filtered out of every usage refresh, so its cached numbers
    /// go stale forever. Takes priority over any usage/lock state in the row
    /// trailing and submenu so the user sees "fix your login", not a misleading
    /// "-% / -%" placeholder that reads like a transient poll gap.
    pub(super) needs_relogin: bool,
    /// Last non-fatal error recorded for this account's refresh, if any. Shown
    /// as the reason line under a `⚠ error` trailing when `needs_relogin` is not
    /// set. `None` in the common healthy case.
    pub(super) error: Option<String>,
    /// The account's subscription lapsed: its cached usage (and reset
    /// countdowns) are meaningless, so the row shows a red "-" and the
    /// submenu explains instead.
    pub(super) no_subscription: bool,
    /// Plan label ("Max 20x", "Pro", "Free", …) when known.
    pub(super) plan: Option<String>,
    /// Credits / breakdown / scoped limits the provider reported, rendered by
    /// [`reported_rows`]. Empty when the provider reported nothing.
    pub(super) reported: ReportedUsage,
    /// When this account's login itself dies (its refresh token's expiry),
    /// if known. Within [`crate::login::RENEW_WINDOW_SECS`] the row asks for a
    /// renewal.
    pub(super) login_expires_at: Option<DateTime<Utc>>,
    /// usagio can run this provider's login itself ("Renew login…").
    pub(super) renewable: bool,
    /// The account's own burn rates (auto-swap v2's rate model), for the
    /// binding-window display. Filled for active accounts only.
    pub(super) rate: crate::autoswap::AccountRate,
    /// The auto-swap trigger: a window at or over it binds the display.
    pub(super) trigger: f64,
}

/// One provider's block in the menu. Rendered only if `accounts` is non-empty
/// (per the "section renders only when the provider has at least one captured
/// account" rule).
pub(crate) struct ProviderSection {
    pub(super) provider_id: &'static str,
    pub(super) display_name: &'static str,
    pub(super) supports_switching: bool,
    /// Gates the "Remove…" row. Defaults `true` for
    /// every current provider — removing a captured account is a generic
    /// state.json operation — but stays explicit so a provider needing extra
    /// cleanup can opt out until that's wired.
    pub(super) supports_remove: bool,
    pub(super) supports_usage: bool,
    pub(super) severity_bands: SeverityBands,
    /// True when the provider's OAuth env-override is set on this process's
    /// environment. Surfaced as a disabled row inside the section and used by
    /// `watch_cycle` to skip that provider entirely.
    pub(super) env_override_active: bool,
    pub(super) accounts: Vec<AcctView>,
}

/// One row directly under "Capture current login ▸". Every row stays
/// clickable so errors surface honestly. `capture_mode` decides the click-id prefix
/// (`capture:` vs `apikey:`) that routes to the right handler in
/// `handle_click`, and orders creds-on-disk providers before API-key ones.
pub(super) struct RegisteredProvider {
    pub(super) provider_id: &'static str,
    pub(super) display_name: &'static str,
    pub(super) capture_mode: CaptureMode,
}

/// Everything the UI needs to render, produced by the poller thread.
#[derive(Default)]
pub(super) struct Snapshot {
    pub(super) sections: Vec<ProviderSection>,
    /// Render order for the flat main-list block sequence: `(section index,
    /// account index within that section)` pairs, spanning ALL providers —
    /// see `flat_account_order`'s doc for the sort. `menu_tree_from_snapshot`
    /// iterates this instead of nesting
    /// `for sec in sections { for a in sec.accounts }`, which used to
    /// silently re-impose provider-declaration order on top of any
    /// per-account priority (the "account order keeps changing" report).
    pub(super) account_order: Vec<(usize, usize)>,
    /// Providers whose `capture_mode == CredsOnDisk` and `supports_usage == true`:
    /// rendered directly under "Capture current login ▸".
    pub(super) capture_creds: Vec<RegisteredProvider>,
    /// Providers whose `capture_mode == ApiKey` and `supports_usage == true`:
    /// rendered directly under "Capture current login ▸", after
    /// `capture_creds`. API-key providers render as direct children of
    /// "Capture current login ▸", same as creds-on-disk ones.
    pub(super) capture_api_key: Vec<RegisteredProvider>,
    pub(super) autoswap: bool,
    pub(super) threshold: f64,
    /// Settings ▸ Auto-swap ▸ "Use credits when every plan is used up".
    pub(super) use_credits: bool,
    /// Settings ▸ Notifications ▸ per-trigger enable checkboxes.
    pub(super) notification_config: crate::notifications::NotificationConfig,
    /// Settings ▸ Tray Icon: `None` = "at risk" (the agent closest to its
    /// limit); `Some(slug)` pins the tray to one provider. Drives
    /// `tray_target` / `title_for` / `tray_icon_bytes`.
    pub(super) tray_icon_mode: Option<String>,
}

/// How near a limit a percentage is, for at-a-glance coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Severity {
    /// >= amber band: approaching the wall.
    Amber,
    /// >= red band: about to hit it.
    Red,
}

/// Map a utilization percentage to a color band given the provider's bands.
pub(super) fn severity_with(p: Option<f64>, bands: SeverityBands) -> Option<Severity> {
    match p {
        Some(v) if v >= bands.red => Some(Severity::Red),
        Some(v) if v >= bands.amber => Some(Severity::Amber),
        _ => None,
    }
}

/// The first two windows of an account, used to build the two-percentage
/// summary text in the header rows. `(pct_a, pct_b)` — either may be `None`.
pub(super) fn summary_pcts(a: &AcctView) -> (Option<f64>, Option<f64>) {
    let a0 = a.windows.first().and_then(|w| w.pct);
    let a1 = a.windows.get(1).and_then(|w| w.pct);
    (a0, a1)
}

/// The account reports a weekly window but no session window (Claude Ultra,
/// Codex), so the weekly pct is its only number.
pub(super) fn weekly_only(a: &AcctView) -> bool {
    matches!(summary_pcts(a), (None, Some(_)))
}

/// Fold an `AcctView` into the shape `countdown::compute_display` expects —
/// pcts from the first two windows plus the raw reset instants we captured off
/// the row. Shared by account-row and provider recovery countdowns.
pub(super) fn account_usage_for(a: &AcctView) -> AccountUsage {
    let (sp, wp) = summary_pcts(a);
    AccountUsage {
        session_pct: sp,
        session_reset: a.session_reset_at,
        weekly_pct: wp,
        weekly_reset: a.weekly_reset_at,
        fetched_at: a.fetched_at,
    }
}

/// If the account is "locked" (session or weekly at ≥99.5% with a
/// still-future reset), return the human countdown string — the piece that
/// swaps in for `S% / W%` in the row title. `None` otherwise (including the
/// `StaleAfterReset` case — a stale-but-past-reset cache isn't "locked", it's
/// just waiting on a fresh poll; see `trailing_for_account` for how that
/// state renders instead).
pub(super) fn locked_countdown_for(
    a: &AcctView,
    now: DateTime<Utc>,
) -> Option<(String, BlockingWindow)> {
    match countdown::compute_display(&account_usage_for(a), now) {
        DisplayState::Locked { until, window } => {
            Some((countdown::format_countdown(until - now), window))
        }
        DisplayState::Usage { .. } | DisplayState::StaleAfterReset { .. } => None,
    }
}

/// Wall-clock "now" used by the render helpers. Overridable in tests so a
/// pinned locked-row can be asserted without waiting real hours.
#[cfg(not(test))]
pub(super) fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
pub(super) fn now_utc() -> DateTime<Utc> {
    super::test_support::test_now()
}

/// Locate the active section + account (if any) in the snapshot.
#[cfg(test)]
pub(super) fn active_account(snap: &Snapshot) -> Option<(&ProviderSection, &AcctView)> {
    for sec in &snap.sections {
        if let Some(a) = sec.accounts.iter().find(|a| a.active) {
            return Some((sec, a));
        }
    }
    None
}

/// The highest window percentage across an account's windows (how close it is
/// to *any* of its limits) — the "at risk" score. `None` when the account has
/// no usage data yet (sorted below any real reading).
pub(super) fn acct_max_pct(a: &AcctView) -> Option<f64> {
    a.windows
        .iter()
        .filter_map(|w| w.pct)
        .fold(None, |acc, p| Some(acc.map_or(p, |m: f64| m.max(p))))
}

/// Highest of the session/weekly percentages only — the windows
/// `compute_display` locks on. Unlike [`acct_max_pct`] it ignores scoped
/// windows (e.g. Opus), which can be maxed while the account stays usable.
pub(super) fn blocking_pct(a: &AcctView) -> Option<f64> {
    let (s, w) = summary_pcts(a);
    match (s, w) {
        (Some(s), Some(w)) => Some(s.max(w)),
        (s, w) => s.or(w),
    }
}

/// Whether the provider currently under an env-override that would defeat any
/// switch we make. Delegates to the shared `crate::env_override::env_override_active` so
/// the menu display and the `choose_swap_target` auto-swap filter can never
/// disagree, and so tests can toggle overrides through one hook.
pub(super) fn env_override_for(provider_id: &str) -> bool {
    env_override_active(provider_id)
}

/// Shared display priority: usable accounts by weekly reset, locked accounts
/// by unlock time, then headroom and key. Lapsed subscriptions come last.
/// Active status is only an indicator. Auto-swap uses its separate runway policy.
pub(super) fn account_priority_cmp(
    a: &AcctView,
    b: &AcctView,
    now: DateTime<Utc>,
) -> std::cmp::Ordering {
    let priority = |x: &AcctView| {
        let (sp, wp) = summary_pcts(x);
        crate::account_order::Priority::new(
            x.no_subscription,
            &account_usage_for(x),
            100.0 - sp.unwrap_or(0.0).max(wp.unwrap_or(0.0)),
            now,
        )
    };
    priority(a).cmp(&priority(b)).then(a.key.cmp(&b.key))
}

/// Global priority order for the flat main-list block sequence, spanning ALL
/// providers — not grouped by provider (v0.5.2 fix for the "account order keeps
/// changing" report: `sections` were previously rendered in fixed
/// `providers::all()` order with only a per-section re-sort, so an account's
/// position could jump around relative to another provider's account for no
/// reason a user could predict). Delegates to [`account_priority_cmp`] — the
/// SAME comparator `build_snapshot` sorts each section's `accounts` vec with —
/// so `account_order` and the section vecs can never disagree (v0.5.18
/// "1 order only"). `provider_grouped_order` later brackets this flat order by
/// provider (alphabetically) for rendering.
pub(super) fn flat_account_order(
    sections: &[ProviderSection],
    now: DateTime<Utc>,
) -> Vec<(usize, usize)> {
    let mut idx: Vec<(usize, usize)> = Vec::new();
    for (si, sec) in sections.iter().enumerate() {
        for ai in 0..sec.accounts.len() {
            idx.push((si, ai));
        }
    }
    idx.sort_by(|&(sx, ax), &(sy, ay)| {
        account_priority_cmp(&sections[sx].accounts[ax], &sections[sy].accounts[ay], now)
    });
    idx
}

/// Partition `snap.account_order` by provider,
/// preserving the within-provider order the global comparator produced, then
/// sort provider groups alphabetically by display name so a two-provider menu
/// always reads Claude → Codex regardless of `providers::all()` registration
/// order. The `account_order` global sort still decides who comes first WITHIN
/// a provider (soonest-reset first, locked accounts sink, etc.) — this
/// grouping just brackets those runs by provider so each block sits under one
/// bold `provider_group_header_row`.
pub(super) fn provider_grouped_order(snap: &Snapshot) -> Vec<(usize, Vec<usize>)> {
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for &(si, ai) in &snap.account_order {
        if let Some(entry) = groups.iter_mut().find(|(s, _)| *s == si) {
            entry.1.push(ai);
        } else {
            groups.push((si, vec![ai]));
        }
    }
    groups.sort_by_key(|(si, _)| snap.sections[*si].display_name);
    groups
}

/// Build one account's rendered view from a v1 `Row`. v1 state only has
/// Claude accounts, so the window set is fixed (session / weekly / opus); the
/// window IDs come from `provider.window_order()` so this is trivially
/// generalizable when state v2 lands.
pub(super) fn acctview_from_row(
    r: &Row,
    active: &Option<String>,
    provider_id: &'static str,
    window_order: &'static [&'static str],
) -> AcctView {
    // Build the fixed pool of windows we know about for this v1 row.
    let mut pool: Vec<WindowView> = Vec::new();
    pool.push(WindowView {
        id: "session".into(),
        label: "5h".into(),
        pct: r.session.pct,
        reset: r.session.resets_in(),
    });
    pool.push(WindowView {
        id: "weekly".into(),
        label: "7d".into(),
        pct: r.weekly.pct,
        reset: r.weekly.resets_in(),
    });
    if let Some(c) = &r.opus {
        pool.push(WindowView {
            id: "opus".into(),
            label: "Opus 7d".into(),
            pct: c.pct,
            reset: c.resets_in(),
        });
    }
    // Present them in the provider's declared order; unknown IDs (a
    // provider added in later phases) fall through to their pool order.
    let mut windows: Vec<WindowView> = Vec::with_capacity(pool.len());
    for id in window_order {
        if let Some(pos) = pool.iter().position(|w| w.id == *id) {
            windows.push(pool.remove(pos));
        }
    }
    windows.extend(pool);

    AcctView {
        provider_id,
        key: r.email.clone(),
        display: r.email.clone(),
        windows,
        updated: age_str(r.fetched_at),
        active: active.as_deref() == Some(r.email.as_str()),
        has_data: r.has_data(),
        session_reset_at: r.session.resets_at,
        weekly_reset_at: r.weekly.resets_at,
        fetched_at: r
            .fetched_at
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
        needs_relogin: r.needs_relogin,
        error: r.error.clone(),
        no_subscription: r.no_subscription,
        plan: r.plan.clone(),
        reported: r.reported.clone(),
        // Filled in by `build_snapshot`, which has the stored credential.
        login_expires_at: None,
        renewable: false,
        rate: Default::default(),
        trigger: TRIGGER_PCT,
    }
}

/// Build the UI snapshot from local State only (no network), reading each
/// account's cached usage. Runs every 0.75s UI tick, so it avoids re-reading
/// state.json unless its mtime changed and probes the Login Item at most once a
/// minute (both were per-tick subprocess/disk costs before).
///
/// A provider's section is only emitted if at least one captured account exists
/// for it in state — that's the "no header, no rows" rule from the design.
/// The "Capture current login" submenu is separately fed from
/// `providers::all()` so every REGISTERED provider stays listable there.
pub(super) fn build_snapshot() -> Snapshot {
    let st = cached_state();
    let autoswap = !st.autoswap_disabled;
    let threshold = st.trigger_pct.unwrap_or(TRIGGER_PCT);
    let active = st.active.clone();
    // One `now` for every ordering decision this snapshot makes, so the
    // per-section sort and the flat `account_order` agree to the instant.
    let now = now_utc();

    // Claude's accounts live in `st.accounts` (its own dedicated slot); every
    // other provider's captured accounts live in `st.providers[slug]` (state
    // v2 — see `store.rs`'s `State` doc). Build one flat `Row` list tagged by
    // `provider_id` from both.
    let mut rows: Vec<Row> = st.accounts.iter().map(row_from_account).collect();
    for (slug, pa) in &st.providers {
        rows.extend(
            pa.accounts
                .iter()
                .map(|a| row_from_provider_account(slug, a)),
        );
    }

    let mut sections: Vec<ProviderSection> = Vec::new();
    for provider in providers::all() {
        let slug = provider.provider_id();
        let provider_rows: Vec<&Row> = rows.iter().filter(|r| r.provider_id == slug).collect();
        if provider_rows.is_empty() {
            continue; // no captured accounts → no section (no header, no rows).
        }
        // Claude's "active" selection is `st.active`; every other provider
        // tracks its own active key in `st.providers[slug].active` (state
        // v2), since a provider's captured accounts are independent of
        // Claude's.
        let active_for_slug = if slug == CLAUDE_SLUG {
            active.clone()
        } else {
            st.providers.get(slug).and_then(|p| p.active.clone())
        };
        let keys: Vec<&str> = provider_rows.iter().map(|r| r.email.as_str()).collect();
        let mut accounts: Vec<AcctView> = provider_rows
            .iter()
            .map(|r| {
                let mut view =
                    acctview_from_row(r, &active_for_slug, slug, provider.window_order());
                view.display =
                    crate::providers::trait_def::account_label(&r.email, keys.iter().copied());
                view
            })
            .collect();
        // The ONE canonical order (v0.5.18 "1 order only"): soonest WEEKLY
        // reset first (use credits top-down so no weekly is left behind),
        // locked accounts follow in unlock order, headroom then email break ties. NOT
        // active-first — the active account ranks on its own priority. This is
        // the exact comparator `flat_account_order` uses, so this section vec
        // and the rendered `account_order` are always identical.
        accounts.sort_by(|a, b| account_priority_cmp(a, b, now));
        let renewable = provider.login_spec().is_some();
        for a in &mut accounts {
            if a.active && a.has_data {
                a.rate = cached_rates(slug, &a.key, now);
            }
            a.trigger = threshold;
            a.renewable = renewable;
            if slug == CLAUDE_SLUG {
                a.login_expires_at = st
                    .find(&a.key)
                    .and_then(|x| x.refresh_token_expires_at())
                    .and_then(DateTime::from_timestamp_millis);
            }
        }
        let caps = provider.capabilities();
        sections.push(ProviderSection {
            provider_id: slug,
            display_name: provider.display_name(),
            supports_switching: caps.supports_switching,
            supports_remove: caps.supports_remove,
            supports_usage: caps.supports_usage,
            severity_bands: provider.severity_bands(),
            env_override_active: env_override_for(slug),
            accounts,
        });
    }

    // The capture submenu lists every registered provider that CAN capture a
    // login on this host — filtered by `capture_menu_providers` so stub
    // providers (`supports_usage == false`) don't clutter the onboarding UX.
    // `capture_creds`/`capture_api_key` still separate creds-on-disk from
    // API-key providers (the click-id prefix differs), but v0.5.2 item 8
    // flattened their RENDERING into one direct child list under "Capture
    // current login ▸" (creds first, then API-key) instead of an API-key-only
    // "Paste API key ▸" sub-submenu. An API-key provider stops appearing here
    // once it has a captured account (i.e. a section above) — that account now
    // renders in the main list instead (item 9 of the original redesign).
    // Creds-on-disk providers stay listed so a second account can be added.
    let captured_provider_ids: Vec<&str> = sections.iter().map(|s| s.provider_id).collect();
    let (creds_providers, api_key_providers) = capture_menu_providers(&captured_provider_ids);
    let capture_creds: Vec<RegisteredProvider> =
        creds_providers.into_iter().map(register_provider).collect();
    let capture_api_key: Vec<RegisteredProvider> = api_key_providers
        .into_iter()
        .map(register_provider)
        .collect();

    let account_order = flat_account_order(&sections, now);

    Snapshot {
        sections,
        account_order,
        capture_creds,
        capture_api_key,
        autoswap,
        threshold,
        use_credits: st.use_credits_when_exhausted,
        notification_config: st.notification_config.clone(),
        tray_icon_mode: st.tray_icon_mode.clone(),
    }
}

/// Build one `RegisteredProvider` row from a live provider reference.
/// Extracted so both buckets in `build_snapshot` populate identically.
pub(super) fn register_provider(p: &'static dyn Provider) -> RegisteredProvider {
    RegisteredProvider {
        provider_id: p.provider_id(),
        display_name: p.display_name(),
        capture_mode: p.capabilities().capture_mode,
    }
}

/// Pick the providers the "Capture current login ▸" menu should offer.
/// Providers with `supports_usage == false` are stubs that have no capture
/// path wired yet (their `capture_current_login` returns `Ok(None)` with a
/// `TODO`) — hiding them keeps the onboarding UX free of dead rows.
///
/// Returns `(creds_on_disk_providers, api_key_providers)`:
/// - the first bucket renders directly under "Capture current login ▸";
/// - the second renders under a "Paste API key ▸" sub-submenu.
///
/// `captured_provider_ids` excludes an API-key provider from the "Paste API
/// key ▸" bucket once it already has a captured account (item 9): that
/// account now renders in the main list instead, and the capture row would
/// just be a confusing duplicate. Creds-on-disk providers (Claude, Codex) are
/// NOT filtered this way — those support multiple captured accounts per
/// provider, so "Capture current login ▸ Claude" must keep working even with
/// an existing Claude account.
///
/// Pure function given its inputs: no I/O, no direct state reads — the caller
/// (`build_snapshot`) is the one that turns `state.json` into
/// `captured_provider_ids`.
pub(crate) fn capture_menu_providers(
    captured_provider_ids: &[&str],
) -> (Vec<&'static dyn Provider>, Vec<&'static dyn Provider>) {
    partition_capture_providers(providers::all(), captured_provider_ids)
}

/// Inner form of `capture_menu_providers` that operates on an arbitrary
/// provider slice so tests can pass their own fixtures without touching the
/// process-wide registry.
pub(super) fn partition_capture_providers<'a>(
    provs: &'a [Box<dyn Provider>],
    captured_provider_ids: &[&str],
) -> (Vec<&'a dyn Provider>, Vec<&'a dyn Provider>) {
    let mut creds: Vec<&'a dyn Provider> = Vec::new();
    let mut api_key: Vec<&'a dyn Provider> = Vec::new();
    for p in provs {
        let caps = p.capabilities();
        if !caps.supports_usage {
            // Stub provider — no capture path wired yet; hide from the menu.
            continue;
        }
        match caps.capture_mode {
            CaptureMode::CredsOnDisk => creds.push(&**p),
            CaptureMode::ApiKey => {
                if !captured_provider_ids.contains(&p.provider_id()) {
                    api_key.push(&**p);
                }
            }
        }
    }
    (creds, api_key)
}

/// Identity of one version of state.json: mtime, length and (on unix) inode.
/// Every save is a tmp+rename, so a new inode tells two saves apart even when
/// a coarse filesystem clock gives them the same mtime.
pub(super) type StateFileKey = Option<(std::time::SystemTime, u64, u64)>;

pub(super) fn state_file_key(path: &std::path::Path) -> StateFileKey {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((m.modified().ok()?, m.len(), ino))
}

/// Return the parsed state, re-reading state.json only when the file changed
/// (see [`state_file_key`]). The 0.75s UI tick would otherwise read+parse the
/// file every tick forever. Called from the main thread, the poller thread
/// (`next_reset_wake_secs` -> `build_snapshot`) and, on Linux/Windows, the
/// `redraw_loop` thread, so the Mutex is required.
pub(super) fn cached_state() -> State {
    use std::sync::{Mutex, OnceLock};
    struct C {
        key: StateFileKey,
        state: State,
        loaded: bool,
    }
    static CELL: OnceLock<Mutex<C>> = OnceLock::new();
    let cell = CELL.get_or_init(|| {
        Mutex::new(C {
            key: None,
            state: State::default(),
            loaded: false,
        })
    });
    let path = crate::store::config_dir()
        .map(|d| d.join("state.json"))
        .unwrap_or_default();
    let key = state_file_key(&path);
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if !g.loaded || g.key != key {
        g.state = State::load().unwrap_or_default();
        g.key = key;
        g.loaded = true;
    }
    g.state.clone()
}

// The old "Backup Config ▸ Restore from backup ▸ <list>" submenu (and its
// `cached_backups()` mtime-cached directory listing) was replaced in v0.5.0
// by native Save…/Restore… file panels under Settings ▸ Advanced ▸ Backups ▸
// (see `handle_backup_save` / `handle_backup_restore_dialog`). The automatic
// rolling backups `store::list_backups()` enumerates still exist on disk —
// `handle_backup_restore_dialog` just defaults the OPEN panel's directory to
// them instead of rendering a menu row per file.

// (Login-item probe cache removed in 0.4.3 alongside the osascript path —
// see the comment on the removed "Launch at login" menu row for rationale.
// `usagio install` / `usagio uninstall` are the sole autostart entry
// points now, backed by a launchd LaunchAgent plist.)

// ---------------------------------------------------------------------------
// Menu building
// ---------------------------------------------------------------------------

/// Whether the window `id` of `a` is `StaleAfterReset`: still at/above the lock
/// threshold in the cache, but its reset passed and the reading predates it.
pub(super) fn window_is_stale_after_reset(a: &AcctView, id: &str, now: DateTime<Utc>) -> bool {
    let mut u = account_usage_for(a);
    match id {
        "session" => {
            u.weekly_pct = None;
            u.weekly_reset = None;
        }
        "weekly" => {
            u.session_pct = None;
            u.session_reset = None;
        }
        _ => return false,
    }
    matches!(
        countdown::compute_display(&u, now),
        DisplayState::StaleAfterReset { .. }
    )
}

/// Whether Settings > Tray Icon should check "At risk".
pub(super) fn at_risk_checked(snap: &Snapshot) -> bool {
    // A pin whose provider has no captured account falls back to at-risk
    // (see `tray_target`), so the menu must show that.
    match snap.tray_icon_mode.as_deref() {
        None => true,
        Some(slug) => !snap.sections.iter().any(|s| s.provider_id == slug),
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
