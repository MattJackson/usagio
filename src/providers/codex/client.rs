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
        for thread in threads {
            let thread_id = thread
                .as_str()
                .context("invalid loaded-thread identifier")?;
            let details = call(
                socket,
                id,
                "thread/read",
                json!({"threadId":thread_id,"includeTurns":false}),
            )?;
            id += 1;
            match details
                .pointer("/thread/status/type")
                .and_then(Value::as_str)
            {
                Some("idle" | "notLoaded" | "systemError") => {}
                Some("active") => busy = true,
                _ => bail!("Codex returned an unknown thread activity state"),
            }
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

pub(crate) fn probe() -> Result<Option<ClientState>> {
    let home = super::auth_json_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .context("cannot resolve Codex home")?;
    let path = home.join("app-server-control/app-server-control.sock");
    if !path.exists() {
        return Ok(None);
    }
    let mut state = with_control("ws://localhost/", inspect)?;
    state.instance_token = ["daemon.pid", "app-server.pid"]
        .into_iter()
        .find_map(|name| {
            let bytes = std::fs::read(home.join("app-server-daemon").join(name)).ok()?;
            let record: Value = serde_json::from_slice(&bytes).ok()?;
            let pid = record
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|id| u32::try_from(id).ok())?;
            let start = record.get("processStartTime").and_then(Value::as_str)?;
            Some(json!({"pid":pid,"start":start}).to_string())
        });
    Ok(Some(state))
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
    let mut child = Command::new(binary(&home))
        .args(["app-server", "proxy", "--sock"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    let _ = child.kill();
    let _ = child.wait();
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
        let mut process = Command::new(binary(&home))
            .args(["app-server", "daemon", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting Codex daemon")?;
        let status = loop {
            if let Some(status) = process.try_wait()? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = process.kill();
                let _ = process.wait();
                bail!("Codex daemon start timed out; handoff remains pending");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
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
    let actual = probe()?.context("Codex daemon did not return after restart")?;
    if !actual
        .email
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(email))
        || expected
            .uuid
            .as_ref()
            .is_some_and(|id| actual.account_id.as_ref() != Some(id))
    {
        bail!("Codex daemon did not load the selected account");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use crate::providers::Provider;
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
                .complete_login(&crate::tests::codex_login_response(
                    "a@example.com",
                    "account-a",
                ))
                .unwrap();
            provider
                .write_active_account(&first.secret_blob, &first.identity)
                .unwrap();
            crate::capture_current_generic("codex").unwrap();
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
            crate::switch_to_provider_account("codex", "a@example.com", true).unwrap();
            assert!(!crate::provider_switch_pending("codex", "a@example.com"));
            let unchanged = probe().unwrap().unwrap();
            assert!(unchanged.busy);
            assert_eq!(unchanged.instance_token, before.instance_token);
            let second = provider
                .complete_login(&crate::tests::codex_login_response(
                    "b@example.com",
                    "account-b",
                ))
                .unwrap();
            crate::persist_login(&provider, second, None).unwrap();
            crate::switch_to_provider_account("codex", "b@example.com", true).unwrap();
            assert!(!crate::provider_switch_pending("codex", "b@example.com"));
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
        let result = probe().unwrap();
        assert!(result.is_some());
        eprintln!("Codex daemon metadata: {result:?}");
    }
}
