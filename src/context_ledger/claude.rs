//! Discovery for the Anthropic Claude Code CLI.
//!
//! Claude loads context from a well-documented set of locations:
//! - ~/.claude/CLAUDE.md — global instructions
//! - `@path` imports inside any of those CLAUDE.md files, recursively (see
//!   [`Imports`])
//! - <project>/CLAUDE.md, <project>/.claude/CLAUDE.md, <project>/CLAUDE.local.md,
//!   and CLAUDE.md / CLAUDE.local.md in every ancestor directory — project
//!   instructions
//! - ~/.claude/skills/*/SKILL.md, and <dir>/.claude/skills/*/SKILL.md for the
//!   project and each ancestor up to its git root — skill definitions; only
//!   the frontmatter `name`/`description` is injected per turn (the body
//!   loads on use). A personal skill wins over a same-named project skill.
//! - ~/.claude/agents/*.md and <project>/.claude/agents/*.md — subagent
//!   definitions (likewise frontmatter only; a project agent wins over a
//!   same-named personal one)
//! - installed plugins (`~/.claude/plugins/installed_plugins.json` ->
//!   each install's `installPath`, e.g. `plugins/cache/<market>/<name>/<ver>`)
//!   that are enabled: their skills, commands and agents (listing text only)
//!   and MCP servers — see [`Plugins`]
//! - MCP servers, each contributing its tool schemas: `~/.claude.json`
//!   (`mcpServers` + `projects.<path>.mcpServers`), `<project>/.mcp.json`,
//!   and (legacy fallback) `~/.claude/settings.json` `mcpServers`

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let home = match crate::paths::user_home() {
        Some(h) => h,
        None => return Ok(items),
    };
    let config_dir = crate::providers::claude::claude_config_dir_override();
    // Claude Code reads CLAUDE.md, skills, agents and plugins from
    // $CLAUDE_CONFIG_DIR when set, else ~/.claude.
    let claude_dir = config_dir.clone().unwrap_or_else(|| home.join(".claude"));
    let (user_json, settings) = user_config_paths(&home, config_dir);

    // Claude Code keys projects by absolute path.
    let proj_abs: Option<PathBuf> =
        project.map(|p| fs::canonicalize(p).unwrap_or_else(|_| absolute_path(p)));
    let user_val = read_json_logged(&user_json);
    let local_proj = match (&user_val, project) {
        (Some(val), Some(given)) => project_keys(proj_abs.as_deref(), given)
            .iter()
            .find_map(|k| val.get("projects").and_then(|p| p.get(k.as_str())))
            .cloned(),
        _ => None,
    };
    let project_flag = |key: &str| {
        local_proj
            .as_ref()
            .and_then(|p| p.get(key))
            .and_then(|b| b.as_bool())
            == Some(true)
    };

    // Instruction files, each with its `@path` imports: the global
    // CLAUDE.md, then the project's ./CLAUDE.md, ./.claude/CLAUDE.md,
    // ./CLAUDE.local.md, plus CLAUDE.md / CLAUDE.local.md in every ancestor.
    let mut roots: Vec<(PathBuf, ItemKind, String)> = vec![(
        claude_dir.join("CLAUDE.md"),
        ItemKind::GlobalInstructions,
        "Global CLAUDE.md".to_string(),
    )];
    if let Some(proj) = &proj_abs {
        for (rel, label) in [
            ("CLAUDE.md", "Project CLAUDE.md"),
            (".claude/CLAUDE.md", "Project .claude/CLAUDE.md"),
            ("CLAUDE.local.md", "Project CLAUDE.local.md"),
        ] {
            roots.push((proj.join(rel), ItemKind::ProjectInstructions, label.into()));
        }
        for dir in proj.ancestors().skip(1) {
            for f in ["CLAUDE.md", "CLAUDE.local.md"] {
                roots.push((
                    dir.join(f),
                    ItemKind::ProjectInstructions,
                    format!("{f} ({})", dir.display()),
                ));
            }
        }
    }
    let mut imports = Imports {
        home: &home,
        working_dir: proj_abs.as_deref(),
        external_approved: project_flag("hasClaudeMdExternalIncludesApproved"),
        // A file loaded in its own right is never counted again as an import.
        seen: roots
            .iter()
            .filter_map(|(p, _, _)| fs::canonicalize(p).ok())
            .collect(),
    };
    for (path, kind, label) in roots {
        let before = items.len();
        push_file_if_exists(&mut items, path.clone(), kind, &label);
        if items.len() > before {
            let user_scope = kind == ItemKind::GlobalInstructions;
            imports.expand(&mut items, &path, kind, user_scope, 1);
        }
    }

    // Skills — ~/.claude/skills/<name>/SKILL.md, then project-scope
    // .claude/skills from the project up to its git root (personal wins).
    let mut skill_dirs = vec![claude_dir.join("skills")];
    if let Some(proj) = &proj_abs {
        skill_dirs.extend(
            super::dirs_up_to_git_root(proj)
                .into_iter()
                .map(|d| d.join(".claude").join("skills")),
        );
    }
    let mut seen_skills = std::collections::HashSet::new();
    for skills_dir in skill_dirs {
        let Ok(entries) = fs::read_dir(&skills_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let skill_md = entry.path().join("SKILL.md");
            if seen_skills.contains(&name) || !skill_md.is_file() {
                continue;
            }
            seen_skills.insert(name.clone());
            push_frontmatter_if_exists(
                &mut items,
                skill_md,
                ItemKind::Skill,
                &format!("skill: {}", name),
            );
        }
    }

    // Subagents — <project>/.claude/agents/*.md, then ~/.claude/agents/*.md
    // (a project agent overrides a same-named personal one).
    let mut agent_dirs = Vec::new();
    if let Some(proj) = &proj_abs {
        agent_dirs.push(proj.join(".claude").join("agents"));
    }
    agent_dirs.push(claude_dir.join("agents"));
    let mut seen_agents = std::collections::HashSet::new();
    for agents_dir in agent_dirs {
        let Ok(entries) = fs::read_dir(&agents_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "agent".to_string());
                if !seen_agents.insert(name.clone()) {
                    continue;
                }
                push_frontmatter_if_exists(
                    &mut items,
                    path,
                    ItemKind::SubagentDef,
                    &format!("agent: {}", name),
                );
            }
        }
    }

    // MCP tools. Claude Code keeps user/local-scope servers in ~/.claude.json
    // and project-scope servers in <project>/.mcp.json; settings.json
    // `mcpServers` is only a legacy fallback. First definition of a name wins
    // (local > project > user > legacy).
    let mut sources: Vec<(PathBuf, Vec<(String, serde_json::Value)>)> = Vec::new();
    // Repo-controlled settings may approve .mcp.json servers only when the
    // user has trusted this folder in Claude Code.
    let trusted = project_flag("hasTrustDialogAccepted");
    if let Some(local) = local_proj
        .as_ref()
        .and_then(|p| p.get("mcpServers"))
        .and_then(|m| m.as_object())
    {
        sources.push((user_json.clone(), object_entries(local)));
    }
    let mut approval = McpApproval::default();
    if let Some(lp) = &local_proj {
        approval.absorb(lp, true, true);
    }
    let settings_val = read_json_logged(&settings);
    if let Some(v) = &settings_val {
        approval.absorb(v, false, true);
    }
    // `enabledPlugins` merges key by key, user < project < local; repo
    // settings count only in a trusted folder.
    let mut enabled_plugins = std::collections::HashMap::new();
    let mut absorb_enabled = |v: &serde_json::Value| {
        if let Some(m) = v.get("enabledPlugins").and_then(|m| m.as_object()) {
            for (id, on) in m {
                if let Some(on) = on.as_bool() {
                    enabled_plugins.insert(id.clone(), on);
                }
            }
        }
    };
    if let Some(v) = &settings_val {
        absorb_enabled(v);
    }
    if let Some(proj) = &proj_abs {
        for f in ["settings.json", "settings.local.json"] {
            if let Some(v) = read_json_logged(&proj.join(".claude").join(f)) {
                approval.absorb(&v, false, trusted);
                if trusted {
                    absorb_enabled(&v);
                }
            }
        }
        let mcp_json = proj.join(".mcp.json");
        if let Some(val) = read_json_logged(&mcp_json) {
            if let Some(m) = val.get("mcpServers").and_then(|m| m.as_object()) {
                let mut approved = Vec::new();
                for (name, cfg) in m {
                    if trusted && approval.project_server_approved(name) {
                        approved.push((name.clone(), cfg.clone()));
                    } else if !trusted {
                        ledger_warn!(
                            "mcp {}: not started (folder not trusted in Claude Code)",
                            printable(name)
                        );
                    } else {
                        // Never execute repo-controlled commands Claude Code
                        // itself would not start.
                        ledger_warn!("mcp {}: not approved (not started)", printable(name));
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
    // Plugins: each installed, enabled plugin's skills, commands and agents
    // (their listing text) plus its MCP servers.
    let plugins = Plugins {
        root: crate::paths::path_env_var("CLAUDE_CODE_PLUGIN_CACHE_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| claude_dir.join("plugins")),
        project: proj_abs.as_deref(),
        trusted,
        enabled: &enabled_plugins,
        approval: &approval,
    };
    for (path, name, cfg) in plugins.collect(&mut items) {
        sources.push((path, vec![(name, cfg)]));
    }
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    let mut origin = std::collections::HashMap::new();
    for (path, entries) in sources {
        for (name, cfg) in entries {
            if approval.project_disabled.contains(&name) {
                ledger_warn!(
                    "mcp {}: disabled for this project (not started)",
                    printable(&name)
                );
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
                    name: format!("mcp: {}", printable(&name)),
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
                ledger_warn!("mcp {} skipped: {}", printable(&name), err);
            }
        }
    }

    Ok(items)
}

/// Installed Claude Code plugins, read as Claude Code loads them: the install
/// records in `<plugins root>/installed_plugins.json` (an id maps to one or
/// more installs, each with a `scope`, `installPath` and, for `project` /
/// `local` installs, a `projectPath`), filtered by `enabledPlugins` (else the
/// manifest's `defaultEnabled`, default on). A plugin root holds an optional
/// `.claude-plugin/plugin.json` manifest and the standard layout: `skills/`
/// (plus manifest `skills` dirs), `commands/` and `agents/` (each replaced by
/// its manifest key), `.mcp.json` (merged with manifest `mcpServers`).
/// Components are namespaced `<plugin>:<name>`; a component path must start
/// with `./` and stay inside the plugin root.
struct Plugins<'a> {
    root: PathBuf,
    project: Option<&'a Path>,
    trusted: bool,
    enabled: &'a std::collections::HashMap<String, bool>,
    approval: &'a McpApproval,
}

/// Bound on agent subfolder depth (agent names include their subfolders).
const MAX_AGENT_DEPTH: usize = 4;

impl Plugins<'_> {
    /// Push every enabled plugin's listing items; return its MCP servers as
    /// `(origin file, "plugin:<plugin>:<server>", config)`.
    fn collect(&self, items: &mut Vec<LedgerItem>) -> Vec<(PathBuf, String, serde_json::Value)> {
        let mut servers = Vec::new();
        for (id, root, project_scoped) in self.installs() {
            if project_scoped && !self.trusted {
                ledger_warn!(
                    "plugin {}: not loaded (folder not trusted in Claude Code)",
                    printable(&id)
                );
                continue;
            }
            servers.extend(self.collect_plugin(items, &id, &root, project_scoped));
        }
        servers
    }

    /// `(id, install root, project-scoped)` for each plugin installed for this
    /// context: a `local` install for this project beats a `project` one,
    /// which beats the `user` install.
    fn installs(&self) -> Vec<(String, PathBuf, bool)> {
        let path = self.root.join("installed_plugins.json");
        let Some(v) = read_json_logged(&path) else {
            return Vec::new();
        };
        let Some(map) = v.get("plugins").and_then(|m| m.as_object()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, entry) in map {
            let installs: Vec<&serde_json::Value> = match entry {
                serde_json::Value::Array(a) => a.iter().collect(),
                serde_json::Value::Object(_) => vec![entry],
                _ => Vec::new(),
            };
            let pick = installs
                .into_iter()
                .filter_map(|i| {
                    let install = PathBuf::from(i.get("installPath")?.as_str()?);
                    let scope = i.get("scope").and_then(|s| s.as_str()).unwrap_or("user");
                    let rank = match scope {
                        "local" | "project" => {
                            let pp = Path::new(i.get("projectPath")?.as_str()?);
                            let proj = self.project?;
                            let same = pp == proj
                                || fs::canonicalize(pp).is_ok_and(|c| c.as_path() == proj);
                            if !same {
                                return None;
                            }
                            usize::from(scope == "project")
                        }
                        _ => 2,
                    };
                    Some((rank, install))
                })
                .min_by_key(|(rank, _)| *rank);
            if let Some((rank, install)) = pick {
                out.push((id.clone(), install, rank < 2));
            }
        }
        out
    }

    fn collect_plugin(
        &self,
        items: &mut Vec<LedgerItem>,
        id: &str,
        root: &Path,
        project_scoped: bool,
    ) -> Vec<(PathBuf, String, serde_json::Value)> {
        let Ok(canon_root) = fs::canonicalize(root) else {
            ledger_warn!("plugin {}: not cached at {}", printable(id), root.display());
            return Vec::new();
        };
        let manifest_path = root.join(".claude-plugin").join("plugin.json");
        let manifest = read_json_logged(&manifest_path).unwrap_or(serde_json::Value::Null);
        let default_on = manifest
            .get("defaultEnabled")
            .and_then(|b| b.as_bool())
            .unwrap_or(true);
        if !self.enabled.get(id).copied().unwrap_or(default_on) {
            return Vec::new();
        }
        let name = manifest
            .get("name")
            .and_then(|n| n.as_str())
            .map(String::from)
            .unwrap_or_else(|| id.split('@').next().unwrap_or(id).to_string());
        let component = |rel: &str| plugin_component(&canon_root, rel);
        let declared = |key: &str| -> Option<Vec<String>> {
            match manifest.get(key)? {
                serde_json::Value::String(s) => Some(vec![s.clone()]),
                serde_json::Value::Array(a) => Some(
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                ),
                _ => None,
            }
        };

        // Skills: skills/ plus manifest dirs (a dir of <name>/SKILL.md, or
        // one folder holding SKILL.md). A root SKILL.md alone is one skill.
        let mut skill_dirs = vec![(canon_root.join("skills"), None)];
        for rel in declared("skills").unwrap_or_default() {
            if let Some(d) = component(&rel) {
                skill_dirs.push((d, Some(rel)));
            }
        }
        let mut seen_skills = std::collections::HashSet::new();
        for (dir, rel) in &skill_dirs {
            if dir.join("SKILL.md").is_file() && rel.is_some() {
                let skill = if *dir == canon_root {
                    name.clone()
                } else {
                    dir.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                };
                if seen_skills.insert(skill.clone()) {
                    push_frontmatter_if_exists(
                        items,
                        dir.join("SKILL.md"),
                        ItemKind::PluginManifest,
                        &format!("plugin skill: {}:{}", name, printable(&skill)),
                    );
                }
                continue;
            }
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                let skill_md = path.join("SKILL.md");
                let skill = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if skill_md.is_file() && seen_skills.insert(skill.clone()) {
                    push_frontmatter_if_exists(
                        items,
                        skill_md,
                        ItemKind::PluginManifest,
                        &format!("plugin skill: {}:{}", name, printable(&skill)),
                    );
                }
            }
        }
        if !canon_root.join("skills").exists()
            && manifest.get("skills").is_none()
            && canon_root.join("SKILL.md").is_file()
        {
            push_frontmatter_if_exists(
                items,
                canon_root.join("SKILL.md"),
                ItemKind::PluginManifest,
                &format!("plugin skill: {}", name),
            );
        }

        // Commands: manifest `commands` (paths or a name map) replaces the
        // default commands/ scan.
        let mut command_files: Vec<PathBuf> = Vec::new();
        match manifest.get("commands") {
            Some(serde_json::Value::Object(map)) => {
                for (cmd, entry) in map {
                    let label = format!("plugin command: {}:{}", name, printable(cmd));
                    if let Some(src) = entry.get("source").and_then(|s| s.as_str()) {
                        if let Some(f) = component(src) {
                            push_listing_text(items, f, &label, entry.get("description"));
                        }
                    } else {
                        let text = entry
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(String::from)
                            .or_else(|| {
                                entry
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .and_then(first_line)
                            })
                            .unwrap_or_default();
                        push_text(items, &manifest_path, &label, &text);
                    }
                }
            }
            _ => {
                let rels = declared("commands").unwrap_or_else(|| vec!["./commands".into()]);
                for rel in rels {
                    let Some(p) = component(&rel) else { continue };
                    if p.is_dir() {
                        command_files.extend(md_files(&p, 0, 0));
                    } else if p.is_file() {
                        command_files.push(p);
                    }
                }
            }
        }
        for f in command_files {
            let cmd = f
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let label = format!("plugin command: {}:{}", name, printable(&cmd));
            push_listing_text(items, f, &label, None);
        }

        // Agents: manifest `agents` (.md files) replaces the default agents/
        // scan, where subfolders become part of the agent name.
        let agent_files: Vec<(PathBuf, String)> = match declared("agents") {
            Some(rels) => rels
                .iter()
                .filter_map(|r| component(r))
                .filter(|p| p.is_file())
                .map(|p| {
                    let stem = p
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    (p, stem)
                })
                .collect(),
            None => {
                let base = canon_root.join("agents");
                md_files(&base, 0, MAX_AGENT_DEPTH)
                    .into_iter()
                    .map(|p| {
                        let rel = p.strip_prefix(&base).unwrap_or(&p).with_extension("");
                        let agent = rel
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(":");
                        (p, agent)
                    })
                    .collect()
            }
        };
        for (p, agent) in agent_files {
            push_frontmatter_if_exists(
                items,
                p,
                ItemKind::PluginManifest,
                &format!("plugin agent: {}:{}", name, printable(&agent)),
            );
        }

        // MCP servers: .mcp.json, then manifest `mcpServers` (file paths,
        // inline maps, or an array of them); a later name replaces an earlier.
        let mut defs: Vec<(PathBuf, serde_json::Map<String, serde_json::Value>)> = Vec::new();
        let mcp_file = |p: &Path| {
            let v = read_json_logged(p)?;
            let m = v.get("mcpServers").cloned().unwrap_or(v);
            m.as_object().cloned()
        };
        if let Some(m) = mcp_file(&canon_root.join(".mcp.json")) {
            defs.push((root.join(".mcp.json"), m));
        }
        let declared_mcp: Vec<serde_json::Value> = match manifest.get("mcpServers") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(v) => vec![v.clone()],
            None => Vec::new(),
        };
        for d in declared_mcp {
            match d {
                serde_json::Value::String(rel) if rel.ends_with(".json") => {
                    if let Some(p) = component(&rel) {
                        if let Some(m) = mcp_file(&p) {
                            defs.push((p, m));
                        }
                    }
                }
                serde_json::Value::String(rel) => {
                    ledger_warn!("plugin {}: MCP bundle {} not probed", name, printable(&rel))
                }
                serde_json::Value::Object(m) => defs.push((manifest_path.clone(), m)),
                _ => {}
            }
        }
        let data_dir = self.root.join("data").join(
            id.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>(),
        );
        let mut vars = vec![
            (
                "CLAUDE_PLUGIN_ROOT",
                canon_root.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_PLUGIN_DATA",
                data_dir.to_string_lossy().into_owned(),
            ),
        ];
        if let Some(p) = self.project {
            vars.push(("CLAUDE_PROJECT_DIR", p.to_string_lossy().into_owned()));
        }
        let mut merged: Vec<(String, (PathBuf, serde_json::Value))> = Vec::new();
        for (origin, map) in defs {
            for (server, cfg) in map {
                merged.retain(|(n, _)| *n != server);
                merged.push((server, (origin.clone(), cfg)));
            }
        }
        let mut out = Vec::new();
        for (server, (origin, mut cfg)) in merged {
            if project_scoped && !self.approval.project_server_approved(&server) {
                ledger_warn!(
                    "mcp plugin:{}:{}: not approved (not started)",
                    name,
                    printable(&server)
                );
                continue;
            }
            substitute_plugin_vars(&mut cfg, &vars);
            if let Some(obj) = cfg.as_object_mut() {
                if obj.contains_key("command") {
                    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
                    if let Some(env) = env.as_object_mut() {
                        for (k, v) in &vars {
                            env.entry(k.to_string()).or_insert_with(|| v.clone().into());
                        }
                    }
                }
            }
            out.push((origin, format!("plugin:{name}:{server}"), cfg));
        }
        out
    }
}

/// A manifest component path, resolved inside the plugin root: it must start
/// with `./` (or be `.`), and may not escape the root (`..`, symlinks out,
/// or — off Windows — a backslash anywhere).
fn plugin_component(canon_root: &Path, rel: &str) -> Option<PathBuf> {
    let inner = if rel == "." || rel == "./" {
        ""
    } else {
        rel.strip_prefix("./")?
    };
    if (cfg!(not(windows)) && rel.contains('\\'))
        || Path::new(inner)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let p = fs::canonicalize(canon_root.join(inner)).ok()?;
    p.starts_with(canon_root).then_some(p)
}

/// `.md` files under `dir` (sorted), descending at most `max_depth` levels.
fn md_files(dir: &Path, depth: usize, max_depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            if depth < max_depth {
                out.extend(md_files(&p, depth + 1, max_depth));
            }
        } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(p);
        }
    }
    out
}

