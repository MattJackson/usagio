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
fn mcp_notifications_before_responses_are_skipped() {
    let note = r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#;
    let req = r#"{"jsonrpc":"2.0","id":9,"method":"roots/list"}"#;
    let script = format!(
        "read a; echo '{note}'; echo '{INIT_OK}'; read b; read c; echo '{note}'; echo '{req}'; \
         echo 'plain log line'; echo '{TOOLS_OK}'; sleep 5"
    );
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 2);
}

#[test]
fn mcp_tools_list_follows_next_cursor() {
    let p1 = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"t1"}],"nextCursor":"c2"}}"#;
    let p2 = r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"t2"},{"name":"t3"}]}}"#;
    let script = format!(
        "read a; echo '{INIT_OK}'; read b; read c; echo '{p1}'; read d; echo '{p2}'; sleep 5"
    );
    let s = mcp::fetch_tools(&sh_server(&script)).unwrap();
    assert_eq!(s.tool_count, 3);
}

#[test]
fn mcp_command_expands_env_var_defaults() {
    let script = format!("read a; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5");
    let cfg = serde_json::json!({
        "command": "${USAGIO_TEST_UNSET_SH:-/bin/sh}",
        "args": ["-c", script]
    });
    let s = mcp::fetch_tools(&cfg).unwrap();
    assert_eq!(s.tool_count, 2);
}

#[test]
fn mcp_initialize_error_surfaced() {
    let script =
        r#"read a; echo '{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no"}}'; sleep 5"#;
    let e = mcp::fetch_tools(&sh_server(script)).unwrap_err();
    assert!(e.starts_with("initialize failed"), "got: {}", e);
}

#[test]
fn mcp_tools_list_error_surfaced() {
    let script = format!(
        r#"read a; echo '{INIT_OK}'; read b; read c; echo '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32601,"message":"Method not found"}}}}'; sleep 5"#
    );
    let e = mcp::fetch_tools(&sh_server(&script)).unwrap_err();
    assert!(
        e.starts_with("tools/list failed") && e.contains("Method not found"),
        "got: {e}"
    );
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

// ---- codeaudit r1 C4 regressions ---------------------------------------

#[test]
fn codex_legacy_instructions_md_is_not_counted() {
    with_home(|home| {
        let dir = home.join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("instructions.md"), "old style").unwrap();
        let l = build_ledger("codex", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        // Current (Rust) Codex never reads the legacy TS CLI's instructions.md.
        assert!(names.is_empty(), "{names:?}");
    });
}

#[test]
fn codex_dir_honours_codex_home() {
    use std::ffi::OsString;
    use std::path::PathBuf;
    let home = Some(PathBuf::from("/h"));
    assert_eq!(
        codex::resolve_codex_dir(Some(OsString::from("/c")), home.clone()),
        Some(PathBuf::from("/c"))
    );
    assert_eq!(
        codex::resolve_codex_dir(Some(OsString::new()), home.clone()),
        Some(PathBuf::from("/h/.codex"))
    );
    assert_eq!(
        codex::resolve_codex_dir(None, home),
        Some(PathBuf::from("/h/.codex"))
    );
}

#[test]
fn claude_skill_counts_only_frontmatter() {
    with_home(|home| {
        let dir = home.join(".claude").join("skills").join("s");
        fs::create_dir_all(&dir).unwrap();
        let body = "x ".repeat(5000);
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: s\ndescription: does a thing\nmodel: opus\n---\n{body}"),
        )
        .unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(
            l.items[0].content_bytes,
            "name: s\ndescription: does a thing\n".len()
        );
    });
}

