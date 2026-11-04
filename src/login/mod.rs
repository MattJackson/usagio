//! "Renew login…": one engine for every provider's browser login.
//!
//! Every vendor CLI logs in the same way — OAuth 2.0 authorization code with
//! PKCE (RFC 7636) and a loopback redirect. A provider describes its endpoints
//! in a [`LoginSpec`] and turns the token response into its own credential
//! blob ([`LoginFlow::complete_login`]); this module does the rest:
//!
//! 1. mint a PKCE verifier/challenge and a `state`,
//! 2. listen on `localhost` for the redirect,
//! 3. show the authorize page — in usagio's own sign-in window
//!    (`Platform::open_sign_in_window`),
//!    which keeps a separate, persistent cookie store per account, or in the
//!    default browser as a fallback,
//! 4. exchange the code, check the account that signed in is the one being
//!    renewed, and save it (`crate::cli::capture::persist_login`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::providers::trait_def::{LoginSpec, Provider, TokenRequestStyle};

#[cfg(test)]
mod tests;

/// How long the user has to finish signing in before the attempt is dropped.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// An account whose login dies within this window is offered "Renew login…".
/// Matches Claude Code's own "Your login expires in N days" warning (3 days).
pub(crate) const RENEW_WINDOW_SECS: i64 = 3 * 24 * 60 * 60;

/// Whether to warn now that a login expiring at `expires_at` is about to die:
/// inside the renew window, not yet dead, and not already warned for this
/// exact expiry. Pure.
pub(crate) fn should_warn_expiry(
    expires_at: Option<i64>,
    now_ms: i64,
    warned: Option<i64>,
) -> bool {
    let Some(at) = expires_at else {
        return false;
    };
    at > now_ms && at - now_ms <= RENEW_WINDOW_SECS * 1000 && warned != Some(at)
}

// ---------------------------------------------------------------------------
// PKCE + authorize URL
// ---------------------------------------------------------------------------

struct Pkce {
    verifier: String,
    challenge: String,
}

/// RFC 7636 S256: a 256-bit random verifier and its SHA-256 challenge.
fn pkce() -> Pkce {
    let verifier = random_token(32);
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// `BASE64URL(SHA256(verifier))`, RFC 7636 §4.2.
fn challenge_for(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    b64url(&buf)
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn authorize_url(
    spec: &LoginSpec,
    challenge: &str,
    state: &str,
    redirect_uri: &str,
    login_hint: Option<&str>,
) -> Result<String> {
    let mut url = url::Url::parse(spec.authorize_url)
        .with_context(|| format!("bad authorize URL {}", spec.authorize_url))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in spec.extra_authorize_params {
            q.append_pair(k, v);
        }
        q.append_pair("client_id", spec.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &spec.scopes.join(" "))
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        if let (Some(param), Some(hint)) = (spec.login_hint_param, login_hint) {
            q.append_pair(param, hint);
        }
    }
    Ok(url.into())
}

// ---------------------------------------------------------------------------
// Loopback redirect
// ---------------------------------------------------------------------------

/// What one request to the loopback listener carried.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// The authorization code for our `state`.
    Code(String),
    /// The vendor redirected back with an OAuth error (user denied, …).
    Denied(String),
    /// Anything else (favicon, wrong path, stale tab with another `state`).
    Ignore,
}

/// Classify a request target (`/callback?code=…&state=…`). Pure.
fn parse_callback(target: &str, path: &str, state: &str) -> Callback {
    let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != path {
        return Callback::Ignore;
    }
    let get = |k: &str| {
        url.query_pairs()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.into_owned())
    };
    if get("state").as_deref() != Some(state) {
        return Callback::Ignore;
    }
    if let Some(err) = get("error") {
        return Callback::Denied(get("error_description").unwrap_or(err));
    }
    match get("code") {
        Some(code) if !code.is_empty() => Callback::Code(code),
        _ => Callback::Ignore,
    }
}

