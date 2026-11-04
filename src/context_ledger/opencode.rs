//! Discovery for the opencode CLI (SST).
//!
//! Opencode context sources (`<cfg>` is `$XDG_CONFIG_HOME/opencode`, else
//! `~/.config/opencode`):
//! - config, merged the way opencode merges it (see [`config_layers`]):
//!   <cfg>/config.json, opencode.json, opencode.jsonc, `$OPENCODE_CONFIG`,
//!   project and `.opencode` opencode.json[c], `$OPENCODE_CONFIG_DIR`,
//!   `$OPENCODE_CONFIG_CONTENT` — with optional `prompt` / `rules` strings, an
//!   `instructions` list of file paths, and an `mcp` server map
//! - <cfg>/AGENTS.md — global rules
//! - <cfg>/agent/*.md — subagent definitions; only the frontmatter
//!   description is injected per turn (the body is the agent's own prompt)
//! - project rules: every AGENTS.md from <project> up to its git root (else
//!   CLAUDE.md, else CONTEXT.md, found the same way)
//!
//! `.jsonc` may contain comments and trailing commas — strip both before
//! parsing.

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let Some(home) = crate::paths::user_home() else {
        return Ok(items);
    };
    let cfg_dir =
        config_base(crate::paths::path_env_var("XDG_CONFIG_HOME"), &home).join("opencode");

    // Global rules file (opencode reads ~/.config/opencode/AGENTS.md).
    push_file_if_exists(
        &mut items,
        cfg_dir.join("AGENTS.md"),
        ItemKind::GlobalInstructions,
        "Global AGENTS.md",
    );

    // Every config source opencode merges, lowest precedence first.
    let mut merged = Merged::default();
    for layer in config_layers(&cfg_dir, &home, project) {
        merged.apply(&layer);
    }
    collect_from_config(&mut items, &merged, project, &home);

    // Subagents — ~/.config/opencode/agent/*.md
    let agents_dir = cfg_dir.join("agent");
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "agent".to_string());
                super::claude::push_frontmatter_if_exists(
                    &mut items,
                    path,
                    ItemKind::SubagentDef,
                    &format!("agent: {}", name),
                );
            }
        }
    }

    // Project rules: every AGENTS.md from the project up to its git root;
    // CLAUDE.md, then CONTEXT.md, only when no AGENTS.md is found.
    if let Some(proj) = project {
        let proj = fs::canonicalize(proj).unwrap_or_else(|_| proj.to_path_buf());
        let dirs = super::dirs_up_to_git_root(&proj);
        for fname in ["AGENTS.md", "CLAUDE.md", "CONTEXT.md"] {
            let before = items.len();
            for dir in &dirs {
                let label = if *dir == proj.as_path() {
                    format!("Project {fname}")
                } else {
                    format!("{fname} ({})", dir.display())
                };
                push_file_if_exists(
                    &mut items,
                    dir.join(fname),
                    ItemKind::ProjectInstructions,
                    &label,
                );
            }
            if items.len() > before {
                break;
            }
        }
    }

    Ok(items)
}

/// opencode's global config base: `$XDG_CONFIG_HOME` when set to an absolute
/// path, else `~/.config` (as its xdg-basedir lookup does).
fn config_base(xdg: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    match xdg.map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => home.join(".config"),
    }
}

/// One opencode config source: a file, or inline `$OPENCODE_CONFIG_CONTENT`.
struct Layer {
    path: PathBuf,
    val: serde_json::Value,
    /// opencode concatenates `instructions` across merge steps, but the three
    /// global files merge among themselves with plain replacement.
    concat_instructions: bool,
}

/// The merged opencode config plus, for each counted value, the source that
/// last set it (the ledger row's path).
#[derive(Default)]
struct Merged {
    val: serde_json::Value,
    origin: std::collections::HashMap<String, PathBuf>,
    instructions: Vec<(String, PathBuf)>,
}

