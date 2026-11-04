//! `usagio list`: the cached-usage table.

use anyhow::Result;
use chrono::Utc;

use crate::display::{age_str, bar, truncate};
use crate::picking::menu_order;
use crate::poll::claude::refresh_usage_cache;
use crate::poll::plan::SUBSCRIPTION_RECHECK_SECS;
use crate::poll::providers::refresh_provider_usage_caches_with;
use crate::providers::CLAUDE_SLUG;
use crate::rows::{row_from_account, row_from_provider_account, Row};
use crate::store::State;
use crate::watch::cadence::TRIGGER_PCT;
use crate::{claude_profiles, providers};

// ---------------------------------------------------------------------------
// list (default) — dashboard
// ---------------------------------------------------------------------------

pub(crate) fn cmd_list(args: &[String]) -> Result<()> {
    let refresh = args.iter().any(|a| a == "--refresh" || a == "-r");
    // By default read the cache (no network); --refresh does exactly one fetch.
    if refresh {
        providers::init();
        refresh_usage_cache(true);
        claude_profiles::refresh(usize::MAX, true);
        let trigger = State::load()
            .ok()
            .and_then(|s| s.trigger_pct)
            .unwrap_or(TRIGGER_PCT);
        refresh_provider_usage_caches_with(true, trigger, &std::collections::HashSet::new());
    }
    providers::init();
    let state = State::load()?;
    let no_claude = state.accounts.is_empty();
    let no_providers = state.providers.values().all(|p| p.accounts.is_empty());
    if no_claude && no_providers {
        println!(
            "No accounts yet. Log into one with `claude` (or `codex`), then: usagio capture \
             (or `usagio capture codex`)"
        );
        return Ok(());
    }

    // Claude accounts (state v1 `accounts` bucket). Ordered by `menu_order`,
    // the same rule the menu bar uses, so `usagio list` and the menu agree on
    // account order (user report: "account ordering seems off").
    if !no_claude {
        let mut rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
        let now = Utc::now();
        rows.sort_by(|a, b| menu_order(a, b, state.active.as_deref(), now));
        render_table(&rows, state.active.as_deref());
    }

    // Every other registered provider's captured accounts (state v2
    // `providers[slug]` slots), one section per provider in `providers::all()`
    // order — mirroring the menu-bar's per-provider sections so `usagio list`
    // and the menu agree (the CLI used to be Claude-only, so a captured Codex
    // account showed in the menu but never in `list`). A provider's own
    // `active` key marks its active row (keys can collide across providers —
    // e.g. the same email is both a Claude account and a Codex key — so each
    // section must be marked independently, not by a single global active).
    for provider in providers::all() {
        let slug = provider.provider_id();
        if slug == CLAUDE_SLUG {
            continue;
        }
        let Some(pa) = state.providers.get(slug) else {
            continue;
        };
        if pa.accounts.is_empty() {
            continue;
        }
        let mut rows: Vec<Row> = pa
            .accounts
            .iter()
            .map(|a| row_from_provider_account(slug, a))
            .collect();
        let now = Utc::now();
        rows.sort_by(|a, b| menu_order(a, b, pa.active.as_deref(), now));
        println!("\n{}:", provider.display_name());
        render_table(&rows, pa.active.as_deref());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub(crate) fn render_table(rows: &[Row], active: Option<&str>) {
    let has_opus = rows.iter().any(|r| r.opus.is_some());
    println!();
    let mut header = format!(
        "{:<2} {:<28} {:<22} {:<11}",
        "", "ACCOUNT", "SESSION (5h)", "RESETS IN"
    );
    header.push_str(&format!("  {:<22} {:<11}", "WEEKLY (7d)", "RESETS IN"));
    if has_opus {
        header.push_str(&format!("  {:<22} {:<11}", "WEEKLY OPUS", "RESETS IN"));
    }
    header.push_str(&format!("  {:<10}", "UPDATED"));
    println!("{header}");
    println!("{}", "-".repeat(header.len()));

    for r in rows {
        let label = providers::trait_def::account_label(
            &r.email,
            rows.iter()
                .filter(|o| o.provider_id == r.provider_id)
                .map(|o| o.email.as_str()),
        );
        let marker = if active == Some(r.email.as_str()) {
            "▶"
        } else {
            " "
        };
        if r.no_subscription {
            println!(
                "{marker}  {:<28} no subscription (Free plan) — usage paused; re-checked every {}m",
                truncate(&label, 28),
                SUBSCRIPTION_RECHECK_SECS / 60,
            );
            continue;
        }
        if !r.has_data() {
            println!(
                "{marker}  {:<28} no data yet (run the menu-bar app or `usagio watch`)",
                truncate(&label, 28),
            );
            continue;
        }
        let mut line = format!(
            "{marker}  {:<28} {:<22} {:<11}",
            truncate(&label, 28),
            bar(r.session.pct),
            r.session.resets_in(),
        );
        line.push_str(&format!(
            "  {:<22} {:<11}",
            bar(r.weekly.pct),
            r.weekly.resets_in()
        ));
        if has_opus {
            match &r.opus {
                Some(c) => line.push_str(&format!("  {:<22} {:<11}", bar(c.pct), c.resets_in())),
                None => line.push_str(&format!("  {:<22} {:<11}", "-", "")),
            }
        }
        line.push_str(&format!("  {:<10}", age_str(r.fetched_at)));
        println!("{line}");
    }
    println!();
    if active.is_none() {
        println!("(no active account tracked yet — `capture` the one you're on)");
    }
    println!(
        "Usage updates on a schedule (menu-bar app / `usagio watch`). \
         Run `usagio list --refresh` to fetch now.\n"
    );
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
