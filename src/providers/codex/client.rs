//! Codex daemon protocol adapter. Only metadata is requested: no thread history
//! or bearer tokens. The lifecycle engine decides when a handoff is appropriate.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ClientState {
    pub busy: bool,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub instance_token: Option<String>,
}

struct Proxy {
    input: ChildStdin,
    output: ChildStdout,
}
impl Read for Proxy {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.output.read(bytes)
    }
}
impl Write for Proxy {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.input.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.input.flush()
    }
}

fn binary(home: &Path) -> PathBuf {
    let managed = home
        .join("packages/app-server-daemon/current/bin")
        .join(format!("codex{}", std::env::consts::EXE_SUFFIX));
    if managed.is_file() {
        managed
    } else {
        PathBuf::from("codex")
    }
}

fn call<S: Read + Write>(
    socket: &mut WebSocket<S>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    socket.send(Message::Text(
        json!({"id": id, "method": method, "params": params})
            .to_string()
            .into(),
    ))?;
    loop {
        let message = socket.read()?;
        if let Message::Text(body) = message {
            let response: Value = serde_json::from_str(&body)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if response.get("error").is_some() {
                // Never include response bodies: protocol errors can echo input.
                bail!("Codex rejected {method}");
            }
            return response
                .get("result")
                .cloned()
                .context("Codex returned no result");
        }
    }
}

fn inspect<S: Read + Write>(socket: &mut WebSocket<S>) -> Result<ClientState> {
    call(
        socket,
        1,
        "initialize",
        json!({
            "clientInfo": {"name": "usagio", "title": "usagio", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true},
        }),
    )?;
    socket.send(Message::Text(
        json!({"method":"initialized"}).to_string().into(),
    ))?;
    let accounts = call(
        socket,
        2,
        "accountSession/list",
        json!({"refreshWorkspaceMetadata":false}),
    )?;
    let session = accounts
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|session| session.get("isActive").and_then(Value::as_bool) == Some(true))
        });
    let email = session
        .and_then(|session| session.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let account_id = session
        .and_then(|session| session.get("selectedWorkspaceAccountId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut cursor = Value::Null;
    let mut id = 3;
    let mut busy = false;
    loop {
        let page = call(
            socket,
            id,
            "thread/loaded/list",
            json!({"cursor":cursor,"limit":100}),
        )?;
        id += 1;
        let threads = page
            .get("data")
            .and_then(Value::as_array)
            .context("Codex returned no loaded-thread list")?;
        let ids = threads
            .iter()
            .map(|thread| thread.as_str().context("invalid loaded-thread identifier"))
            .collect::<Result<Vec<_>>>()?;
        if any_thread_active(socket, &mut id, &ids)? {
            // One active thread settles it: no need to read the rest.
            busy = true;
            break;
        }
        cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            break;
        }
    }
    Ok(ClientState {
        busy,
        email,
        account_id,
        instance_token: None,
    })
}

/// Read the status of every thread in `ids` with one pipelined batch (all
/// requests first, then the replies in any order) instead of one round trip
/// each. Returns as soon as one reply reports an active thread.
fn any_thread_active<S: Read + Write>(
    socket: &mut WebSocket<S>,
    next_id: &mut u64,
    ids: &[&str],
) -> Result<bool> {
    let mut pending = std::collections::HashSet::new();
    for thread_id in ids {
        let id = *next_id;
        *next_id += 1;
        socket.send(Message::Text(
            json!({"id": id, "method": "thread/read",
                   "params": {"threadId": thread_id, "includeTurns": false}})
            .to_string()
            .into(),
        ))?;
        pending.insert(id);
    }
    while !pending.is_empty() {
        let Message::Text(body) = socket.read()? else {
            continue;
        };
        let response: Value = serde_json::from_str(&body)?;
        let Some(id) = response.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if !pending.remove(&id) {
            continue;
        }
        if response.get("error").is_some() {
            // Never include response bodies: protocol errors can echo input.
            bail!("Codex rejected thread/read");
        }
        match response
            .pointer("/result/thread/status/type")
            .and_then(Value::as_str)
        {
            Some("idle" | "notLoaded" | "systemError") => {}
            Some("active") => return Ok(true),
            _ => bail!("Codex returned an unknown thread activity state"),
        }
    }
    Ok(false)
}

pub(crate) fn probe() -> Result<Option<ClientState>> {
    let home = super::auth_json_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .context("cannot resolve Codex home")?;
    let path = home.join("app-server-control/app-server-control.sock");
    if !path.exists() {
        return Ok(None);
    }
    // Pin the token before AND after inspect(): if the daemon restarted in
    // between, busy/email describe one instance and the token another.
    let before = read_instance_token(&home);
    let mut state = with_control("ws://localhost/", inspect)?;
    let after = read_instance_token(&home);
    state.instance_token = stable_instance_token(before, after)?;
    Ok(Some(state))
}

fn read_instance_token(home: &Path) -> Option<String> {
    ["daemon.pid", "app-server.pid"]
        .into_iter()
        .find_map(|name| {
            let path = home.join("app-server-daemon").join(name);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                Err(e) => {
                    // Not "absent": say why the handoff token is missing.
                    crate::logging::log(&format!(
                        "codex: cannot read daemon pid record {}: {e}",
                        path.display()
                    ));
                    return None;
                }
            };
            let record: Value = match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    crate::logging::log(&format!(
                        "codex: daemon pid record {} is unreadable JSON: {e}",
                        path.display()
                    ));
                    return None;
                }
            };
            let pid = record
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|id| u32::try_from(id).ok())?;
            let start = record.get("processStartTime").and_then(Value::as_str)?;
            Some(json!({"pid":pid,"start":start}).to_string())
        })
}

