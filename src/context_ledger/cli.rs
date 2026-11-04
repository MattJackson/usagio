//! `usagio context [--provider <slug>] [--project <path>]` subcommand.
//!
//! When no `--provider` is given, iterates the fixed built-in list (claude,
//! codex, opencode); it is not tied to the provider trait registry. An
//! unknown provider name prints `unknown provider` to stderr, and the command
//! exits non-zero if no provider produced a ledger at all.

use super::{build_ledger, render_terminal};
use std::io::Write;
use std::path::PathBuf;

pub fn run(provider: Option<String>, project: Option<PathBuf>) -> anyhow::Result<()> {
    let providers = match provider {
        Some(p) => vec![p],
        None => vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ],
    };
    // L8 (round-1 codeaudit): render one section per provider unconditionally
    // so the output order matches the iteration order (deterministic) and a
    // provider with no items still surfaces its heading. Previously an empty
    // ledger was suppressed until at least one other provider had already
    // printed, so identical inputs could yield different presence-of-headings
    // depending on which provider iterated first.
    let mut first = true;
    let mut printed_any = false;
    for prov in providers {
        match build_ledger(&prov, project.as_deref()) {
            Ok(ledger) => {
                // `println!` panics on EPIPE (`usagio context | head`); a
                // closed pipe is a normal, quiet exit.
                let mut out = std::io::stdout().lock();
                let sep = if first { "" } else { "\n" };
                if let Err(e) = write!(out, "{sep}{}", render_terminal(&ledger)) {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(e.into());
                }
                first = false;
                printed_any = true;
            }
            Err(super::LedgerError::UnknownProvider(_)) => {
                ledger_warn!("usagio context: unknown provider '{}'", prov);
            }
            Err(err) => {
                ledger_warn!("usagio context: {} failed: {}", prov, err);
            }
        }
    }
    if !printed_any {
        anyhow::bail!("no context ledger could be built for the requested provider(s)");
    }
    Ok(())
}