impl Merged {
    fn apply(&mut self, layer: &Layer) {
        let Some(obj) = layer.val.as_object() else {
            ledger_warn!("{}: config is not a JSON object", layer.path.display());
            return;
        };
        let mut rest = obj.clone();
        if let Some(list) = rest.remove("instructions") {
            let entries: Vec<String> = list
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if !layer.concat_instructions {
                self.instructions.clear();
            }
            for e in entries {
                if !self.instructions.iter().any(|(x, _)| *x == e) {
                    self.instructions.push((e, layer.path.clone()));
                }
            }
        }
        for key in ["prompt", "rules"] {
            if rest.contains_key(key) {
                self.origin.insert(key.into(), layer.path.clone());
            }
        }
        if let Some(servers) = rest.get("mcp").and_then(|m| m.as_object()) {
            for name in servers.keys() {
                self.origin
                    .insert(format!("mcp/{name}"), layer.path.clone());
            }
        }
        if !self.val.is_object() {
            self.val = serde_json::Value::Object(Default::default());
        }
        deep_merge(&mut self.val, &serde_json::Value::Object(rest));
    }

    fn origin(&self, key: &str) -> PathBuf {
        self.origin.get(key).cloned().unwrap_or_default()
    }
}

/// opencode's `mergeDeep`: objects merge key by key, recursively; anything
/// else (arrays included) from `source` replaces `target`.
fn deep_merge(target: &mut serde_json::Value, source: &serde_json::Value) {
    match (target, source) {
        (serde_json::Value::Object(t), serde_json::Value::Object(s)) => {
            for (k, v) in s {
                match t.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        deep_merge(existing, v)
                    }
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (t, s) => *t = s.clone(),
    }
}

/// opencode's truthy env flag: `true` (any case) or `1`.
fn env_flag(key: &str) -> bool {
    crate::paths::path_env_var(key)
        .and_then(|v| v.into_string().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

fn env_path(key: &str) -> Option<PathBuf> {
    crate::paths::path_env_var(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The config sources opencode merges, lowest precedence first (its
/// `Config.load`): the global `config.json`, `opencode.json`,
/// `opencode.jsonc`; `$OPENCODE_CONFIG`; project `opencode.json[c]` from the
/// worktree root down to the project (a directory's `.jsonc` over its
/// `.json`); `opencode.json[c]` in each `.opencode` directory from the
/// project up to the worktree, `~/.opencode` and `$OPENCODE_CONFIG_DIR`; and
/// finally `$OPENCODE_CONFIG_CONTENT`. `$OPENCODE_DISABLE_PROJECT_CONFIG`
/// drops the project sources. The worktree is the git root, or `/` outside a
/// repository.
fn config_layers(cfg_dir: &Path, home: &Path, project: Option<&Path>) -> Vec<Layer> {
    let mut files: Vec<(PathBuf, bool)> = ["config.json", "opencode.json", "opencode.jsonc"]
        .iter()
        .map(|n| (cfg_dir.join(n), false))
        .collect();
    if let Some(p) = env_path("OPENCODE_CONFIG") {
        files.push((p, true));
    }
    let project_enabled = !env_flag("OPENCODE_DISABLE_PROJECT_CONFIG");
    let proj = project.map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    let walk: Vec<&Path> = match &proj {
        Some(p) if project_enabled => {
            let root = p.ancestors().find(|d| d.join(".git").exists());
            let mut dirs: Vec<&Path> = Vec::new();
            for d in p.ancestors() {
                dirs.push(d);
                if Some(d) == root {
                    break;
                }
            }
            dirs
        }
        _ => Vec::new(),
    };
    let mut project_files: Vec<PathBuf> = walk
        .iter()
        .flat_map(|d| [d.join("opencode.jsonc"), d.join("opencode.json")])
        .filter(|f| f.is_file())
        .collect();
    project_files.reverse();
    files.extend(project_files.into_iter().map(|f| (f, true)));

    let config_dir_env = env_path("OPENCODE_CONFIG_DIR");
    let mut dirs: Vec<PathBuf> = vec![cfg_dir.to_path_buf()];
    let mut push_dir = |d: PathBuf| {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    for d in &walk {
        let dot = d.join(".opencode");
        if dot.exists() {
            push_dir(dot);
        }
    }
    if home.join(".opencode").exists() {
        push_dir(home.join(".opencode"));
    }
    if let Some(d) = &config_dir_env {
        push_dir(d.clone());
    }
    for d in &dirs {
        if d.ends_with(".opencode") || Some(d) == config_dir_env.as_ref() {
            for n in ["opencode.json", "opencode.jsonc"] {
                files.push((d.join(n), true));
            }
        }
    }

    let mut layers: Vec<Layer> = files
        .into_iter()
        .filter_map(|(path, concat_instructions)| {
            read_config(&path).map(|val| Layer {
                path,
                val,
                concat_instructions,
            })
        })
        .collect();
    if let Some(content) =
        crate::paths::path_env_var("OPENCODE_CONFIG_CONTENT").and_then(|c| c.into_string().ok())
    {
        let path = PathBuf::from("$OPENCODE_CONFIG_CONTENT");
        if let Some(val) = parse_config(&content, &path) {
            layers.push(Layer {
                path,
                val,
                concat_instructions: true,
            });
        }
    }
    layers
}

/// Read and parse one JSONC config file. Missing -> `None` silently;
/// unreadable or malformed -> `None` with a diagnostic.
fn read_config(path: &Path) -> Option<serde_json::Value> {
    if !path.exists() {
        return None;
    }
    let read = super::read_capped_max(path, super::MAX_CONFIG_FILE_BYTES)
        .ok_or_else(|| "not a regular file or unreadable".to_string())
        .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()));
    match read {
        Err(e) => {
            ledger_warn!("could not read {}: {}", path.display(), e);
            None
        }
        Ok(raw) => parse_config(&raw, path),
    }
}

fn parse_config(raw: &str, path: &Path) -> Option<serde_json::Value> {
    let stripped = strip_jsonc_trailing_commas(&strip_jsonc_comments(raw));
    match serde_json::from_str::<serde_json::Value>(&stripped) {
        Ok(v) => Some(v),
        Err(e) => {
            ledger_warn!("could not parse {}: {}", path.display(), e);
            None
        }
    }
}

fn collect_from_config(
    items: &mut Vec<LedgerItem>,
    merged: &Merged,
    project: Option<&Path>,
    home: &Path,
) {
    let val = &merged.val;
    // `prompt` string
    if let Some(prompt) = val.get("prompt").and_then(|v| v.as_str()) {
        let config_path = merged.origin("prompt");
        let tokens = tokenize::count_tokens(prompt, tokenize::TokenizerHint::Anthropic);
        items.push(LedgerItem {
            kind: ItemKind::Other,
            name: "opencode prompt".into(),
            mtime: file_mtime(&config_path),
            path: config_path,
            token_count: tokens,
            content_bytes: prompt.len(),
            tool_count: None,
        });
    }

    // `rules` array of strings
    if let Some(rules) = val.get("rules").and_then(|v| v.as_array()) {
        let joined = rules
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            let config_path = merged.origin("rules");
            let tokens = tokenize::count_tokens(&joined, tokenize::TokenizerHint::Anthropic);
            items.push(LedgerItem {
                kind: ItemKind::GlobalInstructions,
                name: "opencode rules".into(),
                mtime: file_mtime(&config_path),
                path: config_path,
                token_count: tokens,
                content_bytes: joined.len(),
                tool_count: None,
            });
        }
    }

    // `instructions` — list of instruction FILE paths opencode reads into
    // context, concatenated across config sources. Plain paths only; globs
    // and URLs are not expanded here.
    for (entry, _) in &merged.instructions {
        let entry = entry.as_str();
        if entry.contains(['*', '?', '[']) || entry.starts_with("http") {
            ledger_warn!("opencode instructions entry '{}' skipped (glob/URL)", entry);
            continue;
        }
        let path = if let Some(rest) = entry.strip_prefix("~/") {
            home.join(rest)
        } else if Path::new(entry).is_absolute() {
            PathBuf::from(entry)
        } else if let Some(proj) = project {
            proj.join(entry)
        } else {
            continue;
        };
        push_file_if_exists(
            items,
            path,
            ItemKind::GlobalInstructions,
            &format!("instructions: {}", entry),
        );
    }

    // `mcp` — server map. opencode's shape differs from Claude's; translate.
    if let Some(servers) = val.get("mcp").and_then(|v| v.as_object()) {
        let servers: Vec<(String, serde_json::Value)> = servers
            .iter()
            .filter_map(|(name, cfg)| translate_mcp_entry(cfg).map(|c| (name.clone(), c)))
            .collect();
        for (name, result) in super::probe_servers(servers) {
            match result {
                Ok(summary) => {
                    let config_path = merged.origin(&format!("mcp/{name}"));
                    items.push(LedgerItem {
                        kind: ItemKind::McpTools,
                        name: format!("mcp: {}", name),
                        mtime: file_mtime(&config_path),
                        path: config_path,
                        token_count: summary.token_count,
                        content_bytes: summary.byte_count,
                        tool_count: Some(summary.tool_count),
                    })
                }
                Err(err) => ledger_warn!("opencode mcp {} skipped: {}", name, err),
            }
        }
    }
}

fn v_or_null(o: &serde_json::Map<String, serde_json::Value>, k: &str) -> serde_json::Value {
    o.get(k).cloned().unwrap_or(serde_json::Value::Null)
}

/// Expand opencode's `{env:VAR}` substitution (unset -> empty string, as
/// opencode does).
pub(crate) fn expand_opencode_env(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(i) = rest.find("{env:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 5..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Convert an opencode `mcp` entry to the Claude-style `{command, args, env}`
/// (or `{url}`) shape `mcp::claude_launch` understands. opencode local servers
/// use `command` as an argv ARRAY and `environment` for env vars; entries with
/// `"enabled": false` are not loaded at all (returns `None`). Entries already
/// in Claude shape (string `command`) pass through unchanged.
fn translate_mcp_entry(cfg: &serde_json::Value) -> Option<serde_json::Value> {
    if cfg.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
        return None;
    }
    if let Some(argv) = cfg.get("command").and_then(|c| c.as_array()) {
        let argv: Vec<&str> = argv.iter().filter_map(|a| a.as_str()).collect();
        let (cmd, args) = argv.split_first()?;
        let args: Vec<String> = args.iter().map(|a| expand_opencode_env(a)).collect();
        let env: serde_json::Map<String, serde_json::Value> = cfg
            .get("environment")
            .and_then(|e| e.as_object())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        let v = v.as_str().map(expand_opencode_env).map(Into::into);
                        (k.clone(), v.unwrap_or_else(|| v_or_null(o, k)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Some(serde_json::json!({
            "command": expand_opencode_env(cmd),
            "args": args,
            "env": env,
        }));
    }
    match cfg.get("type").and_then(|t| t.as_str()) {
        // A remote server is never spawned, with or without a `url`.
        Some("remote") => Some(serde_json::json!({ "type": "remote", "url": cfg.get("url") })),
        // opencode's local type is stdio.
        Some("local") => {
            let mut c = cfg.clone();
            if let Some(o) = c.as_object_mut() {
                o.remove("type");
            }
            Some(c)
        }
        _ => Some(cfg.clone()),
    }
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
        path: path.clone(),
        mtime: file_mtime(&path),
        token_count,
        content_bytes: bytes.len(),
        tool_count: None,
    });
}

use super::file_mtime;

/// Minimal JSONC comment stripper. Handles `// line` and `/* block */` — good
/// enough for typical opencode configs; NOT a full spec-compliant JSONC parser
/// (does not respect string boundaries in edge cases like `"a // b"`).
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2 == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = ' ';
                    for c2 in chars.by_ref() {
                        if prev == '*' && c2 == '/' {
                            break;
                        }
                        prev = c2;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Remove trailing commas before `}` / `]` (JSONC allows them) outside of
/// strings. Run AFTER comments are stripped.
fn strip_jsonc_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    #[test]
    fn line_comment_removed_newline_kept() {
        assert_eq!(
            strip_jsonc_comments("{\"a\":1} // note\n{}"),
            "{\"a\":1} \n{}"
        );
    }

    #[test]
    fn block_comment_removed() {
        assert_eq!(strip_jsonc_comments("a/* x */b"), "ab");
        // A lone '*' or '/' inside the comment must not end it early.
        assert_eq!(strip_jsonc_comments("a/* x/y * z */b"), "ab");
        assert_eq!(strip_jsonc_comments("a/** doc **/b"), "ab");
    }

    #[test]
    fn lone_slash_preserved() {
        assert_eq!(strip_jsonc_comments("a/b"), "a/b");
    }

    #[test]
    fn comment_markers_inside_strings_preserved() {
        let s = r#"{"u":"http://x/*y*/"}"#;
        assert_eq!(strip_jsonc_comments(s), s);
    }

    #[test]
    fn escaped_quote_does_not_end_string() {
        let s = r#"{"a":"q\" // still string"}"#;
        assert_eq!(strip_jsonc_comments(s), s);
    }

    #[test]
    fn string_end_re_enables_comment_stripping() {
        assert_eq!(strip_jsonc_comments(r#""a" // c"#), r#""a" "#);
        assert_eq!(strip_jsonc_comments(r#""a"/* c */"b""#), r#""a""b""#);
    }
}