#[test]
fn claude_mcp_servers_read_from_dot_claude_json_and_project_mcp_json() {
    with_home(|home| {
        let proj = tempdir().unwrap();
        let server = |tools: &str| {
            let script =
                format!("read a; echo '{INIT_OK}'; read b; read c; echo '{tools}'; sleep 5");
            serde_json::json!({ "command": "/bin/sh", "args": ["-c", script] })
        };
        fs::write(
            home.join(".claude.json"),
            serde_json::to_string(
                &serde_json::json!({ "mcpServers": { "user-srv": server(TOOLS_OK) } }),
            )
            .unwrap(),
        )
        .unwrap();
        fs::write(
            proj.path().join(".mcp.json"),
            serde_json::to_string(
                &serde_json::json!({ "mcpServers": { "proj-srv": server(TOOLS_OK) } }),
            )
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"enableAllProjectMcpServers": true}"#,
        )
        .unwrap();
        let canon = fs::canonicalize(proj.path()).unwrap();
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": { "user-srv": server(TOOLS_OK) },
                "projects": { canon.to_string_lossy(): { "hasTrustDialogAccepted": true } } })
            .to_string(),
        )
        .unwrap();
        let l = build_ledger("claude", Some(proj.path())).unwrap();
        let mut names: Vec<_> = l
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::McpTools)
            .map(|i| i.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["mcp: proj-srv", "mcp: user-srv"]);
    });
}

#[test]
fn opencode_local_mcp_entry_is_translated_and_disabled_skipped() {
    with_home(|home| {
        let dir = home.join(".config").join("opencode");
        fs::create_dir_all(&dir).unwrap();
        let script =
            format!("read a; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5");
        let cfg = serde_json::json!({
            "mcp": {
                "local-srv": { "type": "local", "command": ["/bin/sh", "-c", script], "environment": {"A": "b"} },
                "off-srv": { "type": "local", "command": ["/bin/sh", "-c", "exit 1"], "enabled": false }
            }
        });
        fs::write(dir.join("opencode.json"), cfg.to_string()).unwrap();
        let l = build_ledger("opencode", None).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["mcp: local-srv"]);
        assert_eq!(l.items[0].tool_count, Some(2));
    });
}

#[test]
fn opencode_jsonc_trailing_commas_and_instructions_and_global_agents_md() {
    with_home(|home| {
        let dir = home.join(".config").join("opencode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "global rules").unwrap();
        let proj = tempdir().unwrap();
        fs::write(proj.path().join("extra.md"), "extra instructions").unwrap();
        fs::write(
            dir.join("opencode.jsonc"),
            r#"{
  // trailing commas are legal JSONC
  "prompt": "hi, there",
  "instructions": ["extra.md",],
}"#,
        )
        .unwrap();
        let l = build_ledger("opencode", Some(proj.path())).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"opencode prompt"), "{names:?}");
        assert!(names.contains(&"Global AGENTS.md"), "{names:?}");
        assert!(names.contains(&"instructions: extra.md"), "{names:?}");
    });
}

#[test]
fn out_of_range_mtime_does_not_panic() {
    let far = std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::MAX / 2);
    assert!(utc_from_system_time(far).is_none());
    assert!(utc_from_system_time(std::time::UNIX_EPOCH).is_some());
}

#[test]
fn read_capped_rejects_non_regular_files() {
    let td = tempdir().unwrap();
    assert!(read_capped(td.path()).is_none(), "directory");
    assert!(
        read_capped(std::path::Path::new("/dev/zero")).is_none(),
        "device"
    );
    let f = td.path().join("f");
    fs::write(&f, "ok").unwrap();
    assert_eq!(read_capped(&f).unwrap(), b"ok");
}

#[test]
fn mcp_servers_are_probed_concurrently() {
    let slow =
        format!("read a; sleep 1; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5");
    let servers: Vec<_> = (0..4)
        .map(|i| (format!("s{i}"), sh_server(&slow)))
        .collect();
    let t = std::time::Instant::now();
    let out = probe_servers(servers);
    assert!(out.iter().all(|(_, r)| r.is_ok()), "{out:?}");
    assert!(
        t.elapsed() < std::time::Duration::from_millis(3000),
        "took {:?}",
        t.elapsed()
    );
    let names: Vec<_> = out.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["s0", "s1", "s2", "s3"]);
}

#[test]
fn cli_run_fails_when_no_provider_could_be_built() {
    with_home(|_| {
        assert!(cli::run(Some("nope".into()), None).is_err());
        assert!(cli::run(Some("codex".into()), None).is_ok());
    });
}

fn mcp_marker_server(marker: &std::path::Path) -> serde_json::Value {
    let script = format!(
        "echo ran > '{}'; read a; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5",
        marker.display()
    );
    serde_json::json!({ "command": "/bin/sh", "args": ["-c", script] })
}