const UNSTABLE_DAEMON_MSG: &str = "Codex daemon changed while being inspected; retry next cycle";

fn is_unstable_daemon(e: &anyhow::Error) -> bool {
    e.to_string() == UNSTABLE_DAEMON_MSG
}

fn stable_instance_token(before: Option<String>, after: Option<String>) -> Result<Option<String>> {
    if before != after {
        bail!("{UNSTABLE_DAEMON_MSG}");
    }
    Ok(after)
}

fn with_control<T: Send + 'static>(
    url: &'static str,
    action: impl FnOnce(&mut WebSocket<Proxy>) -> Result<T> + Send + 'static,
) -> Result<T> {
    with_control_timeout(url, Duration::from_secs(5), action)
}

fn with_control_timeout<T: Send + 'static>(
    url: &'static str,
    timeout: Duration,
    action: impl FnOnce(&mut WebSocket<Proxy>) -> Result<T> + Send + 'static,
) -> Result<T> {
    let home = super::auth_json_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .context("cannot resolve Codex home")?;
    let path = home.join("app-server-control/app-server-control.sock");
    let mut command = Command::new(binary(&home));
    command
        .args(["app-server", "proxy", "--sock"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A bare `codex` may be a launcher shim whose real proxy is a
    // grandchild: spawn a group so a timeout reaches it too.
    let mut child = crate::platform::own_process_group(&mut command)
        .spawn()
        .context("starting Codex control connection")?;
    let proxy = Proxy {
        input: child.stdin.take().context("missing Codex input")?,
        output: child.stdout.take().context("missing Codex output")?,
    };
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let (mut socket, _) = tungstenite::client(url, proxy)
                .map_err(|_| anyhow!("Codex control connection failed"))?;
            action(&mut socket)
        })();
        let _ = send.send(result);
    });
    let result = receive
        .recv_timeout(timeout)
        .map_err(|_| anyhow!("Codex control request timed out"));
    crate::platform::kill_tree(&mut child);
    result?
}

pub(crate) fn verify_stopped(token: &str) -> Result<()> {
    let instance: Value = serde_json::from_str(token).context("invalid Codex daemon instance")?;
    let pid = instance["pid"]
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .context("invalid daemon PID")?;
    let start = instance["start"]
        .as_str()
        .context("missing daemon generation")?;
    if crate::platform::daemon_instance_alive(pid, start)? {
        bail!("Codex daemon is still releasing its sessions; handoff will retry");
    }
    Ok(())
}

/// Stop the recorded instance. Unix uses a bounded restart; Windows retains
/// its once-only graceful shutdown RPC.
pub(crate) fn begin_drain(token: &str) -> Result<()> {
    let instance: Value = serde_json::from_str(token).context("invalid Codex daemon instance")?;
    let pid = instance["pid"]
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .context("invalid daemon PID")?;
    let start = instance["start"]
        .as_str()
        .context("missing daemon generation")?;
    if crate::platform::graceful_daemon_signal(pid, start)? {
        return Ok(());
    }
    with_control("ws://localhost/daemon/shutdown", move |socket| {
        socket.send(Message::Text(pid.to_string().into()))?;
        match socket.read()? {
            Message::Text(reply) if reply == pid.to_string() => {}
            _ => bail!("Codex did not acknowledge graceful drain"),
        }
        let _ = socket.close(None);
        Ok(())
    })
}

