//! `usagio switch` / `start` / `continue`.

use anyhow::{anyhow, bail, Result};

use crate::picking::auto_pick_for;
use crate::providers::require;
use crate::providers::{Provider, CLAUDE_SLUG};
use crate::rows::provider_rows;
use crate::selectors::{
    ambiguous_provider_selector, provider_selector_matches, resolve_provider_selector, select_email,
};
use crate::store::State;
use crate::switching::claude::switch_to;
use crate::switching::provider::{
    provider_by_slug, provider_supports_swap, queued_provider_switch, switch_provider_expecting,
    switch_to_provider_account, try_provider_switch_pending,
};
use crate::watch::cadence::TRIGGER_PCT;
use crate::{providers, store};

#[derive(Clone, Copy)]
pub(crate) enum Launch {
    Fresh,
    Continue,
}

// ---------------------------------------------------------------------------
// switch / start / continue
// ---------------------------------------------------------------------------

pub(crate) fn cmd_switch(selector: Option<&str>, launch: Option<Launch>) -> Result<()> {
    let state = State::load()?;
    if state.accounts.is_empty() && state.providers.values().all(|p| p.accounts.is_empty()) {
        bail!("no accounts yet; capture one with: usagio capture");
    }

    // Explicit selector: try Claude's accounts first (unchanged v1 behavior,
    // including `start`/`continue` launching `claude` afterward). If it
    // doesn't resolve there, fall back to a provider-owned account slot
    // (state v2's `State::providers`) — this is the CLI half of routing a
    // switch by provider slug instead of assuming Claude, matching
    // `menubar.rs::handle_switch`.
    // An explicit pick under the trigger stays until the trigger (no
    // proactive flip-back), then auto-swaps as usual. A pick already at/over
    // the trigger holds until it's exhausted (see `State::manual_holds`).
    if let Some(sel) = selector {
        if let Some((slug, account)) = sel.split_once(':') {
            let slug = slug.to_ascii_lowercase();
            provider_by_slug(&slug)?;
            if slug == CLAUDE_SLUG {
                let email = select_email(&state, (!account.is_empty()).then_some(account))?;
                return finish_claude_switch(&email, launch, !account.is_empty());
            }
            let key = if account.is_empty() {
                let trigger = state.trigger_pct.unwrap_or(TRIGGER_PCT);
                auto_pick_for(&state, &provider_rows(&state, &slug), trigger, trigger)?
            } else {
                resolve_provider_selector(&state, sel)
                    .map(|(_, key)| key)
                    .ok_or_else(|| anyhow!("no unique {slug} account matches '{account}'"))?
            };
            return finish_provider_switch(&slug, &key, launch, !account.is_empty());
        }
        let claude = (!state.accounts.is_empty()).then(|| state.resolve(sel));
        if let Some(Ok(email)) = &claude {
            return finish_claude_switch(email, launch, true);
        }
        // An ambiguous Claude prefix is an error, never a reason to switch a
        // different provider's account (same rule as `rm`).
        if let Some(Err(e)) = &claude {
            if store::is_ambiguous_selector(e) {
                return claude.unwrap().map(|_| ());
            }
        }
        if let Some((slug, key)) = resolve_provider_selector(&state, sel) {
            return finish_provider_switch(&slug, &key, launch, true);
        }
        let other = provider_selector_matches(&state, sel);
        if other.len() > 1 {
            bail!(ambiguous_provider_selector(sel, &other));
        }
        // Surface Claude's own error rather than a bare miss.
        if let Some(Err(e)) = claude {
            return Err(e);
        }
        bail!("no account matches '{sel}'");
    }

    // No selector: unchanged v1 auto-pick behavior, Claude-only.
    if state.accounts.is_empty() {
        let available: Vec<_> = state
            .providers
            .iter()
            .filter(|(slug, accounts)| {
                !accounts.accounts.is_empty() && provider_supports_swap(slug)
            })
            .collect();
        if let [(slug, _)] = available.as_slice() {
            let trigger = state.trigger_pct.unwrap_or(TRIGGER_PCT);
            let key = auto_pick_for(&state, &provider_rows(&state, slug), trigger, trigger)?;
            return finish_provider_switch(slug, &key, launch, false);
        }
        bail!("choose a provider account, for example codex:email@example.com");
    }
    let email = select_email(&state, None)?;
    finish_claude_switch(&email, launch, false)
}

pub(crate) fn finish_provider_switch(
    slug: &str,
    key: &str,
    launch: Option<Launch>,
    hold: bool,
) -> Result<()> {
    let provider = provider_by_slug(slug)?;
    if launch.is_some() && !provider.capabilities().supports_launch {
        bail!("launching {} is not supported", provider.display_name());
    }
    let label = switch_to_provider_account(slug, key, hold)?;
    if try_provider_switch_pending(slug, key)? {
        println!("Switch to {label} ({slug}) is queued until the client finishes its work.");
        if launch.is_none() {
            return Ok(());
        }
        wait_for_queued_provider_switch(slug, key, hold, |secs| {
            std::thread::sleep(std::time::Duration::from_secs(secs));
        })?;
        ensure_switch_landed(slug, key, &label)?;
    }
    println!("Active login is now {label} ({slug}).");
    launch_selected_client(provider, launch)
}

/// Re-drive the queued switch to `key` until it leaves the queue, calling
/// `pause(secs)` between attempts. Each re-drive probes the client (a
/// subprocess for Codex), so back off from 1s to 5s while it is still busy.
pub(crate) fn wait_for_queued_provider_switch(
    slug: &str,
    key: &str,
    hold: bool,
    mut pause: impl FnMut(u64),
) -> Result<()> {
    let mut wait = 1;
    while let Some(pending) = queued_provider_switch(slug, key)? {
        pause(wait);
        // Compare-and-set on the intent seen before the pause: a newer pick
        // queued meanwhile wins, and the loop ends on the next check.
        switch_provider_expecting(slug, key, hold, Some(&pending))?;
        wait = (wait + 1).min(5);
    }
    Ok(())
}

/// After waiting out a queued switch: bail unless `key` really became active
/// (the queue may have been superseded or cancelled instead).
pub(crate) fn ensure_switch_landed(slug: &str, key: &str, label: &str) -> Result<()> {
    let active = State::load()?
        .provider_accounts(slug)
        .and_then(|pa| pa.active.clone());
    if active.is_some_and(|a| a.eq_ignore_ascii_case(key)) {
        return Ok(());
    }
    bail!("the queued switch to {label} ({slug}) did not complete; it is not the active login")
}

pub(crate) fn launch_selected_client(
    provider: &dyn Provider,
    launch: Option<Launch>,
) -> Result<()> {
    let Some(launch) = launch else {
        return Ok(());
    };
    println!("\nLaunching {}…\n", provider.display_name());
    let mode = match launch {
        Launch::Fresh => providers::trait_def::LaunchMode::Fresh,
        Launch::Continue => providers::trait_def::LaunchMode::Continue,
    };
    let status = require(provider.launcher())?.launch_client(mode)?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Shared tail of `cmd_switch` for a resolved Claude email: perform the
/// switch, print the confirmation, and optionally launch `claude`.
pub(crate) fn finish_claude_switch(email: &str, launch: Option<Launch>, hold: bool) -> Result<()> {
    let label = switch_to(email, hold)?;

    println!("Active login is now {label}.");
    println!("New `claude` sessions will use it. Already-running sessions keep their");
    println!("current account until they're restarted.");

    launch_selected_client(provider_by_slug(CLAUDE_SLUG)?, launch)
}

#[cfg(test)]
#[path = "switch_tests.rs"]
mod tests;
