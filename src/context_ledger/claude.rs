//! Discovery for the Anthropic Claude Code CLI.
//!
//! Claude loads context from a well-documented set of locations:
//! - ~/.claude/CLAUDE.md — global instructions
//! - <project>/CLAUDE.md — project instructions
//! - ~/.claude/skills/*/SKILL.md — skill definitions; only the frontmatter
//!   `name`/`description` is injected per turn (the body loads on use)
//! - ~/.claude/agents/*.md — subagent definitions (likewise frontmatter only)
//! - ~/.claude/plugins/*/plugin.json — plugin manifests (legacy layout)
//! - MCP servers, each contributing its tool schemas: `~/.claude.json`
//!   (`mcpServers` + `projects.<path>.mcpServers`), `<project>/.mcp.json`,
//!   and (legacy fallback) `~/.claude/settings.json` `mcpServers`

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(items),
    };
    let claude_dir = home.join(".claude");
    let settings = claude_dir.join("settings.json");

    // Global CLAUDE.md
    push_file_if_exists(
        &mut items,
        claude_dir.join("CLAUDE.md"),
        ItemKind::GlobalInstructions,
        "Global CLAUDE.md",
    );

    // Project CLAUDE.md
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("CLAUDE.md"),
            ItemKind::ProjectInstructions,
            "Project CLAUDE.md",
        );
    }

    // Skills — ~/.claude/skills/<name>/SKILL.md
    let skills_dir = claude_dir.join("skills");
    if let Ok(entries) = fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let skill_md = entry.path().join("SKILL.md");
            push_frontmatter_if_exists(
                &mut items,
                skill_md,
                ItemKind::Skill,
                &format!("skill: {}", name),
            );
        }
    }

    // Subagents — ~/.claude/agents/*.md
    let agents_dir = claude_dir.join("agents");
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "agent".to_string());
                push_frontmatter_if_exists(
                    &mut items,
                    path,
                    ItemKind::SubagentDef,
                    &format!("agent: {}", name),
                );
            }
        }
    }

    // Plugins — ~/.claude/plugins/**/plugin.json (only depth 2 to avoid runaway walks)
    let plugins_dir = claude_dir.join("plugins");
    if let Ok(top) = fs::read_dir(&plugins_dir) {
        for entry in top.flatten() {
            let manifest = entry.path().join("plugin.json");
            if manifest.exists() {
                let name = entry.file_name().to_string_lossy().into_owned();
                push_file_if_exists(
                    &mut items,
                    manifest,
                    ItemKind::PluginManifest,
                    &format!("plugin: {}", name),
                );
            }
        }
    }

    // MCP tools. Claude Code keeps user/local-scope servers in ~/.claude.json
    // and project-scope servers in <project>/.mcp.json; settings.json
    // `mcpServers` is only a legacy fallback. First definition of a name wins
    // (local > project > user > legacy).
    let mut sources: Vec<(PathBuf, Vec<(String, serde_json::Value)>)> = Vec::new();
    let user_json = home.join(".claude.json");
    let user_val = read_json_logged(&user_json);
    if let Some(val) = &user_val {
        if let Some(proj) = project {
            let key = proj.to_string_lossy();
            if let Some(local) = val
                .get("projects")
                .and_then(|p| p.get(key.as_ref()))
                .and_then(|p| p.get("mcpServers"))
                .and_then(|m| m.as_object())
            {
                sources.push((user_json.clone(), object_entries(local)));
            }
        }
    }
    if let Some(proj) = project {
        let mcp_json = proj.join(".mcp.json");
        if let Some(val) = read_json_logged(&mcp_json) {
            if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
                sources.push((mcp_json, object_entries(m)));
            }
        }
    }
    if let Some(val) = &user_val {
        if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
            sources.push((user_json.clone(), object_entries(m)));
        }
    }
    if let Some(val) = read_json_logged(&settings) {
        if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
            sources.push((settings.clone(), object_entries(m)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    let mut origin = std::collections::HashMap::new();
    for (path, entries) in sources {
        for (name, cfg) in entries {
            if seen.insert(name.clone()) {
                origin.insert(name.clone(), path.clone());
                servers.push((name, cfg));
            }
        }
    }
    for (name, result) in super::probe_servers(servers) {
        let path = origin.remove(&name).unwrap_or_else(|| settings.clone());
        match result {
            Ok(summary) => {
                items.push(LedgerItem {
                    kind: ItemKind::McpTools,
                    name: format!("mcp: {}", name),
                    mtime: file_mtime(&path),
                    path,
                    token_count: summary.token_count,
                    content_bytes: summary.byte_count,
                    tool_count: Some(summary.tool_count),
                });
            }
            Err(err) => {
                // Skip failed MCP servers — timeout / crash / bad config.
                // In a follow-up we surface a "?" row with the error.
                eprintln!("mcp {} skipped: {}", name, err);
            }
        }
    }

    Ok(items)
}

fn object_entries(
    m: &serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, serde_json::Value)> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Read + parse a JSON file. Missing file -> `None` silently; unreadable or
/// malformed -> `None` with a stderr diagnostic (so the ledger's understated
/// baseline is explained rather than looking like "no MCP servers").
fn read_json_logged(path: &Path) -> Option<serde_json::Value> {
    let bytes = match super::read_capped_max(path, super::MAX_CONFIG_FILE_BYTES) {
        Some(b) => b,
        None => {
            if path.exists() {
                eprintln!("could not read {}", path.display());
            }
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("could not parse {}: {}", path.display(), e);
            None
        }
    }
}

/// The part of a skill/agent file Claude Code injects every turn: the
/// frontmatter `name` and `description` (including indented continuation
/// lines). Everything else (the body) only loads when the skill/agent is
/// used. `None` when there is no frontmatter block.
fn injected_frontmatter(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut out = String::new();
    let mut keep = false;
    for line in lines {
        if line.trim_end() == "---" {
            return Some(out);
        }
        let continuation = line.starts_with([' ', '\t']) || line.trim().is_empty();
        if !continuation {
            let key = line.split(':').next().unwrap_or("").trim();
            keep = key == "name" || key == "description";
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    None // unterminated frontmatter
}

fn push_frontmatter_if_exists(
    items: &mut Vec<LedgerItem>,
    path: PathBuf,
    kind: ItemKind,
    name: &str,
) {
    let Some(bytes) = super::read_capped(&path) else {
        return;
    };
    let full = String::from_utf8_lossy(&bytes);
    // No usable frontmatter: fall back to the whole file rather than 0.
    let text = injected_frontmatter(&full).unwrap_or_else(|| full.to_string());
    let token_count = tokenize::count_tokens(&text, tokenize::TokenizerHint::Anthropic);
    items.push(LedgerItem {
        kind,
        name: name.to_string(),
        mtime: file_mtime(&path),
        path,
        token_count,
        content_bytes: text.len(),
        tool_count: None,
    });
}

fn push_file_if_exists(items: &mut Vec<LedgerItem>, path: PathBuf, kind: ItemKind, name: &str) {
    let Some(bytes) = super::read_capped(&path) else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let token_count = tokenize::count_tokens(&text, tokenize::TokenizerHint::Anthropic);
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
