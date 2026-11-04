//! `usagio renew` / `login`: browser sign-in for a new or existing account.

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;

use crate::providers::CLAUDE_SLUG;
use crate::selectors::resolve_provider_selector;
use crate::store::State;
use crate::switching::provider::provider_by_slug;
use crate::{login, platform};

/// A `claude:`/`codex:` qualifier on a renew selector picks the provider (as it
/// does for switch/start/rm); returns the bare account selector.
pub(crate) fn strip_renew_qualifier(slug: &mut String, selector: Option<String>) -> Option<String> {
    let sel = selector?;
    match sel.split_once(':') {
        Some((q, account)) => {
            *slug = q.to_ascii_lowercase();
            Some(account.to_string())
        }
        None => Some(sel),
    }
}

/// Resolve a non-Claude renew selector (full key or unique prefix) up front, so
/// a prefix is never passed through as the expected account.
pub(crate) fn resolve_provider_renew_key(state: &State, slug: &str, sel: &str) -> Result<String> {
    resolve_provider_selector(state, &format!("{slug}:{sel}"))
        .map(|(_, key)| key)
        .ok_or_else(|| anyhow!("no unique {slug} account matches '{sel}'"))
}

/// `usagio login|renew [<email>] [--provider <slug>]`: run the provider's own
/// login in the default browser — renewing `<email>`, or adding whichever
/// account signs in when none is given (the CLI side of the menu's "Renew
/// login…" / "Sign in to a new account…"; see `crate::login`).
pub(crate) fn cmd_renew(args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: usagio login|renew [<email>] [--provider <slug>]";
    let mut slug = CLAUDE_SLUG.to_string();
    let mut selector: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--provider" | "-p" => slug = it.next().context(USAGE)?.to_ascii_lowercase(),
            other => selector = Some(other.to_string()),
        }
    }
    let selector = strip_renew_qualifier(&mut slug, selector);
    let provider = provider_by_slug(&slug)?;
    let state = State::load()?;
    let target = match selector {
        Some(sel) if slug == CLAUDE_SLUG => login::Target::Renew(state.resolve(&sel)?),
        Some(sel) => login::Target::Renew(resolve_provider_renew_key(&state, &slug, &sel)?),
        // The browser keeps its own session; this store key is never shown
        // in a window, it just marks the account as browser-added.
        None => login::Target::Add {
            store: login::new_store_key(),
        },
    };
    let who = match &target {
        login::Target::Renew(key) => key.clone(),
        login::Target::Add { .. } => "the account to add".to_string(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let started = login::start(provider, target, move |r| {
        let _ = tx.send(r);
    })?;
    println!(
        "Sign in as {who} in your browser. If it didn't open, visit:\n\n  {}\n",
        started.url
    );
    if let Err(e) = platform().open_url(&started.url) {
        eprintln!("({e:#})");
    }
    let renewed = rx.recv().context("the sign-in ended without a result")??;
    let until = renewed
        .login_expires_at
        .and_then(DateTime::from_timestamp_millis)
        .map(|t| {
            format!(
                "; login good until {}",
                t.with_timezone(&chrono::Local).format("%b %-d")
            )
        })
        .unwrap_or_default();
    let live = if renewed.made_live { " (live now)" } else { "" };
    println!("Signed in {}{until}{live}", renewed.key);
    Ok(())
}