/// Replace `${CLAUDE_PLUGIN_ROOT}`-style references in every string of an MCP
/// server config (Claude Code resolves them in command, args, env and url).
fn substitute_plugin_vars(v: &mut serde_json::Value, vars: &[(&str, String)]) {
    match v {
        serde_json::Value::String(s) => {
            for (k, val) in vars {
                *s = s.replace(&format!("${{{k}}}"), val);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| substitute_plugin_vars(x, vars)),
        serde_json::Value::Object(o) => {
            o.values_mut().for_each(|x| substitute_plugin_vars(x, vars))
        }
        _ => {}
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

/// A command's listing text: its `description` override, else its
/// frontmatter, else its first non-empty line.
fn push_listing_text(
    items: &mut Vec<LedgerItem>,
    path: PathBuf,
    label: &str,
    description: Option<&serde_json::Value>,
) {
    if let Some(d) = description.and_then(|d| d.as_str()) {
        push_text(items, &path, label, d);
        return;
    }
    let Some(bytes) = super::read_capped(&path) else {
        return;
    };
    let full = String::from_utf8_lossy(&bytes);
    let text = injected_frontmatter(&full)
        .or_else(|| first_line(&full))
        .unwrap_or_default();
    push_text(items, &path, label, &text);
}

fn push_text(items: &mut Vec<LedgerItem>, path: &Path, label: &str, text: &str) {
    items.push(LedgerItem {
        kind: ItemKind::PluginManifest,
        name: label.to_string(),
        path: path.to_path_buf(),
        mtime: file_mtime(path),
        token_count: tokenize::count_tokens(text, tokenize::TokenizerHint::Anthropic),
        content_bytes: text.len(),
        tool_count: None,
    });
}

/// Maximum `@path` import depth (hops from the importing CLAUDE.md).
const MAX_IMPORT_HOPS: usize = 4;

/// CLAUDE.md `@path` import expansion, as Claude Code does it: imports are
/// recursive up to [`MAX_IMPORT_HOPS`]; a relative path resolves against the
/// importing file's directory, `~/` against home; every file counts once.
/// In a project-level file, an import resolving outside the working directory
/// is external and loads only once the user approved external imports for
/// the project (`hasClaudeMdExternalIncludesApproved`); imports in the user's
/// own `~/.claude/CLAUDE.md` load without approval.
struct Imports<'a> {
    home: &'a Path,
    working_dir: Option<&'a Path>,
    external_approved: bool,
    seen: std::collections::HashSet<PathBuf>,
}

impl Imports<'_> {
    fn expand(
        &mut self,
        items: &mut Vec<LedgerItem>,
        file: &Path,
        kind: ItemKind,
        user_scope: bool,
        hop: usize,
    ) {
        if hop > MAX_IMPORT_HOPS {
            return;
        }
        let Some(bytes) = super::read_capped(file) else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        let base = file.parent().unwrap_or(Path::new(""));
        for raw in import_paths(&text) {
            let target = if let Some(rest) = raw.strip_prefix("~/") {
                self.home.join(rest)
            } else {
                base.join(&raw)
            };
            let Ok(canon) = fs::canonicalize(&target) else {
                continue;
            };
            if !canon.is_file() || self.seen.contains(&canon) {
                continue;
            }
            let external = self.working_dir.is_some_and(|w| !canon.starts_with(w));
            if !user_scope && external && !self.external_approved {
                ledger_warn!(
                    "{}: import @{} not counted (external imports not approved)",
                    file.display(),
                    printable(&raw)
                );
                continue;
            }
            self.seen.insert(canon);
            let label = format!(
                "@{} (from {})",
                printable(&raw),
                file.file_name().unwrap_or_default().to_string_lossy()
            );
            let before = items.len();
            push_file_if_exists(items, target.clone(), kind, &label);
            if items.len() > before {
                self.expand(items, &target, kind, user_scope, hop + 1);
            }
        }
    }
}

/// The `@path` imports in a CLAUDE.md, in order. An import starts with `@`
/// at the start of a line or after whitespace and ends at the first
/// unescaped whitespace (`\ ` is a literal space). Fenced code blocks and
/// code spans are skipped, and a quoted path is not an import.
fn import_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        let run = |c: char| t.chars().take_while(|&x| x == c).count();
        let opener = ['`', '~']
            .into_iter()
            .map(|c| (c, run(c)))
            .find(|&(_, n)| n >= 3);
        match (fence, opener) {
            (Some((c, n)), Some((c2, n2))) if c == c2 && n2 >= n => {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            (None, Some(f)) => {
                fence = Some(f);
                continue;
            }
            (None, None) => {}
        }
        let chars: Vec<char> = without_code_spans(line).chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
                let mut path = String::new();
                let mut j = i + 1;
                while j < chars.len() {
                    match chars[j] {
                        '\\' if chars.get(j + 1) == Some(&' ') => {
                            path.push(' ');
                            j += 2;
                        }
                        c if c.is_whitespace() => break,
                        c => {
                            path.push(c);
                            j += 1;
                        }
                    }
                }
                if !path.is_empty() && !path.starts_with(['"', '\'']) {
                    out.push(path);
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// `line` with each Markdown code span (a backtick run up to the next run of
/// the same length) blanked out. An unmatched run stays literal.
fn without_code_spans(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let n = chars[i..].iter().take_while(|&&c| c == '`').count();
        let mut j = i + n;
        let mut close = None;
        while j < chars.len() {
            if chars[j] == '`' {
                let m = chars[j..].iter().take_while(|&&c| c == '`').count();
                if m == n {
                    close = Some(j + m);
                    break;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                out.push(' ');
                i = end;
            }
            None => {
                out.extend(&chars[i..i + n]);
                i += n;
            }
        }
    }
    out
}

/// Claude Code's approval state for project-scope `.mcp.json` servers.
#[derive(Default)]
struct McpApproval {
    enable_all: bool,
    enabled: std::collections::HashSet<String>,
    /// `disabledMcpjsonServers`: rejects `.mcp.json` servers only.
    disabled: std::collections::HashSet<String>,
    /// `projects.<path>.disabledMcpServers`: applies across every source.
    project_disabled: std::collections::HashSet<String>,
}

impl McpApproval {
    /// Fold in a settings object (or a `~/.claude.json` `projects.<path>`
    /// entry, which additionally carries `disabledMcpServers`).
    ///
    /// `allow_approvals` is false for repo-controlled files in an untrusted
    /// folder: those may only add to the disabled lists, never approve.
    fn absorb(&mut self, v: &serde_json::Value, project_entry: bool, allow_approvals: bool) {
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
        if allow_approvals {
            if v.get("enableAllProjectMcpServers")
                .and_then(|b| b.as_bool())
                == Some(true)
            {
                self.enable_all = true;
            }
            self.enabled.extend(strs("enabledMcpjsonServers"));
        }
        self.disabled.extend(strs("disabledMcpjsonServers"));
        if project_entry {
            self.project_disabled.extend(strs("disabledMcpServers"));
        }
    }

    fn project_server_approved(&self, name: &str) -> bool {
        !self.disabled.contains(name) && (self.enable_all || self.enabled.contains(name))
    }
}

/// Server names can come from a cloned repo's `.mcp.json`: escape control
/// characters (ESC/OSC sequences) before they reach the terminal.
fn printable(name: &str) -> String {
    name.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// `.claude.json` and `settings.json` locations. Claude Code relocates both
/// under `$CLAUDE_CONFIG_DIR` when it is set (non-empty).
fn user_config_paths(home: &Path, config_dir: Option<PathBuf>) -> (PathBuf, PathBuf) {
    match config_dir.filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => (d.join(".claude.json"), d.join("settings.json")),
        None => (
            home.join(".claude.json"),
            home.join(".claude").join("settings.json"),
        ),
    }
}

/// An absolute spelling of `p` without touching the filesystem.
fn absolute_path(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Strip the Windows verbatim prefix (`\\?\C:\x` -> `C:\x`,
/// `\\?\UNC\h\s` -> `\\h\s`); other paths are returned unchanged.
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Candidate `~/.claude.json` `projects.<path>` keys for a project, most
/// specific first: the canonical path, its non-verbatim form (Windows
/// `canonicalize` yields `\\?\C:\...`, which Claude Code never writes), the
/// forward-slash spelling Claude Code also uses on Windows, and the path as
/// given.
fn project_keys(canonical: Option<&Path>, given: &Path) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut push = |k: String| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    let abs = absolute_path(given);
    for p in canonical.into_iter().chain(std::iter::once(abs.as_path())) {
        let plain = strip_verbatim(&p.to_string_lossy());
        push(p.to_string_lossy().into_owned());
        push(plain.clone());
        if plain.contains('\\') {
            push(plain.replace('\\', "/"));
        }
    }
    push(given.to_string_lossy().into_owned());
    keys
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
                ledger_warn!("could not read {}", path.display());
            }
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            ledger_warn!("could not parse {}: {}", path.display(), e);
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

pub(super) fn push_frontmatter_if_exists(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_dir_relocates_user_json_and_settings() {
        let home = Path::new("/h");
        assert_eq!(
            user_config_paths(home, Some(PathBuf::from("/c"))),
            (
                PathBuf::from("/c/.claude.json"),
                PathBuf::from("/c/settings.json")
            )
        );
        for unset in [None, Some(PathBuf::new())] {
            assert_eq!(
                user_config_paths(home, unset),
                (
                    PathBuf::from("/h/.claude.json"),
                    PathBuf::from("/h/.claude/settings.json")
                )
            );
        }
    }

    #[test]
    fn exported_claude_config_dir_is_ignored_unless_scoped_by_the_test() {
        let _home = crate::store::ScopedConfigDir::new();
        let cfg = tempfile::tempdir().unwrap();
        let marker = cfg.path().join("ran");
        std::fs::write(
            cfg.path().join(".claude.json"),
            serde_json::json!({"mcpServers":{"x":{
                "command":"/usr/bin/touch","args":[marker.to_str().unwrap()]}}})
            .to_string(),
        )
        .unwrap();
        let _l = crate::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct Unset;
        impl Drop for Unset {
            fn drop(&mut self) {
                #[allow(clippy::disallowed_methods)]
                std::env::remove_var("CLAUDE_CONFIG_DIR");
            }
        }
        #[allow(clippy::disallowed_methods)]
        std::env::set_var("CLAUDE_CONFIG_DIR", cfg.path());
        let _u = Unset;
        let _ = collect(None);
        assert!(
            !marker.exists(),
            "ledger executed an MCP server from an unscoped CLAUDE_CONFIG_DIR"
        );
    }

    #[test]
    fn scoped_claude_config_dir_relocates_claude_md() {
        let _home = crate::store::ScopedConfigDir::new();
        let cfg = tempfile::tempdir().unwrap();
        std::fs::write(cfg.path().join("CLAUDE.md"), "hello from cfg dir").unwrap();
        let d = cfg.path().to_str().unwrap().to_string();
        let items = crate::env_lock::scoped_env_var("CLAUDE_CONFIG_DIR", Some(&d), || {
            collect(None).unwrap()
        });
        assert!(items.iter().any(|i| i.name == "Global CLAUDE.md"));
    }

    #[test]
    fn import_paths_follow_claude_code_syntax() {
        let md = "See @docs/a.md and @~/b.md.\n\
                  mail me@example.com\n\
                  `@not-span.md` and ``x `@n` y`` but @real\\ name.md\n\
                  @\"quoted.md\"\n\
                  ```\n@in-fence.md\n```\n\
                  ~~~~\n@tilde-fence.md\n~~~\n@still-fenced.md\n~~~~\n\
                  @after.md";
        assert_eq!(
            import_paths(md),
            vec!["docs/a.md", "~/b.md.", "real name.md", "after.md"]
        );
    }

    #[test]
    fn plugin_component_paths_stay_inside_the_plugin() {
        let td = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(td.path()).unwrap().join("p");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(td.path().join("outside.md"), "x").unwrap();
        assert_eq!(plugin_component(&root, "./a"), Some(root.join("a")));
        assert_eq!(plugin_component(&root, "."), Some(root.clone()));
        assert_eq!(plugin_component(&root, "a"), None, "needs ./");
        assert_eq!(plugin_component(&root, "./../outside.md"), None);
        assert_eq!(plugin_component(&root, "./a/../../outside.md"), None);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(td.path().join("outside.md"), root.join("link.md")).unwrap();
            assert_eq!(plugin_component(&root, "./link.md"), None, "symlink escape");
        }
    }

    #[test]
    fn printable_escapes_terminal_control_sequences() {
        assert_eq!(printable("a\u{1b}]52;c;x\u{7}b"), "a\\u{1b}]52;c;x\\u{7}b");
        assert_eq!(printable("plain-name_1"), "plain-name_1");
    }

    #[test]
    fn windows_verbatim_project_path_maps_to_plain_keys() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\me\proj"), r"C:\Users\me\proj");
        assert_eq!(strip_verbatim(r"\\?\UNC\host\share\p"), r"\\host\share\p");
        assert_eq!(strip_verbatim("/home/me/p"), "/home/me/p");
        let keys = project_keys(Some(Path::new(r"\\?\C:\Users\me\proj")), Path::new("proj"));
        assert!(keys.contains(&r"C:\Users\me\proj".to_string()), "{keys:?}");
        assert!(keys.contains(&"C:/Users/me/proj".to_string()), "{keys:?}");
    }
}
