//! Unit tests for the Context Ledger.
//!
//! Uses `tempfile` to sandbox HOME-relative paths. Some tests set HOME to a
//! temp dir to isolate ~/.claude discovery.
//!
//! Gated to Unix: the ledger discovery paths (`~/.claude/CLAUDE.md`,
//! `~/.codex/prompts/*.md`, `~/.config/opencode/*`) are Unix conventions.
//! Windows has its own analog paths under `%APPDATA%` that the vendor CLIs
//! use, but wiring the ledger discovery to those is a v0.6 follow-up.
//! Uses `cfg(unix)` (not `cfg(target_os)`) so the strict-cfg guard leaves
//! it alone — the whole file becomes empty on Windows compiles.
#![cfg(unix)]

use super::*;
use std::fs;
use tempfile::tempdir;

/// Every test that mutates `$HOME` here is serialised against the *rest of
/// the crate* via `crate::env_lock::scoped_env_var`. Local mutexes weren't
/// enough — the never-re-login incident was caused by a different in-crate
/// test module setting `$HOME` outside this local lock, racing us mid-scope.
fn with_home<F: FnOnce(&std::path::Path)>(f: F) {
    let td = tempdir().unwrap();
    let path = td.path().to_path_buf();
    let path_str = path.to_str().expect("tempdir path is valid UTF-8");
    crate::env_lock::scoped_env_var("HOME", Some(path_str), || {
        f(&path);
    });
}

#[test]
fn empty_state_yields_empty_ledger_claude() {
    with_home(|_| {
        let l = build_ledger("claude", None).unwrap();
        assert!(l.items.is_empty(), "expected empty, got {:?}", l.items);
        assert_eq!(l.total_tokens, 0);
        assert_eq!(l.provider, "claude");
    });
}

#[test]
fn empty_state_yields_empty_ledger_codex() {
    with_home(|_| {
        let l = build_ledger("codex", None).unwrap();
        assert!(l.items.is_empty());
    });
}

#[test]
fn empty_state_yields_empty_ledger_opencode() {
    with_home(|_| {
        let l = build_ledger("opencode", None).unwrap();
        assert!(l.items.is_empty());
    });
}

#[test]
fn unknown_provider_errors() {
    with_home(|_| {
        let e = build_ledger("nope", None).unwrap_err();
        assert!(matches!(e, LedgerError::UnknownProvider(_)));
    });
}

#[test]
fn claude_global_md_counted() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        let content = "hello ".repeat(100); // ~600 chars → ~165 tokens by approx
        fs::write(dir.join("CLAUDE.md"), &content).unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].kind, ItemKind::GlobalInstructions);
        assert!(l.items[0].token_count > 50);
        assert!(l.items[0].token_count < 300);
    });
}

#[test]
fn claude_project_md_counted() {
    with_home(|_| {
        let proj = tempdir().unwrap();
        fs::write(proj.path().join("CLAUDE.md"), "project instructions").unwrap();
        let l = build_ledger("claude", Some(proj.path())).unwrap();
        assert!(l
            .items
            .iter()
            .any(|i| i.kind == ItemKind::ProjectInstructions));
    });
}

#[test]
fn stale_when_file_touched_after_capture() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("CLAUDE.md");
        fs::write(&p, "v1").unwrap();
        let l = build_ledger("claude", None).unwrap();
        // Sleep briefly so mtime is strictly greater than captured_at.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&p, "v2").unwrap();
        assert!(is_stale(&l), "ledger should be stale after file mtime bump");
    });
}

#[test]
fn mcp_missing_binary_skipped_not_fatal() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        // settings.json with a non-existent MCP binary — collect() should NOT
        // return an error; the failed server is just omitted from items.
        let settings = serde_json::json!({
            "mcpServers": {
                "does-not-exist": {
                    "command": "/definitely/not/a/binary",
                    "args": []
                }
            }
        });
        fs::write(
            dir.join("settings.json"),
            serde_json::to_string(&settings).unwrap(),
        )
        .unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert!(l.items.iter().all(|i| i.kind != ItemKind::McpTools));
    });
}

#[test]
fn tokenize_approx_within_range() {
    // ~1000 chars → ~275 tokens by approx (chars*11/40)
    let text = "a".repeat(1000);
    let n = tokenize::count_tokens(&text, tokenize::TokenizerHint::Anthropic);
    assert!((250..=300).contains(&n), "got {}", n);
}