#[test]
fn unapproved_project_mcp_json_server_is_never_executed() {
    with_home(|_home| {
        let proj = tempdir().unwrap();
        let marker = proj.path().join("ran");
        fs::write(
            proj.path().join(".mcp.json"),
            serde_json::json!({ "mcpServers": { "evil": mcp_marker_server(&marker) } }).to_string(),
        )
        .unwrap();
        let l = build_ledger("claude", Some(proj.path())).unwrap();
        assert!(!marker.exists(), "unapproved server command was executed");
        assert!(l.items.iter().all(|i| i.kind != ItemKind::McpTools));
    });
}

#[test]
fn approved_project_mcp_json_server_runs_unless_disabled() {
    with_home(|home| {
        let proj = tempdir().unwrap();
        let canon = fs::canonicalize(proj.path()).unwrap();
        let (m1, m2) = (canon.join("ran1"), canon.join("ran2"));
        fs::write(
            canon.join(".mcp.json"),
            serde_json::json!({ "mcpServers": {
                "ok": mcp_marker_server(&m1), "off": mcp_marker_server(&m2) } })
            .to_string(),
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({ "projects": { canon.to_string_lossy(): {
                "hasTrustDialogAccepted": true,
                "enabledMcpjsonServers": ["ok", "off"],
                "disabledMcpjsonServers": ["off"] } } })
            .to_string(),
        )
        .unwrap();
        // relative/odd spelling of the same project must still resolve
        let spelled = canon.join(".");
        let l = build_ledger("claude", Some(&spelled)).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["mcp: ok"]);
        assert!(m1.exists() && !m2.exists());
    });
}

#[test]
fn claude_skill_when_to_use_is_counted() {
    with_home(|home| {
        let dir = home.join(".claude").join("skills").join("s");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: s\nwhen_to_use: always\n---\nbody",
        )
        .unwrap();
        let l = build_ledger("claude", None).unwrap();
        assert_eq!(
            l.items[0].content_bytes,
            "name: s\nwhen_to_use: always\n".len()
        );
    });
}

#[test]
fn opencode_env_substitution_expands() {
    crate::env_lock::scoped_env_var("USAGIO_TEST_OC_VAR", Some("val"), || {
        assert_eq!(
            opencode::expand_opencode_env("a-{env:USAGIO_TEST_OC_VAR}-b"),
            "a-val-b"
        );
        assert_eq!(
            opencode::expand_opencode_env("{env:USAGIO_NOPE_UNSET}x"),
            "x"
        );
        assert_eq!(
            opencode::expand_opencode_env("plain {env:oops"),
            "plain {env:oops"
        );
    });
}

// ---- MCP driver: failure modes -----------------------------------------

#[test]
fn mcp_silent_server_hits_wall_clock_timeout() {
    // Reads the handshake and never answers: a blocking read would hang
    // forever; the 3s RPC budget must turn it into an Err.
    let started = std::time::Instant::now();
    let e = mcp::fetch_tools(&sh_server("read a; sleep 20")).unwrap_err();
    assert!(e.contains("timeout"), "got: {e}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "timeout must be wall-clock bounded, took {:?}",
        started.elapsed()
    );
}

#[test]
fn mcp_malformed_or_unexpected_replies_are_errors() {
    let bad_json = format!("read a; echo '{INIT_OK}'; read b; read c; echo 'not json'; sleep 5");
    // A non-JSON line is skipped while waiting for the reply with our id
    // (servers may interleave notifications), so it ends as a timeout.
    let e = mcp::fetch_tools(&sh_server(&bad_json)).unwrap_err();
    assert!(e.contains("timeout"), "got: {e}");

    let rpc_err = format!(
        r#"read a; echo '{INIT_OK}'; read b; read c; echo '{{"jsonrpc":"2.0","id":2,"error":{{"code":-1}}}}'; sleep 5"#
    );
    let e = mcp::fetch_tools(&sh_server(&rpc_err)).unwrap_err();
    assert!(e.starts_with("tools/list failed"), "got: {e}");

    let not_array = format!(
        r#"read a; echo '{INIT_OK}'; read b; read c; echo '{{"jsonrpc":"2.0","id":2,"result":{{"tools":{{}}}}}}'; sleep 5"#
    );
    let e = mcp::fetch_tools(&sh_server(&not_array)).unwrap_err();
    assert!(e.contains("missing result.tools"), "got: {e}");

    let eof = "read a; exit 0";
    let e = mcp::fetch_tools(&sh_server(eof)).unwrap_err();
    assert!(e.contains("eof"), "got: {e}");
}

