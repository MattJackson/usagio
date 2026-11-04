//! Context Ledger — audit what an AI CLI auto-injects into the model's
//! context per turn, with token cost per item.
//!
//! Two consumer surfaces:
//! 1. CLI: `usagio context [--provider <slug>] [--project <path>]`
//! 2. Menu: "Context Ledger ▸" floating NSPanel (deferred to a follow-up commit)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Ledger diagnostics to stderr. Unlike `eprintln!`, a closed stderr pipe
/// (`usagio context 2>&1 | head`) is ignored instead of panicking.
macro_rules! ledger_warn {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub(crate) mod claude;
pub(crate) mod cli;
pub(crate) mod codex;
mod mcp;
pub(crate) mod opencode;
pub(crate) mod render;
mod tokenize;

#[cfg(test)]
mod tests;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ItemKind {
    GlobalInstructions,
    ProjectInstructions,
    Skill,
    McpTools,
    SubagentDef,
    PluginManifest,
    /// Free-form context extension (e.g. opencode `prompt` string).
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LedgerItem {
    pub(crate) kind: ItemKind,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    /// mtime at capture time (for staleness detection).
    pub(crate) mtime: DateTime<Utc>,
    token_count: usize,
    content_bytes: usize,
    /// Optional MCP-server-specific: how many tools contribute.
    tool_count: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Ledger {
    pub(crate) provider: String,
    pub(crate) project: Option<PathBuf>,
    pub(crate) items: Vec<LedgerItem>,
    total_tokens: usize,
    estimated_cost_per_turn: Option<f64>,
    captured_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LedgerError {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Build the ledger for a provider. `project` is a filesystem path the CLI
/// would be invoked from — used to find project-scoped context (e.g. an
/// in-tree CLAUDE.md). Missing config dirs yield an empty ledger, not an error.
fn build_ledger(provider: &str, project: Option<&Path>) -> Result<Ledger, LedgerError> {
    let cli = Cli::from_slug(provider)
        .ok_or_else(|| LedgerError::UnknownProvider(provider.to_string()))?;
    let items = match cli {
        Cli::Claude => claude::collect(project)?,
        Cli::Codex => codex::collect(project)?,
        Cli::Opencode => opencode::collect(project)?,
    };
    let total_tokens = items.iter().map(|i| i.token_count).sum();
    let estimated_cost_per_turn = estimate_cost(provider, total_tokens);
    Ok(Ledger {
        provider: provider.to_string(),
        project: project.map(|p| p.to_path_buf()),
        items,
        total_tokens,
        estimated_cost_per_turn,
        captured_at: Utc::now(),
    })
}

fn render_terminal(ledger: &Ledger) -> String {
    render::terminal(ledger)
}

/// Returns true if any tracked file's on-disk mtime is newer than captured_at.
/// Missing files are ignored (returns false for that item).
///
/// M5 (round-1 codeaudit): the previous impl called `metadata()` twice per
/// item (once to check the difference, again to compare) — both TOCTOU-race
/// windows and a `.unwrap()` on the second call would panic if the file
/// disappeared between them. Single-call form: read mtime once, compare
/// directly.
#[allow(dead_code)] // wired in v0.5.0 menu panel (context ledger UI)
fn is_stale(ledger: &Ledger) -> bool {
    ledger.items.iter().any(|item| {
        std::fs::metadata(&item.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(utc_from_system_time)
            .map(|mt| mt > ledger.captured_at)
            .unwrap_or(false)
    })
}

/// Convert a `SystemTime` without going through chrono's `From<SystemTime>`,
/// which panics for times outside chrono's representable range (a hostile or
/// corrupt filesystem can report such mtimes). `None` when out of range.
fn utc_from_system_time(t: std::time::SystemTime) -> Option<DateTime<Utc>> {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => DateTime::from_timestamp(i64::try_from(d.as_secs()).ok()?, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).ok()?.checked_neg()?;
            let nanos = d.subsec_nanos();
            if nanos == 0 {
                DateTime::from_timestamp(secs, 0)
            } else {
                DateTime::from_timestamp(secs.checked_sub(1)?, 1_000_000_000 - nanos)
            }
        }
    }
}

/// mtime of `path`, falling back to "now" if unreadable or out of range.
fn file_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(utc_from_system_time)
        .unwrap_or_else(Utc::now)
}

/// Upper bound on how much of any one context file is read.
const MAX_CONTEXT_FILE_BYTES: u64 = 4 << 20;

/// Read a context file for token counting: regular files only (no FIFOs,
/// devices, ...), capped at [`MAX_CONTEXT_FILE_BYTES`].
/// `proj` and its ancestors up to (and including) the nearest directory with
/// a `.git` entry, nearest first. Just `proj` when there is no git root.
fn dirs_up_to_git_root(proj: &Path) -> Vec<&Path> {
    let Some(root) = proj.ancestors().find(|d| d.join(".git").exists()) else {
        return vec![proj];
    };
    let mut dirs: Vec<&Path> = proj.ancestors().take_while(|d| *d != root).collect();
    dirs.push(root);
    dirs
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    read_capped_max(path, MAX_CONTEXT_FILE_BYTES)
}

/// Upper bound for JSON config files (`~/.claude.json` grows large).
const MAX_CONFIG_FILE_BYTES: u64 = 64 << 20;

fn read_capped_max(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// Probe every Claude-Code-shaped MCP server config concurrently.
fn probe_servers(
    servers: Vec<(String, serde_json::Value)>,
) -> Vec<(String, Result<mcp::McpSummary, String>)> {
    probe_launches(
        servers
            .into_iter()
            .map(|(name, cfg)| (name, mcp::claude_launch(&cfg)))
            .collect(),
    )
}

/// Probe every resolved MCP launch concurrently (each has its own wall-clock
/// budget, so serial probing made total latency the SUM of all servers).
/// A server that failed to resolve keeps its error. Results keep the input
/// order.
fn probe_launches(
    servers: Vec<(String, Result<mcp::Launch, String>)>,
) -> Vec<(String, Result<mcp::McpSummary, String>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = servers
            .iter()
            .map(|(_, launch)| {
                scope.spawn(move || match launch {
                    Ok(l) => mcp::fetch_launch(l),
                    Err(e) => Err(e.clone()),
                })
            })
            .collect();
        servers
            .iter()
            .zip(handles)
            .map(|((name, _), h)| {
                let r = h
                    .join()
                    .unwrap_or_else(|_| Err("mcp probe thread panicked".to_string()));
                (name.clone(), r)
            })
            .collect()
    })
}

/// The AI CLIs the ledger knows. Every provider-slug alias is resolved here
/// once, so discovery, cost and display can never disagree about a slug.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Cli {
    Claude,
    Codex,
    Opencode,
}

impl Cli {
    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Opencode => "opencode",
        }
    }

    /// Rough input price, USD per 1M tokens.
    fn input_usd_per_million(self) -> f64 {
        match self {
            Self::Claude => 3.0,   // Sonnet input
            Self::Codex => 5.0,    // ballpark; not tied to one model
            Self::Opencode => 3.0, // typically Anthropic-backed
        }
    }
}

/// Rough cost estimate per turn — very approximate. Uses Sonnet input pricing
/// as a default; real per-model math lives in a future pricing module.
fn estimate_cost(provider: &str, total_tokens: usize) -> Option<f64> {
    let per_million_usd = Cli::from_slug(provider)?.input_usd_per_million();
    Some((total_tokens as f64) * per_million_usd / 1_000_000.0)
}
