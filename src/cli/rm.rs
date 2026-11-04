//! `usagio rm`: forget an account.

use anyhow::{Context, Result};

use crate::providers::CLAUDE_SLUG;
use crate::selectors::resolve_account_selector;
use crate::state_ops::{remove_account, remove_provider_account_generic};
use crate::store::State;

pub(crate) fn cmd_rm(selector: Option<&str>) -> Result<()> {
    let selector = selector.context("usage: usagio rm <email>")?;
    let state = State::load()?;
    let (slug, key) = resolve_account_selector(&state, selector)?;
    if slug == CLAUDE_SLUG {
        remove_account(&key)?;
    } else {
        remove_provider_account_generic(&slug, &key)?;
    }
    println!("Removed {key}.");
    Ok(())
}

#[cfg(test)]
#[path = "rm_tests.rs"]
mod tests;