/// Start through Codex's supported daemon command and verify its cached
/// account. No tokens are passed on the command line or returned in output.
pub(crate) fn start_and_verify(expected: &crate::providers::IdentitySnapshot) -> Result<()> {
    let email = expected
        .email
        .as_deref()
        .context("cannot verify Codex handoff without account identity")?;
    let home = super::auth_json_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .context("cannot resolve Codex home")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .context("Codex daemon start timed out; handoff remains pending")?;
        let status = crate::platform::current()
            .run_independent(
                &binary(&home),
                &["app-server", "daemon", "start"],
                &[("CODEX_HOME", home.as_os_str())],
                remaining,
            )
            .context("starting independent Codex daemon; handoff remains pending")?;
        if status.success() {
            break;
        }
        // The control socket disappears before the old process finishes its
        // final cleanup. Codex start can reject that still-owned PID briefly.
        if std::time::Instant::now() >= deadline {
            bail!("Codex daemon start failed");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // The pid file can land mid-probe; retry the token-mismatch error within
    // the handoff deadline instead of failing a verification that never uses
    // the token.
    let actual = loop {
        match probe() {
            Ok(state) => break state.context("Codex daemon did not return after restart")?,
            Err(e) if is_unstable_daemon(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    };
    if !identity_matches(&actual, email, expected.uuid.as_ref()) {
        bail!("Codex daemon did not load the selected account");
    }
    Ok(())
}

/// Whether the daemon's reported identity is the account we asked it to load:
/// email must match (case-insensitively) and, when a uuid is expected, the
/// account id must match too.
fn identity_matches(actual: &ClientState, email: &str, uuid: Option<&String>) -> bool {
    actual
        .email
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(email))
        && uuid.is_none_or(|id| actual.account_id.as_ref() == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::trait_def::ActiveSlot;
    use crate::providers::trait_def::LoginFlow;

    /// Run `inspect` against a scripted daemon over a loopback socket pair.
    /// `status` maps a thread id to its `thread/read` status type; loaded
    /// threads come in two pages, `t1` then `t2`.
    fn inspect_scripted(status: fn(&str) -> &'static str) -> Result<ClientState> {
        inspect_scripted_logged(status).0
    }

    /// As `inspect_scripted`, also returning every request method received.
    fn inspect_scripted_logged(
        status: fn(&str) -> &'static str,
    ) -> (Result<ClientState>, Vec<String>) {
        use tungstenite::protocol::Role;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = WebSocket::from_raw_socket(tcp, Role::Server, None);
            let mut methods = Vec::new();
            while let Ok(msg) = ws.read() {
                let Message::Text(body) = msg else { continue };
                let req: Value = serde_json::from_str(&body).unwrap();
                let Some(id) = req.get("id").cloned() else {
                    continue; // the `initialized` notification
                };
                methods.push(req["method"].as_str().unwrap().to_string());
                let result = match req["method"].as_str().unwrap() {
                    "accountSession/list" => json!({"sessions": [
                        {"isActive": false, "email": "other@example.com"},
                        {"isActive": true, "email": "a@example.com",
                         "selectedWorkspaceAccountId": "acct-1"},
                    ]}),
                    "thread/loaded/list" if req["params"]["cursor"].is_null() => {
                        json!({"data": ["t1", "t3"], "nextCursor": "page-2"})
                    }
                    "thread/loaded/list" => json!({"data": ["t2"], "nextCursor": null}),
                    "thread/read" => {
                        let t = req["params"]["threadId"].as_str().unwrap();
                        json!({"thread": {"status": {"type": status(t)}}})
                    }
                    _ => json!({}),
                };
                let reply = json!({"id": id, "result": result}).to_string();
                if ws.send(Message::Text(reply.into())).is_err() {
                    break;
                }
            }
            methods
        });
        let tcp = std::net::TcpStream::connect(addr).unwrap();
        let mut ws = WebSocket::from_raw_socket(tcp, Role::Client, None);
        let state = inspect(&mut ws);
        drop(ws);
        let methods = server.join().unwrap();
        (state, methods)
    }

    #[test]
    fn inspect_stops_at_the_first_active_thread() {
        let (state, methods) =
            inspect_scripted_logged(|t| if t == "t1" { "active" } else { "idle" });
        assert!(state.unwrap().busy);
        let lists = methods
            .iter()
            .filter(|m| *m == "thread/loaded/list")
            .count();
        assert_eq!(
            lists, 1,
            "page 2 was fetched after a busy answer: {methods:?}"
        );
        // All threads idle: every loaded thread is read, page by page.
        let (state, methods) = inspect_scripted_logged(|_| "idle");
        assert!(!state.unwrap().busy);
        let reads = methods.iter().filter(|m| *m == "thread/read").count();
        assert_eq!(reads, 3, "{methods:?}");
    }

    #[test]
    fn inspect_reports_busy_for_an_active_thread_on_a_later_page() {
        let state = inspect_scripted(|t| if t == "t2" { "active" } else { "idle" }).unwrap();
        assert!(state.busy, "the active thread on page 2 was missed");
        assert_eq!(state.email.as_deref(), Some("a@example.com"));
        assert_eq!(state.account_id.as_deref(), Some("acct-1"));
        let idle = inspect_scripted(|t| if t == "t1" { "notLoaded" } else { "idle" }).unwrap();
        assert!(!idle.busy);
    }

    #[test]
    fn inspect_refuses_an_unknown_thread_state() {
        assert!(inspect_scripted(|_| "somethingNew").is_err());
    }

    #[test]
    fn identity_matches_checks_email_and_uuid() {
        let st = |email: Option<&str>, id: Option<&str>| ClientState {
            busy: false,
            email: email.map(String::from),
            account_id: id.map(String::from),
            instance_token: None,
        };
        let u = "u-1".to_string();
        assert!(identity_matches(
            &st(Some("A@x.com"), Some("u-1")),
            "a@x.com",
            Some(&u)
        ));
        assert!(identity_matches(
            &st(Some("a@x.com"), None),
            "a@x.com",
            None
        ));
        assert!(!identity_matches(
            &st(Some("b@x.com"), Some("u-1")),
            "a@x.com",
            Some(&u)
        ));
        assert!(!identity_matches(
            &st(None, Some("u-1")),
            "a@x.com",
            Some(&u)
        ));
        assert!(!identity_matches(
            &st(Some("a@x.com"), Some("u-2")),
            "a@x.com",
            Some(&u)
        ));
        assert!(!identity_matches(
            &st(Some("a@x.com"), None),
            "a@x.com",
            Some(&u)
        ));
    }

    #[test]
    fn instance_token_must_match_before_and_after_inspect() {
        let a = Some("a".to_string());
        assert_eq!(stable_instance_token(a.clone(), a.clone()).unwrap(), a);
        assert!(stable_instance_token(a, Some("b".into())).is_err());
        assert_eq!(stable_instance_token(None, None).unwrap(), None);
        let e = stable_instance_token(None, Some("b".into())).unwrap_err();
        assert!(is_unstable_daemon(&e));
    }

    /// Hold a synthetic Responses stream open until the test releases it.
    /// No credentials or prompts are recorded or sent beyond loopback.
    fn held_response() -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(_) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        Err(_) => return,
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 {
                    return;
                }
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                if length > 4 * 1024 * 1024 {
                    return;
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                drop(reader);
                if !request.starts_with("POST /v1/responses ") {
                    let body = "{\"models\":[]}";
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.flush();
                    continue;
                }
                let opened =
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"mock-response\"}}\n\n";
                let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"mock-response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
                if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{opened}", opened.len() + completed.len()).is_err() { return; }
                let _ = stream.flush();
                let _ = started.send(());
                if release_rx.recv_timeout(Duration::from_secs(25)).is_ok() {
                    let _ = stream.write_all(completed.as_bytes());
                    let _ = stream.flush();
                }
                break;
            }
        });
        (format!("http://{address}/v1"), started_rx, release)
    }

    #[test]
    #[ignore = "requires installed Codex; starts only an isolated test daemon"]
    fn isolated_daemon_restarts_and_loads_the_selected_account() {
        let _cfg = crate::store::ScopedConfigDir::new();
        let dir = tempfile::Builder::new()
            .prefix("usagio-codex-")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        crate::env_lock::scoped_env_var("CODEX_HOME", Some(dir.path().to_str().unwrap()), || {
            let home = dir.path();
            let (base_url, started, release) = held_response();
            std::fs::write(
                home.join("config.toml"),
                format!(
                    r#"
model = "gpt-5.4"
model_provider = "mock"
approval_policy = "never"
sandbox_mode = "read-only"
[model_providers.mock]
name = "Local handoff test"
base_url = "{base_url}"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
"#
                ),
            )
            .unwrap();
            std::fs::create_dir_all(home.join("app-server-daemon")).unwrap();
            std::fs::write(
                home.join("app-server-daemon/settings.json"),
                r#"{"updater":{"autoUpdateEnabled":false},"shutdownGraceSeconds":5}"#,
            )
            .unwrap();
            struct Cleanup;
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = Command::new("codex")
                        .args(["app-server", "daemon", "stop"])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
            let _cleanup = Cleanup;
            let provider = super::super::CodexProvider;
            let first = provider
                .complete_login(&crate::test_support::codex_login_response(
                    "a@example.com",
                    "account-a",
                ))
                .unwrap();
            provider
                .write_active_account(&first.secret_blob, &first.identity)
                .unwrap();
            crate::cli::capture::capture_current_generic("codex").unwrap();
            start_and_verify(&first.identity).unwrap();
            let session = with_control("ws://localhost/", |socket| {
                inspect(socket)?;
                let thread = call(
                    socket,
                    1000,
                    "thread/start",
                    json!({"experimentalRawEvents":false}),
                )?;
                thread
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .context("missing isolated thread ID")
            })
            .unwrap();
            let turn_session = session.clone();
            with_control("ws://localhost/", move |socket| {
                inspect(socket)?;
                call(socket, 1000, "turn/start", json!({"threadId":turn_session,"input":[{"type":"text","text":"Reply briefly.","text_elements":[]}]}))?;
                Ok(())
            }).unwrap();
            started
                .recv_timeout(Duration::from_secs(10))
                .expect("Codex did not reach the local response server");
            let before = probe().unwrap().unwrap();
            assert!(before.busy);
            crate::switching::provider::switch_to_provider_account("codex", "a@example.com", true)
                .unwrap();
            assert!(!crate::switching::provider::provider_switch_pending(
                "codex",
                "a@example.com"
            ));
            let unchanged = probe().unwrap().unwrap();
            assert!(unchanged.busy);
            assert_eq!(unchanged.instance_token, before.instance_token);
            let second = provider
                .complete_login(&crate::test_support::codex_login_response(
                    "b@example.com",
                    "account-b",
                ))
                .unwrap();
            crate::cli::capture::persist_login(&provider, second, None).unwrap();
            crate::switching::provider::switch_to_provider_account("codex", "b@example.com", true)
                .unwrap();
            assert!(!crate::switching::provider::provider_switch_pending(
                "codex",
                "b@example.com"
            ));
            verify_stopped(before.instance_token.as_deref().unwrap()).unwrap();
            let _ = release.send(());
            assert_eq!(
                crate::store::State::load()
                    .unwrap()
                    .provider_accounts("codex")
                    .unwrap()
                    .active
                    .as_deref(),
                Some("b@example.com")
            );
            let after = probe().unwrap().unwrap();
            assert_eq!(after.email.as_deref(), Some("b@example.com"));
            assert_ne!(after.instance_token, before.instance_token);
            with_control("ws://localhost/", move |socket| {
                inspect(socket)?;
                let resumed = call(socket, 1000, "thread/resume", json!({"threadId":session}))?;
                anyhow::ensure!(
                    resumed.pointer("/thread/id").and_then(Value::as_str) == Some(session.as_str()),
                    "session was not resumed after handoff"
                );
                let history = call(
                    socket,
                    1001,
                    "thread/read",
                    json!({"threadId":session,"includeTurns":true}),
                )?;
                let turns = history
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .context("missing resumed turn history")?;
                anyhow::ensure!(
                    turns
                        .last()
                        .and_then(|turn| turn.get("status"))
                        .and_then(Value::as_str)
                        == Some("interrupted"),
                    "forced restart must preserve the interrupted turn in history"
                );
                Ok(())
            })
            .unwrap();
        });
    }

    #[test]
    #[ignore = "explicit read-only inspection of the installed Codex daemon"]
    fn inspect_installed_daemon() {
        // Read-only on purpose: resolve the real Codex home (test builds only
        // see scoped variables), and keep usagio's own log in a temp dir.
        let _cfg = crate::store::ScopedConfigDir::new();
        let (key, real) = match std::env::var("CODEX_HOME") {
            Ok(h) if !h.is_empty() => ("CODEX_HOME", h),
            _ => ("HOME", std::env::var("HOME").expect("HOME is set")),
        };
        let result = crate::env_lock::scoped_env_var(key, Some(&real), || probe().unwrap());
        assert!(result.is_some());
        eprintln!("Codex daemon metadata: {result:?}");
    }
}
