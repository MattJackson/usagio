//! Discovery for the OpenAI Codex CLI.
//!
//! Codex context sources (from openai/codex repo inspection):
//! The Codex home is `$CODEX_HOME` (non-empty), defaulting to `~/.codex`.
//! - <codex home>/AGENTS.md — global instructions
//! - <codex home>/instructions.md — global instructions (older name; only
//!   used when AGENTS.md is absent)
//! - <codex home>/config.toml → any [mcp_servers.*] blocks
//! - <project>/AGENTS.md — project-scoped instructions
//!
//! Codex evolves quickly — recheck paths against upstream when re-verifying.

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

/// Codex's home directory: `$CODEX_HOME` if set and non-empty, else
/// `~/.codex` (matches the Codex credential-store resolver).
fn codex_dir() -> Option<PathBuf> {
    resolve_codex_dir(std::env::var_os("CODEX_HOME"), dirs::home_dir())
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

    // Global AGENTS.md / instructions.md — whichever exists (AGENTS.md first;
    // instructions.md is the older name and only a fallback).
    for (fname, kind, display) in [
        (
            "AGENTS.md",
            ItemKind::GlobalInstructions,
            "Global AGENTS.md",
        ),
        (
            "instructions.md",
            ItemKind::GlobalInstructions,
            "Global instructions.md",
        ),
    ] {
        let before = items.len();
        push_file_if_exists(&mut items, codex_dir.join(fname), kind, display);
        if items.len() > before {
            break;
        }
    }

    // Project AGENTS.md
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("AGENTS.md"),
            ItemKind::ProjectInstructions,
            "Project AGENTS.md",
        );
    }

    // MCP servers from config.toml
    let config = codex_dir.join("config.toml");
    if config.exists() {
        match fs::read_to_string(&config) {
            Err(e) => eprintln!("could not read {}: {}", config.display(), e),
            Ok(text) => match toml::from_str::<toml::Value>(&text) {
                Err(e) => eprintln!("could not parse {}: {}", config.display(), e),
                Ok(parsed) => {
                    if let Some(mcp_servers) = parsed.get("mcp_servers").and_then(|v| v.as_table())
                    {
                        // Convert TOML to JSON for the mcp module's uniform interface.
                        let servers = mcp_servers
                            .iter()
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
                                Err(err) => eprintln!("codex mcp {} skipped: {}", name, err),
                            }
                        }
                    }
                }
            },
        }
    }

    Ok(items)
}

fn push_file_if_exists(items: &mut Vec<LedgerItem>, path: PathBuf, kind: ItemKind, name: &str) {
    let Some(bytes) = super::read_capped(&path) else {
        return;
    };
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
