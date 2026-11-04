//! `usagio watch`: the foreground auto-swap loop.

use anyhow::{anyhow, bail, Context, Result};

use crate::menubar;
use crate::store::State;
use crate::watch::cadence::TRIGGER_PCT;

pub(crate) const WATCH_USAGE: &str =
    "usage: usagio watch [--interval <secs>] [--trigger <pct>] [--ceiling <pct>]";

/// Parse `watch` flags; a missing or unparsable value is an error, never a
/// silent fallback to the saved/default setting.
pub(crate) fn parse_watch_opts(args: &[String]) -> Result<menubar::poller::LoopOpts> {
    fn value<T: std::str::FromStr>(flag: &str, it: &mut std::slice::Iter<'_, String>) -> Result<T> {
        let raw = it
            .next()
            .with_context(|| format!("{flag} needs a value\n{WATCH_USAGE}"))?;
        raw.parse()
            .map_err(|_| anyhow!("invalid value '{raw}' for {flag}\n{WATCH_USAGE}"))
    }
    let mut opts = menubar::poller::LoopOpts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--interval" => opts.base = value("--interval", &mut it)?,
            "--trigger" => opts.trigger = Some(value("--trigger", &mut it)?),
            "--ceiling" => opts.ceiling = Some(value("--ceiling", &mut it)?),
            other => bail!("unknown watch option: {other}\n{WATCH_USAGE}"),
        }
    }
    Ok(opts)
}

pub(crate) fn cmd_watch(args: &[String]) -> Result<()> {
    let opts = parse_watch_opts(args)?;
    let state = State::load().unwrap_or_else(|e| {
        eprintln!("usagio watch: could not read state.json ({e:#}); the banner below shows defaults, not your saved settings");
        State::default()
    });
    let trigger = opts
        .trigger
        .unwrap_or_else(|| state.trigger_pct.unwrap_or(TRIGGER_PCT));
    let ceiling = opts.ceiling.unwrap_or(trigger).min(trigger);
    eprintln!(
        "usagio watch: every {}s, swap at {trigger:.0}%, target < {ceiling:.0}%{}",
        opts.base,
        if state.autoswap_disabled {
            " (auto-swap is off in settings: observing only)"
        } else {
            ""
        }
    );
    // The exact loop the menu-bar app runs: the configured trigger and
    // auto-swap setting (unless overridden above), reset-boundary wakes,
    // suspend/resume detection, per-provider cadence and swaps.
    menubar::poller::poll_loop_with(&opts);
    Ok(())
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
