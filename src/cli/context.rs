//! `usagio context`: the Context Ledger audit.

use anyhow::{bail, Context, Result};

use crate::context_ledger;

// ---------------------------------------------------------------------------
// context — Context Ledger
// ---------------------------------------------------------------------------

/// `usagio context [--provider SLUG] [--project PATH]` — audit what a
/// CLI auto-injects into the model context per turn, with token cost per item.
/// Delegates parsing (positional-agnostic) to a pure helper so the arg parsing
/// stays unit-testable without a subprocess.
pub(crate) fn cmd_context(args: &[String]) -> Result<()> {
    let (provider, project) = parse_context_args(args)?;
    context_ledger::cli::run(provider, project)
}

/// Parse the `context` subcommand's two supported flags. Unknown flags are
/// rejected so a typo doesn't silently degrade to "audit every provider".
pub(crate) fn parse_context_args(
    args: &[String],
) -> Result<(Option<String>, Option<std::path::PathBuf>)> {
    let mut provider: Option<String> = None;
    let mut project: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--provider" => {
                provider = Some(it.next().cloned().context("--provider requires a value")?);
            }
            "--project" => {
                project = Some(std::path::PathBuf::from(
                    it.next().context("--project requires a value")?,
                ));
            }
            other => bail!("unknown context option: {other}"),
        }
    }
    Ok((provider, project))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
