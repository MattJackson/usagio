//! Resolving a user-typed account selector (`email`, prefix, `provider:email`).

use anyhow::{anyhow, bail, Result};

use crate::picking::auto_pick_for;
use crate::providers::CLAUDE_SLUG;
use crate::rows::{row_from_account, Row};
use crate::store;
use crate::store::State;
use crate::switching::provider::provider_by_slug;
use crate::watch::cadence::TRIGGER_PCT;

/// Resolve `selector` (a full account key, or a unique case-insensitive
/// prefix) against every non-Claude provider's captured accounts in state.
/// Returns `(provider_slug, account_key)` on an unambiguous match. Mirrors
/// `State::resolve`'s prefix-matching contract, scoped across all provider
/// slots instead of one flat list, since account keys are only unique within
/// a single provider (a Codex and a future provider could share an email).
pub(crate) fn resolve_provider_selector(state: &State, selector: &str) -> Option<(String, String)> {
    let mut matches = provider_selector_matches(state, selector);
    (matches.len() == 1).then(|| matches.remove(0))
}

/// Every non-Claude `(slug, key)` `selector` matches: the exact matches if
/// any, else the prefix matches. More than one means the selector is ambiguous.
pub(crate) fn provider_selector_matches(state: &State, selector: &str) -> Vec<(String, String)> {
    let (scope, sel) = selector
        .trim()
        .split_once(':')
        .map_or((None, selector.trim()), |(slug, key)| (Some(slug), key));
    if sel.is_empty() {
        return Vec::new();
    }
    let sel_lc = sel.to_lowercase();
    let mut exact: Vec<(String, String)> = Vec::new();
    let mut prefix: Vec<(String, String)> = Vec::new();
    for (slug, pa) in &state.providers {
        if scope.is_some_and(|scope| !scope.eq_ignore_ascii_case(slug)) {
            continue;
        }
        for a in &pa.accounts {
            let key_lc = a.key.to_lowercase();
            if key_lc == sel_lc {
                exact.push((slug.clone(), a.key.clone()));
            } else if key_lc.starts_with(&sel_lc) {
                prefix.push((slug.clone(), a.key.clone()));
            }
        }
    }
    if exact.is_empty() {
        prefix
    } else {
        exact
    }
}

/// Resolve `selector` to an account email, or auto-pick when none is given.
pub(crate) fn select_email(state: &State, selector: Option<&str>) -> Result<String> {
    match selector {
        Some(sel) => state.resolve(sel),
        None => {
            let rows: Vec<Row> = state.accounts.iter().map(row_from_account).collect();
            let trigger = state.trigger_pct.unwrap_or(TRIGGER_PCT);
            auto_pick_for(state, &rows, trigger, trigger)
        }
    }
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// Resolve an account selector across providers: `claude:x` / `codex:x`
/// qualify the provider; an unqualified selector tries Claude first, then the
/// other providers' accounts. Returns `(slug, key)`.
pub(crate) fn resolve_account_selector(state: &State, sel: &str) -> Result<(String, String)> {
    if let Some((slug, account)) = sel.split_once(':') {
        let slug = slug.to_ascii_lowercase();
        provider_by_slug(&slug)?;
        if slug == CLAUDE_SLUG {
            return Ok((slug, state.resolve(account)?));
        }
        return resolve_provider_selector(state, sel)
            .ok_or_else(|| anyhow!("no unique {slug} account matches '{account}'"));
    }
    // Unqualified: fall through to other providers ONLY when Claude has no
    // match at all. An ambiguous Claude selector stays an error, and a match in
    // more than one provider must be qualified (this feeds a destructive `rm`).
    let claude = state.resolve(sel);
    let mut other = provider_selector_matches(state, sel);
    match (claude, other.len()) {
        (Ok(email), 0) => Ok((CLAUDE_SLUG.to_string(), email)),
        (Ok(email), _) => {
            let (slug, key) = &other[0];
            bail!(
                "'{sel}' matches {CLAUDE_SLUG}:{email} and {slug}:{key}; qualify it with a provider prefix"
            )
        }
        (Err(e), _) if store::is_ambiguous_selector(&e) => Err(e),
        (Err(_), 1) => Ok(other.remove(0)),
        (Err(_), 0) => bail!("no account matches '{sel}'"),
        (Err(_), _) => bail!(ambiguous_provider_selector(sel, &other)),
    }
}

pub(crate) fn ambiguous_provider_selector(sel: &str, matches: &[(String, String)]) -> String {
    let list: Vec<String> = matches.iter().map(|(s, k)| format!("{s}:{k}")).collect();
    format!(
        "'{sel}' matches more than one account ({}); be more specific",
        list.join(", ")
    )
}

#[cfg(test)]
#[path = "selectors_tests.rs"]
mod tests;