#[test]
fn mcp_http_transport_and_bad_config_are_skipped_with_errors() {
    let e = mcp::fetch_tools(&serde_json::json!({"url": "http://localhost:1/mcp"})).unwrap_err();
    assert!(e.contains("http transport"), "got: {e}");
    let e = mcp::fetch_tools(&serde_json::json!({"args": []})).unwrap_err();
    assert!(e.contains("bad stdio config"), "got: {e}");
}

// ---- collect(): MCP / skills / plugins actually discovered ---------------

fn working_mcp_server() -> serde_json::Value {
    sh_server(&format!(
        "read a; echo '{INIT_OK}'; read b; read c; echo '{TOOLS_OK}'; sleep 5"
    ))
}

#[test]
fn claude_collect_reports_working_mcp_server_skills_and_plugins() {
    with_home(|home| {
        let dir = home.join(".claude");
        fs::create_dir_all(dir.join("skills/my-skill")).unwrap();
        fs::write(dir.join("skills/my-skill/SKILL.md"), "skill body").unwrap();
        fs::create_dir_all(dir.join("plugins/my-plugin")).unwrap();
        fs::write(dir.join("plugins/my-plugin/plugin.json"), r#"{"name":"p"}"#).unwrap();
        let settings = serde_json::json!({ "mcpServers": { "good": working_mcp_server() } });
        fs::write(
            dir.join("settings.json"),
            serde_json::to_string(&settings).unwrap(),
        )
        .unwrap();
        let l = build_ledger("claude", None).unwrap();
        let mcp_item = l
            .items
            .iter()
            .find(|i| i.kind == ItemKind::McpTools)
            .unwrap_or_else(|| panic!("no McpTools item in {:?}", l.items));
        assert_eq!(mcp_item.name, "mcp: good");
        assert_eq!(mcp_item.tool_count, Some(2));
        assert!(mcp_item.token_count > 0);
        assert!(l
            .items
            .iter()
            .any(|i| i.kind == ItemKind::Skill && i.name == "skill: my-skill"));
        assert!(l
            .items
            .iter()
            .any(|i| i.kind == ItemKind::PluginManifest && i.name == "plugin: my-plugin"));
    });
}

#[test]
fn opencode_collect_reads_mcp_from_opencode_json_fallback() {
    with_home(|home| {
        let dir = home.join(".config/opencode");
        fs::create_dir_all(&dir).unwrap();
        // Only opencode.json (no .jsonc) exists: the fallback name is read.
        let cfg = serde_json::json!({ "mcp": { "good": working_mcp_server() } });
        fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&cfg).unwrap(),
        )
        .unwrap();
        let l = build_ledger("opencode", None).unwrap();
        let item = l
            .items
            .iter()
            .find(|i| i.kind == ItemKind::McpTools)
            .unwrap_or_else(|| panic!("no McpTools item in {:?}", l.items));
        assert_eq!(item.name, "mcp: good");
        assert_eq!(item.tool_count, Some(2));
    });
}

/// A repo with its own `.claude/settings.json` approving its `.mcp.json`
/// server, plus an optional `~/.claude.json` projects entry.
fn self_approving_repo(home: &std::path::Path, trust: Option<bool>) -> std::path::PathBuf {
    let proj = tempdir().unwrap().keep();
    let canon = fs::canonicalize(&proj).unwrap();
    let marker = canon.join("ran");
    fs::write(
        canon.join(".mcp.json"),
        serde_json::json!({ "mcpServers": { "evil": mcp_marker_server(&marker) } }).to_string(),
    )
    .unwrap();
    fs::create_dir_all(canon.join(".claude")).unwrap();
    fs::write(
        canon.join(".claude").join("settings.json"),
        r#"{"enableAllProjectMcpServers": true}"#,
    )
    .unwrap();
    if let Some(t) = trust {
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({ "projects": { canon.to_string_lossy(): {
                "hasTrustDialogAccepted": t } } })
            .to_string(),
        )
        .unwrap();
    }
    canon
}

