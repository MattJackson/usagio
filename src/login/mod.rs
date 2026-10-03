//! "Renew login…": one engine for every provider's browser login.
//!
//! Every vendor CLI logs in the same way — OAuth 2.0 authorization code with
//! PKCE (RFC 7636) and a loopback redirect. A provider describes its endpoints
//! in a [`LoginSpec`] and turns the token response into its own credential
//! blob ([`Provider::complete_login`]); this module does the rest:
//!
//! 1. mint a PKCE verifier/challenge and a `state`,
//! 2. listen on `localhost` for the redirect,
//! 3. show the authorize page — in usagio's own sign-in window
//!    (`Platform::open_sign_in_window`),
//!    which keeps a separate, persistent cookie store per account, or in the
//!    default browser as a fallback,
//! 4. exchange the code, check the account that signed in is the one being
//!    renewed, and save it (`crate::persist_login`).

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
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// An account whose login dies within this window is offered "Renew login…".
/// Matches Claude Code's own "Your login expires in N days" warning (3 days).
pub const RENEW_WINDOW_SECS: i64 = 3 * 24 * 60 * 60;

/// Whether to warn now that a login expiring at `expires_at` is about to die:
/// inside the renew window, not yet dead, and not already warned for this
/// exact expiry. Pure.
pub fn should_warn_expiry(expires_at: Option<i64>, now_ms: i64, warned: Option<i64>) -> bool {
    let Some(at) = expires_at else {
        return false;
    };
    at > now_ms && at - now_ms <= RENEW_WINDOW_SECS * 1000 && warned != Some(at)
}

// ---------------------------------------------------------------------------
// PKCE + authorize URL
// ---------------------------------------------------------------------------

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// RFC 7636 S256: a 256-bit random verifier and its SHA-256 challenge.
pub fn pkce() -> Pkce {
    let verifier = random_token(32);
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// `BASE64URL(SHA256(verifier))`, RFC 7636 §4.2.
pub fn challenge_for(verifier: &str) -> String {
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

pub fn authorize_url(
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
pub enum Callback {
    /// The authorization code for our `state`.
    Code(String),
    /// The vendor redirected back with an OAuth error (user denied, …).
    Denied(String),
    /// Anything else (favicon, wrong path, stale tab with another `state`).
    Ignore,
}

/// Classify a request target (`/callback?code=…&state=…`). Pure.
pub fn parse_callback(target: &str, path: &str, state: &str) -> Callback {
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

pub struct Loopback {
    listener: TcpListener,
    pub redirect_uri: String,
    path: &'static str,
}

impl Loopback {
    pub fn bind(spec: &LoginSpec) -> Result<Self> {
        let port = spec.redirect_port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("listening on localhost:{port} for the sign-in redirect"))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        Ok(Loopback {
            listener,
            redirect_uri: format!("http://localhost:{port}{}", spec.redirect_path),
            path: spec.redirect_path,
        })
    }

    /// Serve the redirect until it brings back a code for `state`, the user
    /// gives up (`cancel`), or `timeout` passes.
    pub fn wait_for_code(
        &self,
        state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                bail!("sign-in window closed");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for sign-in");
            }
            match self.listener.accept() {
                Ok((stream, _)) => match self.answer(stream, state) {
                    Callback::Code(code) => return Ok(code),
                    Callback::Denied(why) => bail!("sign-in was not authorized: {why}"),
                    Callback::Ignore => {}
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e).context("accepting the sign-in redirect"),
            }
        }
    }

    fn answer(&self, stream: TcpStream, state: &str) -> Callback {
        let line = read_request_line(&stream);
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let result = parse_callback(target, self.path, state);
        let (status, body) = match &result {
            Callback::Code(_) => ("200 OK", page("Signed in", "You can close this window.")),
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

/// The request line of one request, read under an overall deadline and size
/// cap so an idle preconnect or a slow local client can't stall the wait loop
/// (which also polls for cancel and timeout). Reads through the end of the
/// headers so closing the socket after replying doesn't reset it.
fn read_request_line(stream: &TcpStream) -> String {
    const MAX: usize = 8192;
    let deadline = Instant::now() + Duration::from_secs(3);
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let mut s = stream;
    while head.len() < MAX && Instant::now() < deadline && !head.ends_with(b"\r\n\r\n") {
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
pub fn exchange(
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
pub struct Progress {
    finished: AtomicBool,
    cancelled: AtomicBool,
}

impl Progress {
    /// The flow ended (success or failure) — close the sign-in page.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    /// The user closed the sign-in page — stop waiting.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// A saved login.
#[derive(Debug, Clone)]
pub struct Renewed {
    pub key: String,
    /// When the new login dies (unix millis), if the provider says.
    pub login_expires_at: Option<i64>,
    /// The account was active, so the vendor CLI's live login was rewritten.
    pub made_live: bool,
}

/// A flow waiting for the user. Show `url`, then `progress` says when to stop.
pub struct Started {
    pub url: String,
    pub progress: Arc<Progress>,
}

/// Begin a login for `provider`. `expected` is the account being renewed; a
/// sign-in as anyone else is refused (nothing saved). `on_done` runs on the
/// worker thread once the flow ends.
pub fn start(
    provider: &'static dyn Provider,
    expected: Option<String>,
    on_done: impl FnOnce(Result<Renewed>) + Send + 'static,
) -> Result<Started> {
    let spec = provider
        .login_spec()
        .ok_or_else(|| anyhow!("{} can't be signed in from usagio", provider.display_name()))?;
    let pkce = pkce();
    let state = random_token(32);
    let loopback = Loopback::bind(&spec)?;
    let url = authorize_url(
        &spec,
        &pkce.challenge,
        &state,
        &loopback.redirect_uri,
        expected.as_deref(),
    )?;
    let progress = Arc::new(Progress::default());
    let worker = Arc::clone(&progress);
    std::thread::spawn(move || {
        let result = (|| {
            let code = loopback.wait_for_code(&state, LOGIN_TIMEOUT, &worker.cancelled)?;
            let tokens = exchange(&spec, &code, &pkce.verifier, &state, &loopback.redirect_uri)?;
            let captured = provider
                .complete_login(&tokens)
                .map_err(|e| anyhow!("{e}"))?;
            check_signed_in_as(
                &provider.account_identifier(&captured.identity),
                expected.as_deref(),
            )?;
            crate::persist_login(provider, captured)
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
        on_done(result);
    });
    Ok(Started { url, progress })
}

/// Refuse a sign-in as anyone but the account being renewed: the window's
/// cookie store could hold another account's session.
pub fn check_signed_in_as(key: &str, expected: Option<&str>) -> Result<()> {
    match expected {
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
pub fn store_id(slug: &str, key: &str) -> [u8; 16] {
    let digest = Sha256::digest(format!("usagio-login:{slug}:{}", key.to_ascii_lowercase()));
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

/// Per-account web data folder (Windows/Linux; macOS uses [`store_id`]).
// WKWebView stores are keyed by `store_id` instead, so unused on macOS.
#[allow(dead_code)]
pub fn store_dir(slug: &str, key: &str) -> Result<PathBuf> {
    let hex: String = store_id(slug, key)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(crate::store::config_dir()?.join("webview").join(hex))
}