/// Tries per port while it is in use, and the pause between them (as
/// openai/codex `login/src/server.rs` `bind_server`: 10 × 200 ms).
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Bind the IPv4 loopback listener on `port`, retrying while it is in use.
/// `Ok(None)` = still in use after `attempts`; any other bind error fails at
/// once.
fn bind_v4_retrying(port: u16, attempts: u32, delay: Duration) -> Result<Option<TcpListener>> {
    for attempt in 1..=attempts.max(1) {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok(Some(listener)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if attempt < attempts {
                    std::thread::sleep(delay);
                }
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("listening on localhost:{port} for the sign-in redirect")
                })
            }
        }
    }
    Ok(None)
}

/// The ports a login with a fixed redirect port may listen on, in order: the
/// preferred one, then the provider's registered fallback.
fn redirect_port_candidates(port: u16, fallback: Option<u16>) -> Vec<u16> {
    let mut ports = vec![port];
    ports.extend(fallback.filter(|f| port != 0 && *f != port));
    ports
}

/// Bind the loopback redirect listener the way the Codex client does: retry
/// `port` while it is in use, then move to the registered `fallback` port
/// (retried the same way). Any other bind error fails at once. Unlike the
/// Codex client, no `/cancel` is sent to the port's holder: that may be the
/// user's own `codex login`, which usagio must not end.
#[cfg(test)]
fn bind_redirect_listener(
    port: u16,
    fallback: Option<u16>,
    attempts: u32,
    delay: Duration,
) -> Result<TcpListener> {
    let ports = redirect_port_candidates(port, fallback);
    for &candidate in &ports {
        if let Some(listener) = bind_v4_retrying(candidate, attempts, delay)? {
            return Ok(listener);
        }
    }
    Err(ports_in_use(&ports))
}

/// The error for a fixed-port login whose every candidate port is taken.
fn ports_in_use(ports: &[u16]) -> anyhow::Error {
    match ports {
        [port, fallback] => {
            anyhow!("sign-in redirect ports {port} and {fallback} are both in use")
        }
        [port, ..] => anyhow!("sign-in redirect port {port} is already in use"),
        [] => anyhow!("no sign-in redirect port"),
    }
}

struct Loopback {
    /// `127.0.0.1:port`, plus `[::1]:port` when the redirect host is the name
    /// `localhost`: the browser may resolve that to either address, and an
    /// address we left unbound could be taken by another local process to
    /// receive the redirect (code + state) instead of us.
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    path: &'static str,
}

/// Attempts at finding an ephemeral port free on both loopback addresses.
const DUAL_BIND_ATTEMPTS: usize = 8;

impl Loopback {
    pub(crate) fn bind(spec: &LoginSpec) -> Result<Self> {
        let both = spec.redirect_host.eq_ignore_ascii_case("localhost");
        match spec.redirect_port {
            Some(port) => Self::bind_fixed(spec, port, both),
            None => Self::bind_ephemeral(spec, both),
        }
    }

    /// A fixed redirect port: the preferred port, else the provider's
    /// registered fallback, whichever is free on every stack the redirect
    /// host resolves to. A port taken on either IPv4 or `[::1]` is skipped
    /// (listening beside a squatter would hand it the redirect); IPv4 is
    /// retried while busy first, as the vendor client does.
    fn bind_fixed(spec: &LoginSpec, port: u16, both: bool) -> Result<Self> {
        let ports = redirect_port_candidates(port, spec.fallback_redirect_port);
        let mut v6_err = None;
        for (i, &candidate) in ports.iter().enumerate() {
            if i > 0 {
                crate::logging::log(&format!(
                    "login: redirect port {port} is in use; using registered fallback port {candidate}"
                ));
            }
            let Some(v4) = bind_v4_retrying(candidate, BIND_ATTEMPTS, BIND_RETRY_DELAY)? else {
                continue;
            };
            let mut listeners = vec![v4];
            if both {
                match bind_v6_loopback(candidate) {
                    Ok(Some(v6)) => listeners.push(v6),
                    Ok(None) => {}
                    Err(e) => {
                        v6_err = Some(anyhow::Error::new(e).context(format!(
                            "listening on [::1]:{candidate} for the sign-in redirect"
                        )));
                        continue;
                    }
                }
            }
            return Self::serve(spec, candidate, listeners);
        }
        Err(v6_err.unwrap_or_else(|| ports_in_use(&ports)))
    }

