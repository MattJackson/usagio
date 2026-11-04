//! usagio — usage/limits across multiple Claude / Codex accounts (and every
//! AI-coding CLI we grow support for), keyed by the account email, and
//! account switching by writing the shared keychain login plus the
//! `~/.claude.json` identity Claude Code reads. New `claude` sessions use the
//! switched account; already-running sessions keep theirs until restarted.
//!
//! Renamed from `claude-usage` in v0.4.0. Config dir, launchd label, and
//! Login Items entry migrate transparently on first run; the macOS Keychain
//! service string is intentionally frozen at "claude-usage" to preserve
//! existing tokens (see `providers/claude/mod.rs`).

mod account_order;
mod autoswap;
mod burn_rate;
mod claude_profiles;
mod context_ledger;
mod countdown;
mod credentials;
#[cfg(test)]
mod env_lock;
mod icons;
mod lifecycle;
mod logging;
mod login;
mod menubar;
mod notifications;
mod paths;
mod platform;
mod providers;
mod store;
// Custom tray-anchored popup UI (Phase 1). Whole module compiled only on macOS
// under the off-by-default `custom-popup` feature; this single gated `mod ui;`
// is the ONLY target_os cfg it needs (the module body has none), per
// docs/design/custom-tray-popup.md and the strict_cfg allowlist note.
#[cfg(all(target_os = "macos", feature = "custom-popup"))]
mod ui;
mod usage_log;
mod watchdog;

mod claude_local;
mod cli;
mod display;
mod env_override;
mod history;
mod picking;
mod poll;
mod rows;
mod selectors;
mod state_ops;
mod switching;
mod watch;

use anyhow::Result;

use crate::cli::capture::cmd_capture;
use crate::cli::context::cmd_context;
use crate::cli::install::{cmd_install, cmd_uninstall};
use crate::cli::internal::{cmd_render_shot, cmd_secrets_selftest};
use crate::cli::list::cmd_list;
use crate::cli::renew::cmd_renew;
use crate::cli::report::cmd_report;
use crate::cli::rm::cmd_rm;
use crate::cli::switch::{cmd_switch, Launch};
use crate::cli::token::cmd_token;
use crate::cli::watch::cmd_watch;
use crate::notifications::notify;
use crate::providers::Provider;

/// Process-global handle to the platform impl. `platform::current()` builds
/// once at first use; every host-OS call (keychain, autostart, paths) routes
/// through this so we never sprinkle `#[cfg(target_os = ...)]` across
/// business logic.
static PLATFORM: std::sync::OnceLock<Box<dyn platform::Platform>> = std::sync::OnceLock::new();

/// Return the process-global platform impl, initializing it on first call.
pub(crate) fn platform() -> &'static dyn platform::Platform {
    PLATFORM.get_or_init(platform::current).as_ref()
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// True if `exe` looks like it was launched from inside a macOS `.app`
/// bundle (a Finder double-click), as opposed to a bare CLI invocation
/// (`/opt/homebrew/bin/usagio`, or a shell alias). Checked by path shape
/// only — no `cfg(target_os)` needed, since `.app/Contents/MacOS/` simply
/// never appears in a non-bundle exe path on any OS. Mirrors the same
/// pattern `launch_agent_exe_path` already checks for the install path.
fn is_app_bundle_launch(exe: &std::path::Path) -> bool {
    exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

/// Resolve the effective "first CLI argument" `run()`'s dispatch match
/// switches on: the real first arg if one was given, otherwise `"menubar"`
/// when we were launched from a `.app` bundle (so double-clicking
/// usagio.app in Finder starts the menu bar, not a one-shot `list` that
/// prints to a terminal nobody's watching), otherwise `None` (the bare
/// binary's existing default: `cmd_list`).
///
/// Pure function of `args`/`exe` so a test can inject both without touching
/// real argv or a real bundle.
fn effective_first_arg<'a>(args: &'a [String], exe: &std::path::Path) -> Option<&'a str> {
    args.first()
        .map(String::as_str)
        .or_else(|| is_app_bundle_launch(exe).then_some("menubar"))
}

/// Arguments after the subcommand. Empty (not a panic) when argv was empty,
/// as in an argv-less `.app` bundle launch where the subcommand is implied.
fn tail_args(args: &[String]) -> &[String] {
    args.get(1..).unwrap_or(&[])
}

