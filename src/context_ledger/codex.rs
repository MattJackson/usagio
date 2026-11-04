//! Discovery for the OpenAI Codex CLI.
//!
//! Codex context sources (from openai/codex repo inspection):
//! The Codex home is `$CODEX_HOME` (non-empty), defaulting to `~/.codex`.
//! - <codex home>/AGENTS.override.md, else AGENTS.md — global instructions
//!   (the legacy TS CLI's instructions.md is not read by current Codex)
//! - <codex home>/config.toml → any [mcp_servers.*] blocks
//! - AGENTS.override.md, else AGENTS.md in each directory from the git root
//!   down to <project> — project-scoped instructions, capped at 32 KiB total.
//!   The first of those that exists is the directory's doc; a blank one
//!   contributes nothing (an empty override suppresses AGENTS.md)
//!
//! Codex evolves quickly — recheck paths against upstream when re-verifying.

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

/// Codex's home directory: `$CODEX_HOME` if set and non-empty, else
/// `~/.codex`, with `~` resolved by the same `paths::user_home()` the Codex
/// credential-store resolver uses. (An empty `$CODEX_HOME` is treated as unset
/// here, as Codex itself does.)
fn codex_dir() -> Option<PathBuf> {
    resolve_codex_dir(
        crate::paths::path_env_var("CODEX_HOME"),
        crate::paths::user_home(),
    )
}

pub(crate) fn resolve_codex_dir(
    codex_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match codex_home {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => home.map(|h| h.join(".codex")),
    }
}

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let Some(codex_dir) = codex_dir() else {
        return Ok(items);
    };

    // Global instructions — the first non-empty of AGENTS.override.md,
    // AGENTS.md.
    for (fname, display) in [
        ("AGENTS.override.md", "Global AGENTS.override.md"),
        ("AGENTS.md", "Global AGENTS.md"),
    ] {
        let before = items.len();
        push_file_if_exists(
            &mut items,
            codex_dir.join(fname),
            ItemKind::GlobalInstructions,
            display,
            None,
        );
        if items.len() > before {
            break;
        }
    }

    // Project instructions: walk from the git root down to the project,
    // taking AGENTS.override.md else AGENTS.md per directory, until the
    // combined size reaches Codex's project_doc_max_bytes (32 KiB default).
    if let Some(proj) = project {
        let proj = fs::canonicalize(proj).unwrap_or_else(|_| proj.to_path_buf());
        let root = proj
            .ancestors()
            .find(|d| d.join(".git").exists())
            .unwrap_or(&proj);
        let mut dirs: Vec<&Path> = proj.ancestors().take_while(|d| *d != root).collect();
        dirs.push(root);
        dirs.reverse();
        let mut remaining = PROJECT_DOC_MAX_BYTES;
        for dir in dirs {
            if remaining == 0 {
                break;
            }
            // Codex takes the first candidate that exists as a file; a blank
            // one contributes nothing rather than falling through.
            let Some(fname) = ["AGENTS.override.md", "AGENTS.md"]
                .into_iter()
                .find(|f| dir.join(f).is_file())
            else {
                continue;
            };
            let label = if dir == proj {
                format!("Project {fname}")
            } else {
                format!("{fname} ({})", dir.display())
            };
            let before = items.len();
            push_file_if_exists(
                &mut items,
                dir.join(fname),
                ItemKind::ProjectInstructions,
                &label,
                Some(remaining),
            );
            if items.len() > before {
                remaining = remaining.saturating_sub(items[before].content_bytes);
            }
        }
    }

    // MCP servers from config.toml
    let config = codex_dir.join("config.toml");
    if config.exists() {
        let read = super::read_capped_max(&config, super::MAX_CONFIG_FILE_BYTES)
            .ok_or_else(|| "not a regular file or unreadable".to_string())
            .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()));
        match read {
            Err(e) => ledger_warn!("could not read {}: {}", config.display(), e),
            Ok(text) => match toml::from_str::<toml::Value>(&text) {
                Err(e) => ledger_warn!("could not parse {}: {}", config.display(), e),
                Ok(parsed) => {
                    if let Some(mcp_servers) = parsed.get("mcp_servers").and_then(|v| v.as_table())
                    {
                        // Convert TOML to JSON for the mcp module's uniform interface.
                        let servers = mcp_servers
                            .iter()
                            // `enabled = false` servers are never started by Codex.
                            .filter(|(_, cfg)| {
                                cfg.get("enabled").and_then(|e| e.as_bool()) != Some(false)
                            })
                            .map(|(name, cfg)| {
                                (
                                    name.clone(),
                                    serde_json::to_value(cfg).unwrap_or(serde_json::Value::Null),
                                )
                            })
                            .collect();
                        for (name, result) in super::probe_servers(servers) {
                            match result {
                                Ok(summary) => items.push(LedgerItem {
                                    kind: ItemKind::McpTools,
                                    name: format!("mcp: {}", name),
                                    path: config.clone(),
                                    mtime: file_mtime(&config),
                                    token_count: summary.token_count,
                                    content_bytes: summary.byte_count,
                                    tool_count: Some(summary.tool_count),
                                }),
                                Err(err) => ledger_warn!("codex mcp {} skipped: {}", name, err),
                            }
                        }
                    }
                }
            },
        }
    }

    Ok(items)
}

/// Codex's default `project_doc_max_bytes`.
const PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

/// Push `path` as an item unless missing/empty; `limit` truncates the counted
/// bytes (Codex stops adding project docs at its size cap).
fn push_file_if_exists(
    items: &mut Vec<LedgerItem>,
    path: PathBuf,
    kind: ItemKind,
    name: &str,
    limit: Option<usize>,
) {
    let Some(mut bytes) = super::read_capped(&path) else {
        return;
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return;
    }
    if let Some(l) = limit {
        bytes.truncate(l);
    }
    let text = String::from_utf8_lossy(&bytes);
    let token_count = tokenize::count_tokens(&text, tokenize::TokenizerHint::OpenAi);
    items.push(LedgerItem {
        kind,
        name: name.to_string(),
        mtime: file_mtime(&path),
        path,
        token_count,
        content_bytes: bytes.len(),
        tool_count: None,
    });
}

use super::file_mtime;
