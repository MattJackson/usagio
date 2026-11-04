//! MCP (Model Context Protocol) server tool-schema fetch.
//!
//! Spawn each server as configured, drive stdio JSON-RPC:
//!   1. `initialize` (required by spec before any other call)
//!   2. `notifications/initialized`
//!   3. `tools/list`
//!
//! Serialize the tools response to JSON and count its tokens — that's
//! approximately what enters the model's context per turn.
//!
//! Timeout: a hard wall-clock ~3s per server, enforced regardless of whether
//! the child ever writes a byte (robustness-02, v0.5.2 audit — see
//! `read_line_with_timeout`'s doc for why a plain polling loop around a
//! blocking `read_until` isn't enough). Missing binaries / crashes surface as
//! errors and the caller skips that row.

use super::tokenize;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// RAII guard that kills + reaps a spawned MCP child on drop. Ensures every
/// `?` early return between spawn and cleanup — spawn failure, write failure,
/// timeout, malformed response — reaps the child rather than leaking it as a
/// zombie. See H5 in the round-1 codeaudit findings.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(c: Child) -> Self {
        Self(Some(c))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            // Kill the child's whole process GROUP, not just the direct child.
            // MCP servers are commonly `npx`/`node` (or `uvx`/`python`) wrappers
            // that spawn their own children; `Child::kill` signals only the
            // direct child, leaving those grandchildren orphaned and running
            // (H5 reaped the direct child but not its tree). The child is
            // spawned with `process_group(0)` on unix, so its PGID == its PID;
            // sending SIGKILL to `-PID` reaps the entire subtree. The direct
            // `kill()`/`wait()` still runs to reap the child itself (and is the
            // only cleanup on non-unix, where process groups aren't set up).
            #[cfg(unix)]
            {
                let pgid = c.id() as i32;
                // SAFETY: kill(2) with a negative pid targets that process
                // group. `pgid` is this child's own pid — which is its pgid
                // because it was spawned with process_group(0) — so this can
                // only ever signal processes usagio itself spawned.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

const RPC_TIMEOUT: Duration = Duration::from_secs(3);

/// R3-RES-01: per-line byte cap for stdio JSON-RPC reads. A misbehaving MCP
/// server that streams bytes without a newline would otherwise grow the read
/// buffer unboundedly (BufRead::read_line has no size limit and the 3s
/// timeout is only checked between successive read_line calls). 1 MiB sits
/// well above realistic tools/list responses.
const MAX_LINE_BYTES: u64 = 1 << 20;

#[derive(Debug)]
pub(crate) struct McpSummary {
    pub(crate) tool_count: usize,
    pub(crate) token_count: usize,
    pub(crate) byte_count: usize,
}

#[derive(Debug, Deserialize)]
struct StdioConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
}

/// A fully resolved stdio server launch: exactly what gets spawned.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Launch {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Start from an empty environment (Codex) instead of inheriting ours.
    pub(crate) clear_env: bool,
    pub(crate) cwd: Option<std::path::PathBuf>,
}

/// Probe a Claude-Code-shaped server config (`{type?, command, args, env}`).
/// Test-only, and only the unix test suite (which spawns `sh` servers) uses it.
#[cfg(all(test, unix))]
pub(crate) fn fetch_tools(config: &Value) -> Result<McpSummary, String> {
    fetch_launch(&claude_launch(config)?)
}

/// Resolve a Claude Code MCP config to a launch. Only stdio servers are
/// probed: any other `type` (`sse`, `http`, `ws`, ...) is a remote transport
/// even when its `url` is missing, and must never fall through to spawning a
/// `command` it happens to carry.
pub(crate) fn claude_launch(config: &Value) -> Result<Launch, String> {
    match config.get("type") {
        None => {}
        Some(Value::String(t)) if t == "stdio" => {}
        Some(Value::String(t)) => return Err(format!("{t} transport not yet supported")),
        Some(other) => return Err(format!("bad server type: {other}")),
    }
    // Only stdio-transport servers supported here. HTTP transport (URL-based)
    // is out of scope for the first ledger pass — flag and skip.
    if config.get("url").is_some() {
        return Err("http transport not yet supported".into());
    }
    let cfg: StdioConfig =
        serde_json::from_value(config.clone()).map_err(|e| format!("bad stdio config: {}", e))?;
    // Claude Code expands `${VAR}` / `${VAR:-default}` in command, args and env
    // values of MCP server configs; do the same so we spawn what it would.
    Ok(Launch {
        command: expand_env_vars(&cfg.command),
        args: cfg.args.iter().map(|a| expand_env_vars(a)).collect(),
        env: cfg
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand_env_vars(v)))
            .collect(),
        clear_env: false,
        cwd: None,
    })
}