    /// An ephemeral redirect port: any port free on every stack.
    fn bind_ephemeral(spec: &LoginSpec, both: bool) -> Result<Self> {
        let attempts = if both { DUAL_BIND_ATTEMPTS } else { 1 };
        let mut last_err = None;
        for _ in 0..attempts {
            let v4 = TcpListener::bind(("127.0.0.1", 0))
                .context("listening on localhost:0 for the sign-in redirect")?;
            let port = v4.local_addr()?.port();
            let mut listeners = vec![v4];
            if both {
                match bind_v6_loopback(port) {
                    Ok(Some(v6)) => listeners.push(v6),
                    Ok(None) => {}
                    Err(e) => {
                        // Someone holds [::1]:port; an ephemeral port can be
                        // picked again.
                        last_err = Some(anyhow::Error::new(e).context(format!(
                            "listening on [::1]:{port} for the sign-in redirect"
                        )));
                        continue;
                    }
                }
            }
            return Self::serve(spec, port, listeners);
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no free loopback port for the sign-in redirect")))
    }

    fn serve(spec: &LoginSpec, port: u16, listeners: Vec<TcpListener>) -> Result<Self> {
        for l in &listeners {
            l.set_nonblocking(true)?;
        }
        Ok(Loopback {
            listeners,
            redirect_uri: format!("http://{}:{port}{}", spec.redirect_host, spec.redirect_path),
            path: spec.redirect_path,
        })
    }

    /// The port the redirect comes back to.
    #[cfg(test)]
    fn port(&self) -> u16 {
        self.listeners[0].local_addr().unwrap().port()
    }

    /// Serve the redirect until it brings back a code for `state`, the user
    /// gives up (`cancel`), or `timeout` passes.
    fn wait_for_code(&self, state: &str, timeout: Duration, cancel: &AtomicBool) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                bail!("sign-in window closed");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for sign-in");
            }
            let mut idle = true;
            for listener in &self.listeners {
                match listener.accept() {
                    Ok((stream, _)) => {
                        idle = false;
                        match self.answer(stream, state, cancel) {
                            Callback::Code(code) => return Ok(code),
                            Callback::Denied(why) => bail!("sign-in was not authorized: {why}"),
                            Callback::Ignore => {}
                        }
                    }
                    Err(e) if is_transient_accept_error(e.kind()) => {}
                    Err(e) => return Err(e).context("accepting the sign-in redirect"),
                }
            }
            if idle {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn answer(&self, stream: TcpStream, state: &str, cancel: &AtomicBool) -> Callback {
        let line = read_request_line(&stream, cancel);
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let result = parse_callback(target, self.path, state);
        let (status, body) = match &result {
            Callback::Code(_) => (
                "200 OK",
                page(
                    "Sign-in received",
                    "Return to usagio to finish; the result is shown there.",
                ),
            ),
            Callback::Denied(why) => ("200 OK", page("Sign-in cancelled", why)),
            Callback::Ignore => ("404 Not Found", String::new()),
        };
        let mut s = &stream;
        let _ = write!(
            s,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.flush();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        result
    }
}

/// Bind `[::1]:port`. `Ok(None)` when this machine has no IPv6 loopback (so
/// `localhost` cannot resolve there and nothing can squat it); `Err` when the
/// address exists but is taken (`AddrInUse`, or refused to us).
fn bind_v6_loopback(port: u16) -> std::io::Result<Option<TcpListener>> {
    match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)) {
        Ok(l) => Ok(Some(l)),
        Err(e) if v6_absent(e.kind()) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Bind errors that mean "no IPv6 loopback here", not "somebody has it".
fn v6_absent(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::*;
    !matches!(kind, AddrInUse | PermissionDenied)
}

/// `accept()` errors on the nonblocking loopback listener that mean "try
/// again" rather than "the listener is broken": nothing pending, a signal
/// interrupted the call, or a client reset before we accepted it.
fn is_transient_accept_error(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        kind,
        WouldBlock | Interrupted | ConnectionAborted | ConnectionReset
    )
}

/// The request line of one request, read under an overall deadline and size
/// cap so an idle preconnect or a slow local client can't stall the wait loop
/// (which also polls for cancel and timeout). Gives up as soon as `cancel` is
/// set, so a superseded flow frees its (possibly fixed) port promptly. Reads
/// through the end of the headers so closing the socket after replying
/// doesn't reset it.
fn read_request_line(stream: &TcpStream, cancel: &AtomicBool) -> String {
    const MAX: usize = 8192;
    let deadline = Instant::now() + Duration::from_secs(3);
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let mut s = stream;
    while head.len() < MAX
        && Instant::now() < deadline
        && !head.ends_with(b"\r\n\r\n")
        && !cancel.load(Ordering::SeqCst)
    {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&head);
    head.lines().next().unwrap_or("").to_string()
}

fn page(title: &str, text: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<!doctype html><meta charset=utf-8><title>usagio</title>\
         <body style=\"font:15px -apple-system,system-ui,sans-serif;text-align:center;\
         padding-top:20vh\"><h2>{}</h2><p>{}</p>",
        esc(title),
        esc(text)
    )
}

