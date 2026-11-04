//! Discovery for the opencode CLI (SST).
//!
//! Opencode context sources:
//! - ~/.config/opencode/opencode.jsonc (or opencode.json) — top-level config
//!   with optional `prompt` / `rules` strings, an `instructions` list of
//!   file paths, and an `mcp` server map
//! - ~/.config/opencode/AGENTS.md — global rules
//! - ~/.config/opencode/agent/*.md — subagent definitions
//! - <project>/AGENTS.md — sometimes read via project convention
//!
//! `.jsonc` may contain comments and trailing commas — strip both before
//! parsing.

use super::{tokenize, ItemKind, LedgerError, LedgerItem};
use std::fs;
use std::path::{Path, PathBuf};

pub fn collect(project: Option<&Path>) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut items = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return Ok(items);
    };
    let cfg_dir = home.join(".config").join("opencode");

    // opencode.jsonc / opencode.json
    let config_candidates = ["opencode.jsonc", "opencode.json"];
    let mut chosen_config = None;
    for name in config_candidates {
        let p = cfg_dir.join(name);
        if p.exists() {
            chosen_config = Some(p);
            break;
        }
    }

    // Global rules file (opencode reads ~/.config/opencode/AGENTS.md).
    push_file_if_exists(
        &mut items,
        cfg_dir.join("AGENTS.md"),
        ItemKind::GlobalInstructions,
        "Global AGENTS.md",
    );

    if let Some(config_path) = chosen_config {
        match fs::read_to_string(&config_path) {
            Err(e) => eprintln!("could not read {}: {}", config_path.display(), e),
            Ok(raw) => {
                let stripped = strip_jsonc_trailing_commas(&strip_jsonc_comments(&raw));
                match serde_json::from_str::<serde_json::Value>(&stripped) {
                    Err(e) => eprintln!("could not parse {}: {}", config_path.display(), e),
                    Ok(val) => collect_from_config(&mut items, &val, &config_path, project, &home),
                }
            }
        }
    }

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
                push_file_if_exists(
                    &mut items,
                    path,
                    ItemKind::SubagentDef,
                    &format!("agent: {}", name),
                );
            }
        }
    }

    // Project AGENTS.md (opencode also reads this by convention on some paths)
    if let Some(proj) = project {
        push_file_if_exists(
            &mut items,
            proj.join("AGENTS.md"),
            ItemKind::ProjectInstructions,
            "Project AGENTS.md",
        );
    }

    Ok(items)
}

fn collect_from_config(
    items: &mut Vec<LedgerItem>,
    val: &serde_json::Value,
    config_path: &Path,
    project: Option<&Path>,
    home: &Path,
) {
    let config_path = config_path.to_path_buf();
    // `prompt` string
    if let Some(prompt) = val.get("prompt").and_then(|v| v.as_str()) {
        let tokens = tokenize::count_tokens(prompt, tokenize::TokenizerHint::Anthropic);
        items.push(LedgerItem {
            kind: ItemKind::Other,
            name: "opencode prompt".into(),
            path: config_path.clone(),
            mtime: file_mtime(&config_path),
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
            let tokens = tokenize::count_tokens(&joined, tokenize::TokenizerHint::Anthropic);
            items.push(LedgerItem {
                kind: ItemKind::GlobalInstructions,
                name: "opencode rules".into(),
                path: config_path.clone(),
                mtime: file_mtime(&config_path),
                token_count: tokens,
                content_bytes: joined.len(),
                tool_count: None,
            });
        }
    }

    // `instructions` — list of instruction FILE paths opencode reads into
    // context. Plain paths only; globs and URLs are not expanded here.
    if let Some(list) = val.get("instructions").and_then(|v| v.as_array()) {
        for entry in list.iter().filter_map(|v| v.as_str()) {
            if entry.contains(['*', '?', '[']) || entry.starts_with("http") {
                eprintln!("opencode instructions entry '{}' skipped (glob/URL)", entry);
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
    }

    // `mcp` — server map. opencode's shape differs from Claude's; translate.
    if let Some(servers) = val.get("mcp").and_then(|v| v.as_object()) {
        let servers: Vec<(String, serde_json::Value)> = servers
            .iter()
            .filter_map(|(name, cfg)| translate_mcp_entry(cfg).map(|c| (name.clone(), c)))
            .collect();
        for (name, result) in super::probe_servers(servers) {
            match result {
                Ok(summary) => items.push(LedgerItem {
                    kind: ItemKind::McpTools,
                    name: format!("mcp: {}", name),
                    path: config_path.clone(),
                    mtime: file_mtime(&config_path),
                    token_count: summary.token_count,
                    content_bytes: summary.byte_count,
                    tool_count: Some(summary.tool_count),
                }),
                Err(err) => eprintln!("opencode mcp {} skipped: {}", name, err),
            }
        }
    }
}

/// Convert an opencode `mcp` entry to the Claude-style `{command, args, env}`
/// (or `{url}`) shape `mcp::fetch_tools` understands. opencode local servers
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
        return Some(serde_json::json!({
            "command": cmd,
            "args": args,
            "env": cfg.get("environment").cloned().unwrap_or_else(|| serde_json::json!({})),
        }));
    }
    if cfg.get("type").and_then(|t| t.as_str()) == Some("remote") && cfg.get("url").is_some() {
        return Some(serde_json::json!({ "url": cfg["url"] }));
    }
    Some(cfg.clone())
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