/// Variables Codex passes from its own environment to every stdio MCP server
/// (`rmcp-client` `DEFAULT_ENV_VARS`); the child otherwise starts empty.
#[cfg(unix)]
const CODEX_DEFAULT_ENV_VARS: &[&str] = &[
    "HOME",
    "LOGNAME",
    "PATH",
    "SHELL",
    "USER",
    "__CF_USER_TEXT_ENCODING",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "TZ",
];
#[cfg(windows)]
const CODEX_DEFAULT_ENV_VARS: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SHELL",
    "COMSPEC",
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "USERNAME",
    "USERDOMAIN",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    "LOCALAPPDATA",
    "APPDATA",
    "TEMP",
    "TMP",
    "TMPDIR",
    "POWERSHELL",
    "PWSH",
];

/// Resolve a Codex `[mcp_servers.<name>]` table (as JSON) to a launch, the
/// way Codex starts it: a `url` server is streamable HTTP; a `command` server
/// runs in an EMPTY environment holding only Codex's default variables, the
/// `env_vars` names (plain strings or `{name, source}`) copied from ours, and
/// then `env`; in `cwd`, else `fallback_cwd` (the session directory). Codex
/// does no `${VAR}` expansion.
pub(crate) fn codex_launch(
    config: &Value,
    fallback_cwd: Option<&std::path::Path>,
) -> Result<Launch, String> {
    if config.get("url").is_some() {
        return Err("http transport not yet supported".into());
    }
    let command = match config.get("command") {
        Some(Value::String(c)) => c.clone(),
        _ => return Err("bad stdio config: missing string `command`".into()),
    };
    let strings = |key: &str| -> Result<Vec<String>, String> {
        match config.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or_else(|| format!("bad stdio config: `{key}` entries must be strings"))
                })
                .collect(),
            Some(_) => Err(format!("bad stdio config: `{key}` must be an array")),
        }
    };
    let args = strings("args")?;
    let mut names: Vec<String> = CODEX_DEFAULT_ENV_VARS
        .iter()
        .map(|s| s.to_string())
        .collect();
    match config.get("env_vars") {
        None => {}
        Some(Value::Array(a)) => {
            for v in a {
                let (name, source) = match v {
                    Value::String(n) => (n.clone(), None),
                    Value::Object(o) => match o.get("name").and_then(|n| n.as_str()) {
                        Some(n) => (n.to_string(), o.get("source").and_then(|s| s.as_str())),
                        None => {
                            return Err("bad stdio config: env_vars entry without `name`".into())
                        }
                    },
                    _ => return Err("bad stdio config: bad env_vars entry".into()),
                };
                match source {
                    None | Some("local") => names.push(name),
                    Some("remote") => {
                        return Err(format!(
                        "env_vars entry `{name}` uses source `remote` (remote stdio, not probed)"
                    ))
                    }
                    Some(other) => {
                        return Err(format!("unsupported env_vars source `{other}`"));
                    }
                }
            }
        }
        Some(_) => return Err("bad stdio config: `env_vars` must be an array".into()),
    }
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: String, v: String| {
        env.retain(|(ek, _)| ek != &k);
        env.push((k, v));
    };
    for name in names {
        if let Ok(v) = std::env::var(&name) {
            set(name, v);
        }
    }
    match config.get("env") {
        None => {}
        Some(Value::Object(o)) => {
            for (k, v) in o {
                let v = v
                    .as_str()
                    .ok_or_else(|| format!("bad stdio config: env `{k}` must be a string"))?;
                set(k.clone(), v.to_string());
            }
        }
        Some(_) => return Err("bad stdio config: `env` must be a table".into()),
    }
    let cwd = match config.get("cwd") {
        None => fallback_cwd.map(|p| p.to_path_buf()),
        Some(Value::String(c)) => Some(std::path::PathBuf::from(c)),
        Some(_) => return Err("bad stdio config: `cwd` must be a string".into()),
    };
    Ok(Launch {
        command,
        args,
        env,
        clear_env: true,
        cwd,
    })
}

