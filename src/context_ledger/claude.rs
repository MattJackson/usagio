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
    // Claude Code keys projects by absolute path.
    let proj_abs: Option<PathBuf> = project.map(|p| {
        fs::canonicalize(p).unwrap_or_else(|_| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|c| c.join(p))
                    .unwrap_or_else(|_| p.to_path_buf())
            }
        })
    });
    let local_proj = match (&user_val, &proj_abs) {
        (Some(val), Some(proj)) => val
            .get("projects")
            .and_then(|p| p.get(proj.to_string_lossy().as_ref()))
            .cloned(),
        _ => None,
    };
    if let Some(local) = local_proj
        .as_ref()
        .and_then(|p| p.get("mcpServers"))
        .and_then(|m| m.as_object())
    {
        sources.push((user_json.clone(), object_entries(local)));
    }
    let mut approval = McpApproval::default();
    if let Some(lp) = &local_proj {
        approval.absorb(lp, true);
    }
    let settings_val = read_json_logged(&settings);
    if let Some(v) = &settings_val {
        approval.absorb(v, false);
    }
    if let Some(proj) = &proj_abs {
        for f in ["settings.json", "settings.local.json"] {
            if let Some(v) = read_json_logged(&proj.join(".claude").join(f)) {
                approval.absorb(&v, false);
            }
        }
        let mcp_json = proj.join(".mcp.json");
        if let Some(val) = read_json_logged(&mcp_json) {
            if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
                let mut approved = Vec::new();
                for (name, cfg) in m {
                    if approval.project_server_approved(name) {
                        approved.push((name.clone(), cfg.clone()));
                    } else {
                        // Never execute repo-controlled commands Claude Code
                        // itself would not start.
                        eprintln!("mcp {}: not approved (not started)", name);
                    }
                }
                sources.push((mcp_json, approved));
            }
        }
    }
    if let Some(val) = &user_val {
        if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
            sources.push((user_json.clone(), object_entries(m)));
        }
    }
    if let Some(val) = &settings_val {
        if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
            sources.push((settings.clone(), object_entries(m)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    let mut origin = std::collections::HashMap::new();
    for (path, entries) in sources {
        for (name, cfg) in entries {
            if approval.disabled.contains(&name) {
                eprintln!("mcp {}: disabled for this project (not started)", name);
                continue;
            }
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

/// Claude Code's approval state for project-scope `.mcp.json` servers.
#[derive(Default)]
struct McpApproval {
    enable_all: bool,
    enabled: std::collections::HashSet<String>,
    disabled: std::collections::HashSet<String>,
}

impl McpApproval {
    /// Fold in a settings object (or a `~/.claude.json` `projects.<path>`
    /// entry, which additionally carries `disabledMcpServers`).
    fn absorb(&mut self, v: &serde_json::Value, project_entry: bool) {
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        if v.get("enableAllProjectMcpServers")
            .and_then(|b| b.as_bool())
            == Some(true)
        {
            self.enable_all = true;
        }
        self.enabled.extend(strs("enabledMcpjsonServers"));
        self.disabled.extend(strs("disabledMcpjsonServers"));
        if project_entry {
            self.disabled.extend(strs("disabledMcpServers"));
        }
    }

    fn project_server_approved(&self, name: &str) -> bool {
        !self.disabled.contains(name) && (self.enable_all || self.enabled.contains(name))
    }
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
            keep = matches!(key, "name" | "description" | "when_to_use");
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