#[test]
fn repo_settings_cannot_self_approve_without_trust() {
    for trust in [None, Some(false)] {
        with_home(|home| {
            let canon = self_approving_repo(home, trust);
            let l = build_ledger("claude", Some(&canon)).unwrap();
            assert!(
                !canon.join("ran").exists(),
                "repo-approved server was executed"
            );
            assert!(l.items.iter().all(|i| i.kind != ItemKind::McpTools));
        });
    }
}

#[test]
fn repo_settings_approve_when_folder_trusted() {
    with_home(|home| {
        let canon = self_approving_repo(home, Some(true));
        let l = build_ledger("claude", Some(&canon)).unwrap();
        assert!(canon.join("ran").exists());
        assert!(l.items.iter().any(|i| i.name == "mcp: evil"));
    });
}

fn user_approved_repo(home: &std::path::Path, trust: bool, settings: &str) -> std::path::PathBuf {
    let canon = self_approving_repo(home, trust.then_some(true));
    fs::remove_file(canon.join(".claude").join("settings.json")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude").join("settings.json"), settings).unwrap();
    canon
}

#[test]
fn user_level_approval_does_not_start_servers_in_untrusted_folder() {
    for settings in [
        r#"{"enableAllProjectMcpServers": true}"#,
        r#"{"enabledMcpjsonServers": ["evil"]}"#,
    ] {
        with_home(|home| {
            let canon = user_approved_repo(home, false, settings);
            let l = build_ledger("claude", Some(&canon)).unwrap();
            assert!(!canon.join("ran").exists(), "untrusted folder server ran");
            assert!(l.items.iter().all(|i| i.kind != ItemKind::McpTools));
        });
    }
}

#[test]
fn user_level_approval_starts_servers_in_trusted_folder() {
    with_home(|home| {
        let canon = user_approved_repo(home, true, r#"{"enableAllProjectMcpServers": true}"#);
        let l = build_ledger("claude", Some(&canon)).unwrap();
        assert!(canon.join("ran").exists());
        assert!(l.items.iter().any(|i| i.name == "mcp: evil"));
    });
}

#[test]
fn repo_settings_can_still_disable_servers_when_untrusted() {
    with_home(|home| {
        let canon = self_approving_repo(home, None);
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({ "projects": { canon.to_string_lossy(): {
                "enabledMcpjsonServers": ["evil"] } } })
            .to_string(),
        )
        .unwrap();
        fs::write(
            canon.join(".claude").join("settings.local.json"),
            r#"{"disabledMcpjsonServers": ["evil"]}"#,
        )
        .unwrap();
        build_ledger("claude", Some(&canon)).unwrap();
        assert!(!canon.join("ran").exists());
    });
}

#[test]
fn claude_project_instructions_found_in_all_documented_locations() {
    with_home(|_home| {
        let root = tempdir().unwrap();
        let proj = root.path().join("sub");
        fs::create_dir_all(proj.join(".claude")).unwrap();
        fs::write(proj.join(".claude").join("CLAUDE.md"), "dot claude").unwrap();
        fs::write(proj.join("CLAUDE.local.md"), "local").unwrap();
        fs::write(root.path().join("CLAUDE.md"), "ancestor").unwrap();
        let l = build_ledger("claude", Some(&proj)).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Project .claude/CLAUDE.md"), "{names:?}");
        assert!(names.contains(&"Project CLAUDE.local.md"), "{names:?}");
        assert!(
            names.iter().any(|n| n.starts_with("CLAUDE.md (")),
            "{names:?}"
        );
    });
}

#[test]
fn codex_override_walk_cap_and_disabled_mcp() {
    with_home(|home| {
        let dir = home.join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "x".repeat(30_000)).unwrap();
        fs::write(dir.join("AGENTS.override.md"), "short").unwrap();
        let marker = home.join("ran");
        let script = format!("echo ran > '{}'; exit 1", marker.display());
        fs::write(
            dir.join("config.toml"),
            format!(
                "[mcp_servers.off]\ncommand = \"/bin/sh\"\nargs = [\"-c\", {script:?}]\nenabled = false\n"
            ),
        )
        .unwrap();
        // repo root (has .git) with a big AGENTS.md; project is a subdir.
        let repo = tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.md"), "r".repeat(40_000)).unwrap();
        let sub = repo.path().join("pkg");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("AGENTS.md"), "sub").unwrap();
        let l = build_ledger("codex", Some(&sub)).unwrap();
        let by = |n: &str| l.items.iter().find(|i| i.name.starts_with(n));
        assert_eq!(by("Global AGENTS.override.md").unwrap().content_bytes, 5);
        assert!(by("Global AGENTS.md").is_none());
        let root_doc = l
            .items
            .iter()
            .find(|i| i.name.starts_with("AGENTS.md ("))
            .expect("git-root AGENTS.md");
        assert_eq!(root_doc.content_bytes, 32 * 1024);
        assert!(by("Project AGENTS.md").is_none(), "cap reached");
        assert!(!marker.exists(), "enabled=false server was started");
    });
}