fn run() -> Result<()> {
    // One-shot rename migration: if `~/.config/claude-usage/` still exists
    // and `~/.config/usagio/` doesn't, atomically move it (with a
    // cross-device copy+delete fallback). Idempotent and safe to run on
    // every subsequent boot. Must run BEFORE the first state-load or
    // logging init so the new dir is populated before anyone reads it.
    match paths::migrate_config_dir_if_needed() {
        Ok(paths::MigrationResult::Migrated { from, to }) => {
            eprintln!(
                "usagio: migrated config directory {} -> {}",
                from.display(),
                to.display()
            );
        }
        Ok(paths::MigrationResult::BothExisted { new, old }) => {
            eprintln!(
                "usagio: both {} and {} exist; using {} and leaving the old one \
                 in place for inspection",
                old.display(),
                new.display(),
                new.display()
            );
        }
        Ok(_) => {}
        Err(paths::MigrationError::OldRemovalFailed { new, source }) => {
            // The NEW tree is populated and mode-correct — proceed as if the
            // migration succeeded. Log the leftover so the user can inspect.
            // See H6 in the round-1 codeaudit findings and M1 for the variant.
            eprintln!(
                "usagio: migrated config to {}; old tree left in place ({source})",
                new.display()
            );
        }
        Err(e) => {
            // Any other migration failure means the new tree is NOT populated.
            // Falling straight through to State::default (an ENOENT from
            // State::load) would look like a fresh install and prompt every
            // user to re-login. Fire a menu-bar notification AND log loudly.
            // We still proceed (State::default), but the user is at least
            // told and can retry.
            let log_path = store::config_dir()
                .map(|d| d.join("usagio.log").display().to_string())
                .unwrap_or_else(|_| "usagio.log in the config directory".to_string());
            let msg = format!(
                "usagio: config migration FAILED — accounts may appear missing. \
                 See {log_path}; try again or copy the old dir manually. \
                 Details: {e:?}"
            );
            eprintln!("{msg}");
            logging::log(&msg);
            notify(&msg);
        }
    }

    // Raise RLIMIT_NOFILE from macOS's stock 256 to something a long-lived
    // menubar can actually survive. Under 256, an fsnotify watcher whose
    // parent lands on a big directory (see spawn_watchers) plus normal state
    // reads exhaust FDs within a day, and every subsequent open — state.json,
    // DNS resolution, keychain lookups — starts failing with EMFILE. Best
    // effort; ignored on non-Unix or if the shell's hard cap is lower.
    #[cfg(unix)]
    raise_nofile_limit();

    // Populate the provider registry once, before any command handler runs.
    // Cheap (a `Vec::push` per feature-gated provider) and idempotent. This is
    // load-bearing: `menubar::run` and the switch/refresh/capture handlers
    // resolve providers via `providers::get`/`providers::all`, so `init()` must
    // precede the dispatch below (and the `providers::all()` call right after).
    providers::init();

    let provider_slugs: Vec<&'static str> =
        providers::all().iter().map(|p| p.provider_id()).collect();
    logging::log(&format!(
        "event=startup version={} pid={} config_dir={} providers=[{}]",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        store::config_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unresolved>".to_string()),
        provider_slugs.join(","),
    ));

    // Spawn fsnotify watchers on every provider's credential paths so
    // rotations the vendor CLI writes (or another usagio process makes)
    // are absorbed as they happen, not just on the next 150s watch tick.
    // Only long-running command paths (watch / menubar) actually benefit;
    // one-shot commands complete before any event fires, but the setup is
    // cheap and self-contained.
    // Leaked deliberately: the watcher owns a thread and must outlive the
    // process. `Box::leak` on a heap allocation is the standard trick for
    // "live for program lifetime" without introducing a global mutex.
    // Stored in a process-global registry (not `Box::leak`'d) so the fd-watchdog
    // can drop + rebuild the watcher at runtime to release leaked descriptors —
    // the runtime safety net for the fd-leak class of outage (see
    // `src/watchdog.rs`). The watcher still lives for the process lifetime; the
    // registry just gives the watchdog a handle on it.
    let providers_static: Vec<&'static dyn Provider> =
        providers::all().iter().map(|b| &**b).collect();
    credentials::install_watchers(providers_static);

    // Register our bundle id with the OS notification system before any
    // notification can fire, so macOS doesn't pop the "Where is use_default?"
    // dialog on the first notify (mac-notification-sys otherwise falls back to
    // the "use_default" literal). One-shot, best-effort; no-op off macOS.
    platform::current().register_notification_app();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().unwrap_or_default();
    match effective_first_arg(&args, &exe) {
        None => cmd_list(&[]),
        Some("list") | Some("ls") => cmd_list(tail_args(&args)),
        Some("capture") | Some("add") => cmd_capture(tail_args(&args)),
        Some("switch") | Some("use") => cmd_switch(args.get(1).map(String::as_str), None),
        Some("start") => cmd_switch(args.get(1).map(String::as_str), Some(Launch::Fresh)),
        Some("continue") | Some("cont") | Some("c") => {
            cmd_switch(args.get(1).map(String::as_str), Some(Launch::Continue))
        }
        Some("token") => cmd_token(args.get(1).map(String::as_str)),
        Some("watch") => cmd_watch(tail_args(&args)),
        Some("menubar") => {
            menubar::theme::set_theme_override_from_args(tail_args(&args));
            menubar::run()
        }
        Some("report") => cmd_report(tail_args(&args)),
        Some("context") => cmd_context(tail_args(&args)),
        Some("install") => cmd_install(),
        Some("uninstall") => cmd_uninstall(),
        Some("rm") | Some("remove") => cmd_rm(args.get(1).map(String::as_str)),
        Some("renew") | Some("login") => cmd_renew(tail_args(&args)),
        // Internal, undocumented — see `cmd_secrets_selftest` doc comment.
        Some("__secrets_selftest") => cmd_secrets_selftest(tail_args(&args)),
        // Internal, undocumented — headless screenshot renderer (see cmd doc).
        Some("__render_shot") => cmd_render_shot(tail_args(&args)),
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            Ok(())
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("usagio {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "usagio — usage & automatic account switching for Claude and Codex\n\n\
         Accounts are identified by their email; commands accept a full email or a\n\
         unique prefix (e.g. `dev` for dev@example.com). Use `codex:email` or `claude:email` to select a provider.\n\n\
         USAGE:\n  \
         usagio                   Show cached usage for every account (default)\n  \
         usagio list --refresh    Fetch usage now, then show it\n  \
         usagio capture [prov]    Save the account you're logged into (default claude; e.g. `capture codex`)\n  \
         usagio switch [email]    Make <email> the active login (no launch)\n  \
         usagio start [email]     Switch, then launch the selected provider CLI\n  \
         usagio continue [email]  Switch, then resume the selected provider session\n  \
         usagio token [email]     Print an access token (the active one: the token Claude Code is using)\n  \
         usagio watch [OPTS]      Auto-swap at your trigger (default 95%), keep working (foreground)\n  \
                                        --trigger <pct>  --ceiling <pct>  --interval <secs>\n  \
         usagio menubar           Run the menu-bar/tray app (usage + auto-swap)\n  \
         usagio menubar --theme <os>  Force an OEM look for a UI audit: windows|macos|gnome|system\n  \
         usagio install           Run the menu-bar/tray app at every login\n  \
         usagio uninstall         Stop running the menu-bar app at login\n  \
         usagio report            Usage patterns by weekday / hour / account\n  \
         usagio report --pace     Per-account burn-rate forecast (empty-in ETA)\n  \
         usagio context [OPTS]    Audit CLI auto-injected context (per turn)\n  \
                                        --provider <slug>  claude|codex|opencode\n  \
                                        --project  <path>  scope in-tree instructions to this project\n  \
         usagio login             Sign in to a new account in your browser (adds it)\n  \
         usagio renew <email>     Sign that account in again in your browser (fresh login)\n  \
                                        --provider <slug>  or use `codex:email`\n  \
         usagio rm <email>        Forget an account\n\n\
         With no [email], switch/start/continue auto-pick, from cached usage, the\n  \
         ready account (15+ min of room before the trigger) whose weekly limit\n  \
         resets earliest (use it before the quota resets). If none is ready, the\n  \
         one that lasts longest; never an account at its limit.\n  \
         With an [email], a pick already at/over the trigger holds until it's\n  \
         exhausted; one under the trigger swaps at the trigger as usual.\n\n\
         Onboarding: log into an account with `claude` as usual, then\n  \
         `usagio capture`. Repeat once per account.\n"
    );
}

/// Bump the soft `RLIMIT_NOFILE` up to the hard cap (or 65536, whichever is
/// smaller). macOS ships every process with a soft cap of 256 files, which
/// is fine for a CLI one-shot but crippling for a menubar that keeps
/// fsnotify watchers open, opens state.json on every poll, does DNS, and
/// talks to the keychain. libc getrlimit/setrlimit are always available on
/// Unix; failure is logged and swallowed (we still run, just with the
/// stock cap).
///
/// The ceiling was 4096, but a since-fixed fsnotify fd leak (the `macos_kqueue`
/// backend — see `credentials.rs`) blew past 4096 and started failing keychain
/// calls with EMFILE. The real fix removed the leak; this higher ceiling is
/// defense-in-depth so any future fd growth degrades gracefully rather than
/// silently breaking account switching.
#[cfg(unix)]
fn raise_nofile_limit() {
    // SAFETY: getrlimit/setrlimit take a valid resource id and a writable
    // rlimit struct; both preconditions are trivially met here.
    unsafe {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
            crate::logging::log("credentials: getrlimit(NOFILE) failed; leaving stock cap");
            return;
        }
        let target = rl.rlim_max.min(65536);
        if rl.rlim_cur >= target {
            return;
        }
        let new = libc::rlimit {
            rlim_cur: target,
            rlim_max: rl.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &new) != 0 {
            crate::logging::log(&format!(
                "credentials: setrlimit(NOFILE, {target}) failed; \
                 leaving soft cap at {}",
                rl.rlim_cur
            ));
        }
    }
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