/// Spawn `launch` and count the tool schemas it lists.
pub(crate) fn fetch_launch(launch: &Launch) -> Result<McpSummary, String> {
    let start = Instant::now();
    let mut cmd = Command::new(&launch.command);
    if launch.clear_env {
        cmd.env_clear();
    }
    if let Some(dir) = &launch.cwd {
        cmd.current_dir(dir);
    }
    cmd.args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Put the server in its own process group so ChildGuard's Drop can SIGKILL
    // the whole tree (npx → node → real server), not just the direct child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {}", e))?;

    // Pull the stdio handles out BEFORE handing the child to ChildGuard so the
    // guard doesn't need to be re-borrowed for each I/O op (avoids a two-mut
    // borrow through `guard.as_mut()`). ChildGuard now owns kill+reap on any
    // early return between here and the happy-path drop at fn end.
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = BufReader::new(stdout);
    let _guard = ChildGuard::new(child);

    // robustness-02 (v0.5.2 audit): hand the blocking reader off to a
    // dedicated thread so the 3s budget below is a real wall-clock timeout,
    // not just a gap check between blocking `read_until` calls. A server
    // that writes nothing and never exits would otherwise block
    // `read_until` inside the kernel indefinitely — the old polling loop's
    // `start.elapsed() > RPC_TIMEOUT` check can only run BETWEEN calls, so it
    // never fires while a call is in flight. The reader thread itself
    // terminates naturally once `_guard`'s `Drop` kills the child: killing
    // it closes the write end of the stdout pipe, so the blocked
    // `read_until` unblocks with EOF instead of leaking the thread.
    let rx = spawn_line_reader(reader);

    // Initialize request
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "usagio-ledger", "version": "0.1"}
        }
    });
    writeln!(&mut stdin, "{}", init).map_err(|e| format!("write init: {}", e))?;

    // Wait for initialize response; if the server rejected our handshake
    // (JSON-RPC error object at top level), surface it now instead of
    // proceeding into a `tools/list` that would fail the same way.
    let init_response = read_response(&rx, start, 1)?;
    if let Some(err) = init_response.get("error") {
        return Err(format!("initialize failed: {}", err));
    }

    // Send initialized notification (no id — no response expected)
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(&mut stdin, "{}", initialized).map_err(|e| format!("write initialized: {}", e))?;

    // tools/list, following `result.nextCursor` pagination (MCP spec) under
    // the same wall-clock deadline. Capped so a server that never stops
    // returning a cursor can't loop forever.
    let mut tools: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    for page in 0..MAX_TOOL_PAGES {
        let id = 2 + page as i64;
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let tools_req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": params
        });
        writeln!(&mut stdin, "{}", tools_req).map_err(|e| format!("write tools/list: {}", e))?;
        let parsed = read_response(&rx, start, id)?;
        if let Some(err) = parsed.get("error") {
            return Err(format!("tools/list failed: {}", err));
        }
        let page_tools = parsed
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .ok_or("tools/list missing result.tools")?;
        tools.extend(page_tools.iter().cloned());
        cursor = parsed
            .get("result")
            .and_then(|r| r.get("nextCursor"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(String::from);
        if cursor.is_none() {
            break;
        }
    }

    // Happy path: ChildGuard::Drop at end-of-scope will kill+reap the child.
    let serialized = serde_json::to_string(&tools).unwrap_or_default();
    let token_count = tokenize::count_tokens(&serialized, tokenize::TokenizerHint::Anthropic);
    Ok(McpSummary {
        tool_count: tools.len(),
        token_count,
        byte_count: serialized.len(),
    })
}

/// Upper bound on `tools/list` pages followed per server.
const MAX_TOOL_PAGES: usize = 50;