// ---------------------------------------------------------------------------
// Code exchange
// ---------------------------------------------------------------------------

/// Trade the authorization code for tokens at the provider's token endpoint.
fn exchange(
    spec: &LoginSpec,
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<serde_json::Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let req = agent.post(spec.token_url);
    let resp = match spec.token_request {
        TokenRequestStyle::Json { include_state } => {
            let mut body = serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": redirect_uri,
                "client_id": spec.client_id,
                "code_verifier": verifier,
            });
            if include_state {
                body["state"] = state.into();
            }
            req.set("Content-Type", "application/json").send_json(body)
        }
        TokenRequestStyle::Form => req.send_form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", spec.client_id),
            ("code_verifier", verifier),
        ]),
    };
    match resp {
        Ok(r) => r.into_json().context("parsing the token response"),
        // Never echo the body: it can carry submitted secrets.
        Err(ureq::Error::Status(code, _)) => bail!("the sign-in was rejected (HTTP {code})"),
        Err(e) => bail!("exchanging the sign-in code failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Flow
// ---------------------------------------------------------------------------

/// Shared between the engine's worker thread and whatever shows the page.
#[derive(Default)]
pub(crate) struct Progress {
    finished: AtomicBool,
    cancelled: AtomicBool,
    superseded: AtomicBool,
}

impl Progress {
    /// The flow ended (success or failure) — close the sign-in page.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    /// Mark the flow ended, as its worker does on exit.
    #[cfg(test)]
    pub(crate) fn mark_finished(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }
    /// The sign-in page was closed, or a newer login superseded this flow.
    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    /// A newer login for the same provider replaced this flow.
    pub(crate) fn is_superseded(&self) -> bool {
        self.superseded.load(Ordering::SeqCst)
    }
    /// Stop this flow because a newer one is starting: like [`cancel`](Self::cancel),
    /// but the flow ends quietly (no failure notification).
    pub(crate) fn supersede(&self) {
        self.superseded.store(true, Ordering::SeqCst);
        self.cancel();
    }
    /// The user closed the sign-in page — stop waiting.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// A saved login.
#[derive(Debug, Clone)]
pub(crate) struct Renewed {
    pub(crate) key: String,
    /// When the new login dies (unix millis), if the provider says.
    pub(crate) login_expires_at: Option<i64>,
    /// The account was active, so the vendor CLI's live login was rewritten.
    pub(crate) made_live: bool,
}

/// A flow waiting for the user. Show `url`, then `progress` says when to stop.
pub(crate) struct Started {
    pub(crate) url: String,
    pub(crate) progress: Arc<Progress>,
}

/// What a login is for.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// Renew this account; a sign-in as anyone else is refused (nothing saved).
    Renew(String),
    /// Add whichever account signs in. `store` is the fresh sign-in window
    /// store it used, remembered on the account so its renewals reuse that
    /// session (see [`new_store_key`]).
    Add { store: String },
}

impl Target {
    fn expected(&self) -> Option<&str> {
        match self {
            Target::Renew(key) => Some(key),
            Target::Add { .. } => None,
        }
    }

    fn new_store(&self) -> Option<&str> {
        match self {
            Target::Add { store } => Some(store),
            Target::Renew(_) => None,
        }
    }
}

/// A never-used sign-in store key for "Sign in to a new account…".
pub(crate) fn new_store_key() -> String {
    format!("new:{}", random_token(12))
}

/// Wait for the redirect, then release the listener (and its port) before
/// returning: the caller signals completion only afterwards, so a successor
/// that binds the same fixed port (Codex: 1455) never races a live listener.
fn wait_and_release(
    loopback: Loopback,
    state: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<String> {
    let code = loopback.wait_for_code(state, timeout, cancel);
    drop(loopback);
    code
}

/// Begin a login for `provider`. `on_done` runs on the worker thread once the
/// flow ends.
pub(crate) fn start(
    provider: &'static dyn Provider,
    target: Target,
    on_done: impl FnOnce(Result<Renewed>) + Send + 'static,
) -> Result<Started> {
    let flow = provider
        .login_flow()
        .ok_or_else(|| anyhow!("{} can't be signed in from usagio", provider.display_name()))?;
    let spec = flow.login_spec();
    let pkce = pkce();
    let state = random_token(32);
    let loopback = Loopback::bind(&spec)?;
    let url = authorize_url(
        &spec,
        &pkce.challenge,
        &state,
        &loopback.redirect_uri,
        // The vendor knows the email, not usagio's workspace-qualified key.
        target
            .expected()
            .map(crate::providers::trait_def::key_email),
    )?;
    let redirect_uri = loopback.redirect_uri.clone();
    let progress = Arc::new(Progress::default());
    let worker = Arc::clone(&progress);
    // The worker logs through the config dir.
    crate::store::spawn_in_config_scope(move || {
        let result = (|| {
            let code = wait_and_release(loopback, &state, LOGIN_TIMEOUT, &worker.cancelled)?;
            let tokens = exchange(&spec, &code, &pkce.verifier, &state, &redirect_uri)?;
            let captured = flow.complete_login(&tokens).map_err(|e| anyhow!("{e}"))?;
            check_signed_in_as(
                &provider.account_identifier(&captured.identity),
                target.expected(),
            )?;
            crate::cli::capture::persist_login(provider, captured, target.new_store())
        })();
        worker.finished.store(true, Ordering::SeqCst);
        crate::logging::log(&match &result {
            Ok(r) => format!(
                "event=login_renewed provider={} account={} made_live={}",
                provider.provider_id(),
                r.key,
                r.made_live
            ),
            Err(e) => format!(
                "event=login_failed provider={} reason={e:#}",
                provider.provider_id()
            ),
        });
        // A flow replaced by a newer sign-in ends quietly: its failure
        // notice would read as an error beside the new attempt.
        if worker.is_superseded() && result.is_err() {
            return;
        }
        on_done(result);
    });
    Ok(Started { url, progress })
}

/// Refuse a sign-in as anyone but the account being renewed: the window's
/// cookie store could hold another account's session.
fn check_signed_in_as(key: &str, expected: Option<&str>) -> Result<()> {
    use crate::providers::trait_def::{key_email, WORKSPACE_KEY_SEP};
    match expected {
        // A legacy email-only key (its workspace was never recorded) is
        // renewed by any workspace of that email, which then upgrades it.
        Some(want)
            if !want.contains(WORKSPACE_KEY_SEP) && key_email(key).eq_ignore_ascii_case(want) =>
        {
            Ok(())
        }
        Some(want) if !key.eq_ignore_ascii_case(want) => {
            bail!("signed in as {key}, not {want} — nothing was changed")
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Where the sign-in page is shown
// ---------------------------------------------------------------------------

/// Stable per-account identity for the sign-in window's cookie store, so each
/// account keeps its own vendor session across renewals.
pub(crate) fn store_id(slug: &str, key: &str) -> [u8; 16] {
    let digest = Sha256::digest(format!("usagio-login:{slug}:{}", key.to_ascii_lowercase()));
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

/// Per-account web data folder (Windows/Linux; macOS uses [`store_id`]).
// WKWebView stores are keyed by `store_id` instead, so unused on macOS.
#[allow(dead_code)]
pub(crate) fn store_dir(slug: &str, key: &str) -> Result<PathBuf> {
    let hex: String = store_id(slug, key)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(crate::store::config_dir()?.join("webview").join(hex))
}