#[test]
fn mcpjson_rejection_does_not_hide_same_named_user_server() {
    with_home(|home| {
        let canon = self_approving_repo(home, None);
        // the repo's `evil` is rejected, but the user's own `evil` server runs
        let marker = home.join("user-ran");
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": { "evil": mcp_marker_server(&marker) },
                "projects": { canon.to_string_lossy(): {
                    "disabledMcpjsonServers": ["evil"] } } })
            .to_string(),
        )
        .unwrap();
        let l = build_ledger("claude", Some(&canon)).unwrap();
        assert!(!canon.join("ran").exists());
        assert!(
            l.items.iter().any(|i| i.name == "mcp: evil"),
            "user server dropped"
        );
    });
}

/// Set several env vars (HOME, XDG_CONFIG_HOME, ...) for `f` under the
/// crate-wide ENV_LOCK (`scoped_env_var` is not reentrant), restoring after.
fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let _l = crate::env_lock::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    struct Restore(Vec<(String, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (k, v) in self.0.drain(..).rev() {
                #[allow(clippy::disallowed_methods)]
                match v {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
    let mut restore = Restore(Vec::new());
    let mut marks = Vec::new();
    for (k, v) in vars {
        restore.0.push((k.to_string(), std::env::var_os(k)));
        #[allow(clippy::disallowed_methods)]
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
        marks.push(crate::env_lock::mark_scoped(k));
    }
    f();
    drop(marks);
    drop(restore);
}

#[test]
fn claude_project_skills_and_agents_counted() {
    with_home(|home| {
        // Personal skill wins over a same-named project skill.
        let user_skill = home.join(".claude/skills/deploy");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: mine\n---\nbody",
        )
        .unwrap();
        let repo = tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        for (skill, desc) in [("deploy", "theirs, longer"), ("lint", "lints")] {
            let d = repo.path().join(".claude/skills").join(skill);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {skill}\ndescription: {desc}\n---\nbody"),
            )
            .unwrap();
        }
        let proj = repo.path().join("pkg");
        fs::create_dir_all(proj.join(".claude/agents")).unwrap();
        fs::write(
            proj.join(".claude/agents/reviewer.md"),
            format!(
                "---\nname: reviewer\ndescription: reviews\n---\n{}",
                "x".repeat(4000)
            ),
        )
        .unwrap();
        let l = build_ledger("claude", Some(&proj)).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names.iter().filter(|n| **n == "skill: deploy").count(),
            1,
            "{names:?}"
        );
        assert!(names.contains(&"skill: lint"), "{names:?}");
        let deploy = l.items.iter().find(|i| i.name == "skill: deploy").unwrap();
        assert_eq!(
            deploy.content_bytes,
            "name: deploy\ndescription: mine\n".len()
        );
        let agent = l
            .items
            .iter()
            .find(|i| i.name == "agent: reviewer")
            .unwrap_or_else(|| panic!("project agent missing: {names:?}"));
        assert_eq!(agent.kind, ItemKind::SubagentDef);
        assert!(agent.content_bytes < 100, "only frontmatter counted");
    });
}