#[test]
fn opencode_prompt_counted() {
    with_home(|home| {
        let dir = home.join(".config").join("opencode");
        fs::create_dir_all(&dir).unwrap();
        let cfg = r#"{
  // A comment inside JSONC
  "prompt": "You are a helpful assistant.",
  "rules": ["be terse", "use plain text"]
}"#;
        fs::write(dir.join("opencode.jsonc"), cfg).unwrap();
        let l = build_ledger("opencode", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"opencode prompt"), "names: {:?}", names);
        assert!(names.contains(&"opencode rules"));
    });
}

#[test]
fn total_tokens_sums_items() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("CLAUDE.md"), "hello world").unwrap();
        let l = build_ledger("claude", None).unwrap();
        let sum: usize = l.items.iter().map(|i| i.token_count).sum();
        assert_eq!(l.total_tokens, sum);
    });
}

#[test]
fn claude_agents_only_md_files_counted() {
    with_home(|home| {
        let dir = home.join(".claude").join("agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reviewer.md"), "review things").unwrap();
        fs::write(dir.join("notes.txt"), "not an agent").unwrap();
        let l = build_ledger("claude", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["agent: reviewer"]);
        assert_eq!(l.items[0].kind, ItemKind::SubagentDef);
    });
}

#[test]
fn opencode_agents_only_md_files_counted() {
    with_home(|home| {
        let dir = home.join(".config").join("opencode").join("agent");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("planner.md"), "plan things").unwrap();
        fs::write(dir.join("notes.txt"), "not an agent").unwrap();
        let l = build_ledger("opencode", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["agent: planner"]);
        assert_eq!(l.items[0].kind, ItemKind::SubagentDef);
        assert_eq!(l.items[0].content_bytes, "plan things".len());
        assert!(l.items[0].token_count > 0);
    });
}

#[test]
fn opencode_project_agents_md_counted() {
    with_home(|_| {
        let proj = tempdir().unwrap();
        fs::write(proj.path().join("AGENTS.md"), "project rules").unwrap();
        let l = build_ledger("opencode", Some(proj.path())).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].name, "Project AGENTS.md");
        assert_eq!(l.items[0].kind, ItemKind::ProjectInstructions);
        assert_eq!(l.items[0].content_bytes, "project rules".len());
        assert!(l.items[0].token_count > 0);
    });
}

#[test]
fn codex_global_and_project_files_counted() {
    with_home(|home| {
        let dir = home.join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "global agents").unwrap();
        fs::write(dir.join("instructions.md"), "old style instructions").unwrap();
        let proj = tempdir().unwrap();
        fs::write(proj.path().join("AGENTS.md"), "project agents").unwrap();
        let l = build_ledger("codex", Some(proj.path())).unwrap();
        let got: Vec<_> = l
            .items
            .iter()
            .map(|i| (i.name.as_str(), i.kind, i.content_bytes))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Global AGENTS.md", ItemKind::GlobalInstructions, 13),
                ("Global instructions.md", ItemKind::GlobalInstructions, 22),
                ("Project AGENTS.md", ItemKind::ProjectInstructions, 14),
            ]
        );
        assert!(l.items.iter().all(|i| i.token_count > 0));
        assert_eq!(
            l.total_tokens,
            l.items.iter().map(|i| i.token_count).sum::<usize>()
        );
    });
}

// ---- MCP stdio driver -------------------------------------------------
// Fake servers are `/bin/sh -c` scripts speaking just enough JSON-RPC.

const INIT_OK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
const TOOLS_OK: &str =
    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"t1"},{"name":"t2"}]}}"#;

fn sh_server(script: &str) -> serde_json::Value {
    serde_json::json!({ "command": "/bin/sh", "args": ["-c", script] })
}

fn pid_alive(pid: i32) -> bool {
    // A zombie still answers kill(0); `ps` reports it as Z.
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[test]
fn mcp_happy_path_counts_tools() {
    let script = format!("read a; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5");
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 2);
    assert_eq!(s.byte_count, r#"[{"name":"t1"},{"name":"t2"}]"#.len());
    assert!(s.token_count > 0);
}

#[test]
fn mcp_blank_lines_between_responses_ignored() {
    let script = format!(
        "read a; echo; echo '  '; echo '{INIT_OK}'; read b; read c; echo; echo '{TOOLS_OK}'; sleep 5"
    );
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 2);
}

#[test]
fn mcp_final_line_without_newline_accepted() {
    let script = format!("read a; echo '{INIT_OK}'; read b; read c; printf '%s' '{TOOLS_OK}'");
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 2);
}

#[test]
fn mcp_oversized_line_without_newline_rejected() {
    // > 1 MiB with no newline must be refused rather than buffered.
    let script = "read a; head -c 1200000 /dev/zero | tr '\\0' a; sleep 5";
    let e = mcp::fetch_tools(&sh_server(script)).unwrap_err();
    assert!(e.contains("exceeded"), "got: {}", e);
}