/// Read lines until the JSON-RPC response whose `id` equals `id` arrives,
/// skipping anything else a server may interleave on stdout: notifications
/// and server->client requests (they carry `method`), responses to other ids,
/// and non-JSON log lines. All within the shared `RPC_TIMEOUT` deadline.
fn read_response(rx: &mpsc::Receiver<LineMsg>, start: Instant, id: i64) -> Result<Value, String> {
    loop {
        let line = read_line_with_timeout(rx, start)?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("method").is_some() {
            continue;
        }
        if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
            return Ok(v);
        }
    }
}

/// Expand `${VAR}` and `${VAR:-default}` against the process environment.
/// An unset variable with no default is left as the literal text (the server
/// will fail visibly rather than us guessing a value).
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = match body.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (body, None),
        };
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => match default {
                Some(d) => out.push_str(d),
                None => {
                    out.push_str("${");
                    out.push_str(body);
                    out.push('}');
                }
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// One outcome of the background reader thread's attempt to produce the next
/// non-empty line.
enum LineMsg {
    Line(String),
    Eof,
    Err(String),
}

/// Spawn a thread that owns `reader` for the rest of the child's lifetime,
/// blocking on `read_until(b'\n')` in a loop and forwarding each non-empty
/// line (or the terminal EOF/error) over the returned channel. This is what
/// lets `read_line_with_timeout` enforce a real wall-clock deadline: the
/// blocking syscall lives on this thread, so the caller's `recv_timeout` can
/// give up on it without waiting for the kernel read to return.
fn spawn_line_reader(mut reader: BufReader<ChildStdout>) -> mpsc::Receiver<LineMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        // R3-RES-01: cap per-line reads at MAX_LINE_BYTES so a server that
        // streams bytes without a newline can't grow this buffer unboundedly.
        let mut bytes: Vec<u8> = Vec::new();
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES);
        let msg = match limited.read_until(b'\n', &mut bytes) {
            Ok(0) => {
                let _ = tx.send(LineMsg::Eof);
                return;
            }
            Ok(_) => {
                if bytes.len() as u64 >= MAX_LINE_BYTES && !bytes.ends_with(b"\n") {
                    let _ = tx.send(LineMsg::Err(format!(
                        "line exceeded {} bytes without newline",
                        MAX_LINE_BYTES
                    )));
                    return;
                }
                match String::from_utf8(bytes) {
                    Ok(s) if s.trim().is_empty() => continue,
                    Ok(s) => LineMsg::Line(s),
                    Err(e) => {
                        let _ = tx.send(LineMsg::Err(format!("non-utf8 in response: {}", e)));
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(LineMsg::Err(format!("read: {}", e)));
                return;
            }
        };
        // The receiver may already be gone (fetch_tools returned early on a
        // prior error and dropped `rx`) — stop reading rather than spin
        // forever against a child nobody is listening to anymore. The
        // ChildGuard has already killed the process by the time that
        // happens, so this thread exits promptly either way.
        if tx.send(msg).is_err() {
            return;
        }
    });
    rx
}

/// Wait for the next line the background reader thread (see
/// `spawn_line_reader`) produces, enforcing a hard wall-clock deadline of
/// `RPC_TIMEOUT` measured from `start` — regardless of whether the child
/// ever writes anything. Unlike a polling loop around a blocking read, this
/// deadline is real: `recv_timeout` returns on schedule even while the
/// reader thread's `read_until` is still parked in the kernel waiting on the
/// child's stdout pipe.
fn read_line_with_timeout(rx: &mpsc::Receiver<LineMsg>, start: Instant) -> Result<String, String> {
    let remaining = RPC_TIMEOUT
        .checked_sub(start.elapsed())
        .unwrap_or(Duration::ZERO);
    if remaining.is_zero() {
        return Err(format!("timeout after {:?}", RPC_TIMEOUT));
    }
    match rx.recv_timeout(remaining) {
        Ok(LineMsg::Line(s)) => Ok(s),
        Ok(LineMsg::Eof) => Err("eof before response".into()),
        Ok(LineMsg::Err(e)) => Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!("timeout after {:?}", RPC_TIMEOUT)),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("reader thread ended without a response".into())
        }
    }
}