#[test]
fn codex_blank_override_suppresses_that_directorys_agents_md() {
    with_home(|_| {
        let proj = tempdir().unwrap();
        fs::create_dir_all(proj.path().join(".git")).unwrap();
        fs::write(proj.path().join("AGENTS.override.md"), "  \n").unwrap();
        fs::write(proj.path().join("AGENTS.md"), "shared rules").unwrap();
        let l = build_ledger("codex", Some(proj.path())).unwrap();
        assert!(l.items.is_empty(), "{:?}", l.items);
    });
}

#[test]
fn codex_project_override_is_labelled_by_the_file_counted() {
    with_home(|_| {
        let repo = tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.override.md"), "root override").unwrap();
        fs::write(repo.path().join("AGENTS.md"), "root shared").unwrap();
        let proj = repo.path().join("pkg");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("AGENTS.override.md"), "short").unwrap();
        fs::write(proj.join("AGENTS.md"), "x".repeat(2000)).unwrap();
        let l = build_ledger("codex", Some(&proj)).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Project AGENTS.override.md"), "{names:?}");
        assert!(
            names.iter().any(|n| n.starts_with("AGENTS.override.md (")),
            "{names:?}"
        );
    });
}

#[test]
fn opencode_config_dir_honours_xdg_config_home() {
    let home = tempdir().unwrap();
    let xdg = tempdir().unwrap();
    let cfg = xdg.path().join("opencode");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("AGENTS.md"), "xdg rules").unwrap();
    with_env(
        &[
            ("HOME", home.path().to_str()),
            ("XDG_CONFIG_HOME", xdg.path().to_str()),
        ],
        || {
            let l = build_ledger("opencode", None).unwrap();
            let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(names, vec!["Global AGENTS.md"]);
        },
    );
}

#[test]
fn opencode_agent_counts_only_frontmatter() {
    with_home(|home| {
        let dir = home.join(".config/opencode/agent");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("reviewer.md"),
            format!(
                "---\ndescription: reviews code\nmode: subagent\n---\n{}",
                "prompt ".repeat(1000)
            ),
        )
        .unwrap();
        let l = build_ledger("opencode", None).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(
            l.items[0].content_bytes,
            "description: reviews code\n".len()
        );
    });
}

#[test]
fn opencode_project_rules_found_walking_up_to_the_repo_root() {
    with_home(|_| {
        let repo = tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join("AGENTS.md"), "root rules").unwrap();
        let proj = repo.path().join("packages/api");
        fs::create_dir_all(&proj).unwrap();
        let l = build_ledger("opencode", Some(&proj)).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.starts_with("AGENTS.md (")),
            "{names:?}"
        );

        // No AGENTS.md anywhere: CLAUDE.md is the fallback.
        let repo2 = tempdir().unwrap();
        fs::create_dir_all(repo2.path().join(".git")).unwrap();
        fs::write(repo2.path().join("CLAUDE.md"), "claude rules").unwrap();
        let l = build_ledger("opencode", Some(repo2.path())).unwrap();
        let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Project CLAUDE.md"]);
    });
}

#[test]
fn opencode_home_resolves_like_the_rest_of_the_crate() {
    // HOME empty counts as unset, so `paths::user_home()` falls back to
    // USERPROFILE; `~/` instruction paths must resolve under it.
    let profile = tempdir().unwrap();
    fs::write(profile.path().join("rules.md"), "profile rules").unwrap();
    let xdg = tempdir().unwrap();
    let cfg = xdg.path().join("opencode");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(
        cfg.join("opencode.json"),
        r#"{"instructions":["~/rules.md"]}"#,
    )
    .unwrap();
    with_env(
        &[
            ("HOME", Some("")),
            ("USERPROFILE", profile.path().to_str()),
            ("XDG_CONFIG_HOME", xdg.path().to_str()),
        ],
        || {
            let l = build_ledger("opencode", None).unwrap();
            let names: Vec<_> = l.items.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(names, vec!["instructions: ~/rules.md"]);
        },
    );
}