#[test]
fn mcp_initialize_error_surfaced() {
    let script =
        r#"read a; echo '{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no"}}'; sleep 5"#;
    let e = mcp::fetch_tools(&sh_server(script)).unwrap_err();
    assert!(e.starts_with("initialize failed"), "got: {}", e);
}

#[test]
fn mcp_child_and_grandchildren_killed_after_fetch() {
    let td = tempdir().unwrap();
    let pids = td.path().join("pids");
    let script = format!(
        "sleep 30 & echo $$ > '{p}'; echo $! >> '{p}'; read a; echo '{INIT_OK}'; read b; read c; \
         echo '{TOOLS_OK}'; wait",
        p = pids.display()
    );
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 2);
    let text = fs::read_to_string(&pids).unwrap();
    let ids: Vec<i32> = text.lines().map(|l| l.trim().parse().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    // Allow the kernel a moment to deliver SIGKILL / reparent.
    for _ in 0..40 {
        if ids.iter().all(|p| !pid_alive(*p)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let alive: Vec<_> = ids.iter().filter(|p| pid_alive(**p)).collect();
    // Clean up before asserting so a failure doesn't leak a sleeper.
    for p in &ids {
        unsafe {
            libc::kill(*p, libc::SIGKILL);
        }
    }
    assert!(alive.is_empty(), "still running after fetch: {:?}", alive);
}

#[test]
fn estimate_cost_per_provider() {
    let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
    assert!(close(estimate_cost("claude", 1_000_000), 3.0));
    assert!(close(estimate_cost("claude-code", 2_000_000), 6.0));
    assert!(close(estimate_cost("codex", 1_000_000), 5.0));
    assert!(close(estimate_cost("codex", 500_000), 2.5));
    assert!(close(estimate_cost("opencode", 1_000_000), 3.0));
    assert!(close(estimate_cost("opencode", 500_000), 1.5));
    assert_eq!(estimate_cost("mystery", 1_000_000), None);
}

#[test]
fn build_ledger_carries_cost_and_project() {
    with_home(|home| {
        let dir = home.join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "x".repeat(4000)).unwrap();
        let l = build_ledger("codex", None).unwrap();
        assert!(l.total_tokens > 0);
        let want = l.total_tokens as f64 * 5.0 / 1_000_000.0;
        assert!((l.estimated_cost_per_turn.unwrap() - want).abs() < 1e-12);
    });
}

#[test]
fn render_terminal_shows_provider_and_items() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("CLAUDE.md"), "hello world").unwrap();
        let l = build_ledger("claude", None).unwrap();
        let out = render_terminal(&l);
        assert!(out.starts_with("Context Ledger — Claude Code\n"), "{}", out);
        assert!(out.contains("Global CLAUDE.md"), "{}", out);
        assert!(out.contains("Total baseline per turn"), "{}", out);
    });
}

fn ledger_tracking(path: &std::path::Path, captured_at: DateTime<Utc>) -> Ledger {
    let mtime = DateTime::<Utc>::from(fs::metadata(path).unwrap().modified().unwrap());
    Ledger {
        provider: "claude".into(),
        project: None,
        items: vec![LedgerItem {
            kind: ItemKind::GlobalInstructions,
            name: "t".into(),
            path: path.to_path_buf(),
            mtime,
            token_count: 1,
            content_bytes: 1,
            tool_count: None,
        }],
        total_tokens: 1,
        estimated_cost_per_turn: None,
        captured_at,
    }
}

#[test]
fn not_stale_when_nothing_changed() {
    let td = tempdir().unwrap();
    let p = td.path().join("f.md");
    fs::write(&p, "v1").unwrap();
    let mtime = DateTime::<Utc>::from(fs::metadata(&p).unwrap().modified().unwrap());
    // Captured strictly after the write, and captured exactly at the mtime:
    // neither is "modified since capture".
    assert!(!is_stale(&ledger_tracking(
        &p,
        mtime + chrono::Duration::seconds(5)
    )));
    assert!(!is_stale(&ledger_tracking(&p, mtime)));
    // Captured before the write: stale.
    assert!(is_stale(&ledger_tracking(
        &p,
        mtime - chrono::Duration::seconds(5)
    )));
}

#[test]
fn not_stale_when_tracked_file_missing_or_ledger_empty() {
    let td = tempdir().unwrap();
    let p = td.path().join("gone.md");
    fs::write(&p, "v1").unwrap();
    let l = ledger_tracking(&p, Utc::now() - chrono::Duration::hours(1));
    fs::remove_file(&p).unwrap();
    assert!(!is_stale(&l));
    let mut empty = l.clone();
    empty.items.clear();
    assert!(!is_stale(&empty));
}
