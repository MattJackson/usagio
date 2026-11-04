//! Shared test fixtures: mock providers, mock HTTP servers, and seeded
//! accounts used by tests across modules.

use crate::claude_local::write_claude_identity;
use crate::poll::claude::{refresh_usage_cache, RefreshOutcome};
use crate::poll::trackers::{claim_subscription_check, note_polled_active};
use crate::providers::trait_def::Provider;
use crate::providers::CLAUDE_SLUG;
use crate::rows::{Cell, Row};
use crate::store::{Account, CachedUsage, State};
use crate::{providers, store};
use chrono::{DateTime, Duration, Utc};

// --- Row helpers ---

pub(crate) fn cell(pct: Option<f64>) -> Cell {
    Cell {
        pct,
        resets_at: None,
    }
}

pub(crate) fn row(session: Option<f64>, weekly: Option<f64>) -> Row {
    Row {
        provider_id: CLAUDE_SLUG.to_string(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
        email: "x@e.com".to_string(),
        session: cell(session),
        weekly: cell(weekly),
        opus: None,
        error: None,
        fetched_at: Some(0),
        reported: Default::default(),
    }
}

/// A row with an email and a weekly reset time, for pick/order tests.
pub(crate) fn row_full(email: &str, session: f64, weekly: f64, weekly_reset: DateTime<Utc>) -> Row {
    row_full_with_provider(CLAUDE_SLUG, email, session, weekly, weekly_reset)
}

/// Like `row_full`, but with an explicit provider slug — for tests that need
/// to verify the swap-capability gate on non-Claude / unregistered slugs.
fn row_full_with_provider(
    provider_id: &str,
    email: &str,
    session: f64,
    weekly: f64,
    weekly_reset: DateTime<Utc>,
) -> Row {
    Row {
        provider_id: provider_id.to_string(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
        email: email.to_string(),
        session: Cell {
            pct: Some(session),
            resets_at: None,
        },
        weekly: Cell {
            pct: Some(weekly),
            resets_at: Some(weekly_reset),
        },
        opus: None,
        error: None,
        fetched_at: Some(Utc::now().timestamp()),
        reported: Default::default(),
    }
}

// --- v0.8.0 smart-cadence tighten (projection can only speed up cadence) ---

// --- v0.8.0 auto-swap escalation on repeated /oauth/usage 429s ---

/// Tests share the process-global `USAGE_FETCH_TRACKERS` map; use unique emails
/// per test so state can't cross-contaminate. Reset the caller's entry at the
/// end of each test to keep the map bounded even in long test-runner sessions.
pub(crate) fn escalation_test_email(tag: &str) -> String {
    format!(
        "escalation-{tag}-{}-{}@test",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

// --- merged_cached_usage (re-capture preserves usage) ---

pub(crate) fn acct_with_cache(cache: Option<CachedUsage>) -> Account {
    let mut a = Account::from_keychain_blob(
        r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":0}}"#,
    )
    .unwrap();
    a.cached_usage = cache;
    a
}

// ---------------------------------------------------------------------------
// Active-account CAS refresh (v0.5.0). `active_refresh_cas` is exercised
// directly against a `MockActiveSlotProvider` rather than through the full
// `refresh_usage_cache()` — the real `ClaudeProvider::read_active_slot` /
// `mirror_rotated_token` on macOS route through the REAL login keychain
// (`Platform::secrets()`), which unit tests must never touch (see the
// `#[ignore]`d `secret_store_roundtrip` in `platform/macos.rs` for the same
// rule). The mock provider gives `active_refresh_cas` an in-memory "OS-native
// slot" it can read/write freely, while the `/token` POST itself still goes
// through the same thread-local URL override + in-process mock HTTP server
// every other refresh test in this file uses (`oauth::refresh` is
// Claude-specific and not swappable per-provider).
// ---------------------------------------------------------------------------

/// Minimal `Provider` stand-in whose "OS-native active slot" is an in-memory
/// `Mutex<Option<String>>` instead of the real keychain / credentials file.
/// Every method besides `read_active_slot` / `mirror_rotated_token` is an
/// inert default — `active_refresh_cas` never calls them.
pub(crate) struct MockActiveSlotProvider {
    pub(crate) client_running: bool,
    pub(crate) writes: std::sync::atomic::AtomicUsize,
    pub(crate) fail_handoff: std::sync::atomic::AtomicBool,
    pub(crate) fail_drain: std::sync::atomic::AtomicBool,
    pub(crate) retry_drain: bool,
    pub(crate) process_alive: std::sync::atomic::AtomicBool,
    pub(crate) drains: std::sync::Mutex<Vec<String>>,
    pub(crate) lock_held_at_handoff: std::sync::atomic::AtomicBool,
    pub(crate) email: String,
    pub(crate) slot: std::sync::Mutex<Option<String>>,
}

impl MockActiveSlotProvider {
    pub(crate) fn new(initial: &str) -> Self {
        Self {
            client_running: false,
            writes: std::sync::atomic::AtomicUsize::new(0),
            fail_handoff: std::sync::atomic::AtomicBool::new(false),
            fail_drain: std::sync::atomic::AtomicBool::new(false),
            retry_drain: false,
            process_alive: std::sync::atomic::AtomicBool::new(false),
            drains: std::sync::Mutex::new(Vec::new()),
            lock_held_at_handoff: std::sync::atomic::AtomicBool::new(false),
            email: "active@example.com".into(),
            slot: std::sync::Mutex::new(Some(initial.to_string())),
        }
    }

    /// Simulate Claude Code rotating the slot out from under usagio, as if a
    /// live `claude` session refreshed independently.
    #[allow(dead_code)]
    pub(crate) fn rotate_externally(&self, new_blob: &str) {
        *self.slot.lock().unwrap() = Some(new_blob.to_string());
    }
}

impl crate::providers::Provider for MockActiveSlotProvider {
    fn provider_id(&self) -> &'static str {
        "mock-active-slot"
    }
    fn display_name(&self) -> &'static str {
        "Mock"
    }
    fn capabilities(&self) -> crate::providers::trait_def::Capabilities {
        crate::providers::trait_def::Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: false,
            supports_email_capture: false,
            secret_backend: crate::providers::trait_def::SecretBackend::Keychain,
            capture_mode: crate::providers::trait_def::CaptureMode::CredsOnDisk,
        }
    }

    fn login_capture(&self) -> Option<&dyn crate::providers::LoginCapture> {
        Some(self)
    }

    fn active_slot(&self) -> Option<&dyn crate::providers::ActiveSlot> {
        Some(self)
    }

    fn client_handoff(&self) -> Option<&dyn crate::providers::ClientHandoff> {
        Some(self)
    }
}

impl crate::providers::LoginCapture for MockActiveSlotProvider {
    fn capture_current_login(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<crate::providers::trait_def::CapturedAccount>>
    {
        Ok(None)
    }
}

impl crate::providers::ActiveSlot for MockActiveSlotProvider {
    fn write_active_account(
        &self,
        blob: &str,
        _identity: &providers::IdentitySnapshot,
    ) -> providers::PResult<()> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.slot.lock().unwrap() = Some(blob.into());
        Ok(())
    }
    fn read_active_identity(
        &self,
    ) -> crate::providers::trait_def::PResult<Option<providers::IdentitySnapshot>> {
        Ok(Some(providers::IdentitySnapshot {
            email: Some(self.email.clone()),
            uuid: None,
            display_name: None,
            native_blob: serde_json::Value::Null,
        }))
    }
    fn read_active_slot(&self) -> crate::providers::trait_def::PResult<Option<String>> {
        Ok(self.slot.lock().unwrap().clone())
    }
    fn mirror_rotated_token(&self, blob: &str) -> crate::providers::trait_def::PResult<()> {
        *self.slot.lock().unwrap() = Some(blob.to_string());
        Ok(())
    }
}

impl crate::providers::ClientHandoff for MockActiveSlotProvider {
    fn verify_handoff_stopped(&self, _token: &str) -> providers::PResult<()> {
        if self.process_alive.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(providers::ProviderError::Other(
                "still releasing sessions".into(),
            ));
        }
        Ok(())
    }
    fn handoff_request_is_idempotent(&self) -> bool {
        self.retry_drain
    }
    fn client_status(&self) -> providers::PResult<providers::ClientStatus> {
        Ok(providers::ClientStatus {
            running: self.client_running,
            busy: self.client_running,
            account_key: Some(self.email.clone()),
            account_id: None,
            handoff_token: self.client_running.then(|| "renewal-instance".into()),
        })
    }
    fn handoff_client(
        &self,
        _identity: &providers::IdentitySnapshot,
        _token: Option<&str>,
    ) -> providers::PResult<()> {
        self.lock_held_at_handoff.store(
            crate::credentials::state_lock_held(),
            std::sync::atomic::Ordering::SeqCst,
        );
        if self
            .fail_handoff
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(providers::ProviderError::Other(
                "verification timed out".into(),
            ));
        }
        Ok(())
    }
    fn begin_handoff(&self, token: &str) -> providers::PResult<()> {
        self.drains.lock().unwrap().push(token.to_owned());
        // The engine must persist intent before a potentially disruptive RPC.
        let state = State::load().unwrap();
        assert_eq!(
            state
                .provider_accounts(self.provider_id())
                .unwrap()
                .pending_switch
                .as_ref()
                .unwrap()
                .drain_token
                .as_deref(),
            Some(token)
        );
        if self
            .fail_drain
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(providers::ProviderError::Other(
                "drain delivery failed".into(),
            ));
        }
        Ok(())
    }

    fn handoff_stops_synchronously(&self) -> bool {
        false
    }
}

pub(crate) fn mock_claude_blob(access: &str, refresh: &str, expires_at: i64) -> String {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    })
    .to_string()
}

/// Minimal single-threaded mock token endpoint: accepts TCP connections,
/// reads the full request, and replies with a fixed valid refresh-grant JSON
/// body. Returns (base_url, hits) where `hits` is bumped once per accepted
/// connection.
///
/// The request is read to completion (request line + headers up to the blank
/// line + any `Content-Length` body) BEFORE the response is written, and the
/// write half is shut down explicitly after flushing. A prior version issued a
/// single `stream.read(&mut buf)` and closed the socket as soon as it had
/// written the response. Under parallel load that raced ureq's request write
/// two ways: the single read could return before ureq finished sending the
/// body (a request split across TCP segments), and closing the socket with
/// unread inbound data still buffered makes the kernel send an RST, which ureq
/// surfaces mid-response as `Transient("parsing token response: Failed to read
/// JSON: Invalid argument (os error 22)")`. That dropped the refresh on the
/// floor (kept-cache path) and flaked
/// `refresh_usage_cache_still_refreshes_inactive_accounts` ~25-50% of full
/// parallel runs. Draining the request first, then a graceful `shutdown(Write)`,
/// closes both windows (the fix the old `active_refresh_*` TODO called for).
pub(crate) fn spawn_mock_token_server() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>)
{
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock token server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            // Read the whole request before responding. `TcpStream` is `Read`
            // + `Write` through a shared `&TcpStream`, so a `BufReader` borrow
            // for the request and direct writes for the response can coexist.
            let mut reader = BufReader::new(&stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            // Consume headers up to the blank line, tracking Content-Length so
            // the body is fully drained (avoids a close-with-unread-data RST).
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header == "\r\n" || header == "\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            if content_length > 0 {
                let mut body_buf = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body_buf);
            }
            let is_token_post = request_line.starts_with("POST");
            let body = if is_token_post {
                hits_thread.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "access_token": "mock-refreshed-access-token",
                    "refresh_token": "mock-refreshed-refresh-token",
                    "expires_in": 3600,
                })
                .to_string()
            } else {
                // Usage GET — any well-formed empty snapshot avoids a parse
                // error; this test doesn't assert on usage contents.
                serde_json::json!({
                    "five_hour": null,
                    "seven_day": null,
                    "seven_day_opus": null,
                })
                .to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            drop(reader);
            let mut stream = stream;
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            // Graceful half-close: signal EOF to ureq once the full response is
            // out, rather than dropping the socket abruptly.
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

// ---------------------------------------------------------------------------
// codex-switch-e2e: generic (non-Claude) capture / switch / CAS dispatch.
// Every test below uses `ScopedConfigDir` (state.json → tempdir) and
// `env_lock::scoped_env_var("CODEX_HOME", ...)` (auth.json → tempdir), so
// none of them ever touch a real `~/.codex/auth.json` or the real OS
// keychain — consistent with the crate-wide test-hermeticity contract.
// ---------------------------------------------------------------------------

pub(crate) fn codex_id_token(email: &str, exp: i64) -> String {
    use base64::Engine;
    let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({ "email": email, "exp": exp })
            .to_string()
            .as_bytes(),
    );
    format!("{hdr}.{payload}.sig")
}

/// A full Codex `auth.json` blob for `email`, expiring at `exp` (unix
/// seconds). Mirrors `providers::codex::mod::tests::make_blob`, duplicated
/// here rather than imported since it's `#[cfg(test)]`-private to that
/// module and this file's edit scope doesn't include changing that.
pub(crate) fn codex_auth_json(email: &str, exp: i64, access: &str, refresh: &str) -> String {
    let id_token = codex_id_token(email, exp);
    serde_json::json!({
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
        }
    })
    .to_string()
}

pub(crate) fn usage_at(pct: f64) -> CachedUsage {
    CachedUsage {
        session_pct: Some(pct),
        weekly_pct: Some(10.0),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: Utc::now().timestamp(),
        reported: Default::default(),
    }
}

// --- mutation-hardening: swap decision core ---

// --- mutation-hardening: swap verification + stale rescue ---

/// Mock usage endpoint: answers every request with `status` and `body`,
/// counting hits.
pub(crate) fn spawn_mock_usage_server(
    status: u16,
    body: String,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock usage server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut content_length = 0usize;
            {
                use std::io::Read;
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0
                        || line == "\r\n"
                        || line == "\n"
                    {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                // Drain the body so closing never resets the connection mid-request.
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
            }
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
    (format!("http://{addr}"), hits)
}

/// A `/api/oauth/usage` body: both windows reset in the future.
pub(crate) fn usage_body(session: f64, weekly: f64) -> String {
    let session_reset = (Utc::now() + Duration::hours(3)).to_rfc3339();
    let weekly_reset = (Utc::now() + Duration::days(3)).to_rfc3339();
    serde_json::json!({
        "five_hour": {"utilization": session, "resets_at": session_reset},
        "seven_day": {"utilization": weekly, "resets_at": weekly_reset},
        "seven_day_opus": null,
    })
    .to_string()
}

/// A Claude account whose token is good for another hour (`expires_in_ms`
/// negative for an already-expired one).
pub(crate) fn claude_account(email: &str, expires_in_ms: i64) -> Account {
    let exp = Utc::now().timestamp_millis() + expires_in_ms;
    let mut a = Account::from_keychain_blob(&format!(
        r#"{{"claudeAiOauth":{{"accessToken":"at-{email}","refreshToken":"rt-{email}","expiresAt":{exp}}}}}"#
    ))
    .unwrap();
    a.email = Some(email.to_string());
    a
}

pub(crate) fn seed_claude_state(accounts: Vec<Account>, active: Option<&str>) {
    let mut st = State::default();
    for a in accounts {
        st.upsert(a);
    }
    st.active = active.map(str::to_string);
    st.save().unwrap();
}

pub(crate) fn hit_count(h: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
    h.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serializes the tests that observe or move the process-global "last polled
/// active account" (every `refresh_usage_cache` call touches it).
pub(crate) static POLL_ACTIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn plan(label: &str, active: bool) -> crate::providers::trait_def::PlanStatus {
    crate::providers::trait_def::PlanStatus {
        label: Some(label.to_string()),
        active,
    }
}

pub(crate) fn read_log() -> String {
    std::fs::read_to_string(store::config_dir().unwrap().join("usagio.log")).unwrap_or_default()
}

// --- mutation-hardening: switching, identity, keychain sync ---

pub(crate) fn claude_account_with_identity(email: &str, expires_in_ms: i64) -> Account {
    let mut a = claude_account(email, expires_in_ms);
    a.oauth_account = Some(serde_json::json!({
        "emailAddress": email,
        "accountUuid": format!("uuid-{email}"),
    }));
    a
}

/// Run `f` with `$HOME` and the config dir both pointing at a scratch dir
/// holding a `~/.claude.json` for `json_email`.
pub(crate) fn with_claude_home<R>(json_email: &str, f: impl FnOnce(&std::path::Path) -> R) -> R {
    let scd = crate::store::ScopedConfigDir::new();
    let home = scd.home();
    crate::env_lock::scoped_env_var("HOME", Some(home.to_str().unwrap()), || {
        std::fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {
                    "emailAddress": json_email,
                    "accountUuid": format!("uuid-{json_email}"),
                },
                "userID": "user-1",
            })
            .to_string(),
        )
        .unwrap();
        f(&home)
    })
}

/// Put `blob` where the vendor CLI's active login lives on this OS.
pub(crate) fn prime_active_slot(blob: &str) {
    providers::get(CLAUDE_SLUG)
        .unwrap()
        .active_slot()
        .unwrap()
        .mirror_rotated_token(blob)
        .unwrap();
}

pub(crate) fn prime_active_account(account: &Account) {
    let identity = account.oauth_account.clone().unwrap_or_else(|| {
        serde_json::json!({
            "emailAddress": account.email,
        })
    });
    write_claude_identity(&identity, None).unwrap();
    prime_active_slot(&account.keychain_blob);
}

// --- mutation-hardening: the Claude usage refresh policy ---

pub(crate) fn uniq(tag: &str) -> String {
    escalation_test_email(tag)
}

/// A cached reading `age` seconds old.
pub(crate) fn cu(session: f64, weekly: f64, age: i64) -> CachedUsage {
    CachedUsage {
        session_pct: Some(session),
        weekly_pct: Some(weekly),
        session_reset: None,
        weekly_reset: None,
        opus_pct: None,
        opus_reset: None,
        fetched_at: Utc::now().timestamp() - age,
        reported: Default::default(),
    }
}

pub(crate) fn account_with_cache(email: &str, cache: Option<CachedUsage>) -> Account {
    let mut a = claude_account_with_identity(email, 3_600_000);
    a.cached_usage = cache;
    a
}

pub(crate) struct Fx {
    pub(crate) accounts: Vec<Account>,
    pub(crate) active: Option<String>,
    pub(crate) force: bool,
    pub(crate) status: u16,
    pub(crate) body: String,
    /// Make this look like the first poll since a switch.
    pub(crate) just_switched: bool,
    /// Token endpoint behaviour, if the scenario refreshes tokens.
    pub(crate) token: Option<(u16, String)>,
    /// Runs inside the scratch config dir before the pass (e.g. seed history).
    pub(crate) before: Option<Box<dyn FnOnce()>>,
}

impl Fx {
    pub(crate) fn new(accounts: Vec<Account>, active: Option<&str>) -> Self {
        Fx {
            accounts,
            active: active.map(str::to_string),
            force: false,
            status: 200,
            body: usage_body(10.0, 10.0),
            just_switched: false,
            token: None,
            before: None,
        }
    }
    pub(crate) fn status(mut self, status: u16, body: &str) -> Self {
        self.status = status;
        self.body = body.to_string();
        self
    }
    pub(crate) fn force(mut self) -> Self {
        self.force = true;
        self
    }
    pub(crate) fn just_switched(mut self) -> Self {
        self.just_switched = true;
        self
    }
}

/// Run one `refresh_usage_cache` pass against mock endpoints inside a scratch
/// `$HOME`; `check` sees the outcome, the usage-endpoint hit count and the
/// token-endpoint hit count while the scratch state still exists.
pub(crate) fn run_refresh<R>(fx: Fx, check: impl FnOnce(&RefreshOutcome, usize, usize) -> R) -> R {
    use crate::providers::claude::{oauth, usage};
    let _poll = POLL_ACTIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_claude_home("scratch-json@e.com", |_| {
        let (url, hits) = spawn_mock_usage_server(fx.status, fx.body.clone());
        usage::set_usage_url_override(Some(&format!("{url}/api/oauth/usage")));
        let token = fx.token.clone().map(|(s, b)| spawn_mock_usage_server(s, b));
        if let Some((u, _)) = &token {
            oauth::set_token_url_override(Some(&format!("{u}/v1/oauth/token")));
        }
        let now = Utc::now().timestamp();
        for a in &fx.accounts {
            // Throttle the lapse probe so a 403/429 never reaches the profile
            // endpoint.
            claim_subscription_check(a.key(), now, false);
        }
        if let Some(active) = &fx.active {
            let account = fx.accounts.iter().find(|a| a.key() == active).unwrap();
            prime_active_account(account);
        }
        seed_claude_state(fx.accounts.clone(), fx.active.as_deref());
        if let Some(before) = fx.before {
            before();
        }
        if fx.just_switched {
            note_polled_active(Some("someone-else@e.com"));
        } else {
            note_polled_active(fx.active.as_deref());
        }
        let out = refresh_usage_cache(fx.force);
        usage::set_usage_url_override(None);
        oauth::set_token_url_override(None);
        let token_hits = token.as_ref().map(|(_, h)| hit_count(h)).unwrap_or(0);
        check(&out, hit_count(&hits), token_hits)
    })
}

/// Number of usage requests a pass makes.
pub(crate) fn fetches(fx: Fx) -> usize {
    run_refresh(fx, |_, hits, _| hits)
}

pub(crate) fn claude_expired(email: &str) -> Account {
    let mut a = claude_account_with_identity(email, -60_000);
    a.cached_usage = Some(cu(10.0, 10.0, 2000));
    a
}

// --- mutation-hardening: the provider (Codex) usage refresh policy ---
//
// Both endpoints are local mocks. Inactive expired accounts reach refresh;
// active accounts reach usage directly, without consuming a refresh token.
// Either request proves the account passed its scheduling gates.

pub(crate) fn codex_account(
    key: &str,
    cache: Option<CachedUsage>,
) -> crate::store::ProviderAccount {
    crate::store::ProviderAccount {
        key: key.to_string(),
        secret_blob: "{}".to_string(),
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
        expires_at: Utc::now().timestamp() - 100,
        identity_email: None,
        identity_uuid: None,
        identity_display_name: None,
        identity_native_blob: serde_json::Value::Null,
        cached_usage: cache,
        notif_state: Default::default(),
        needs_relogin: false,
        no_subscription: false,
        plan: None,
    }
}

pub(crate) fn history_events() -> Vec<serde_json::Value> {
    std::fs::read_to_string(store::config_dir().unwrap().join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// --- mutation-hardening: exact-second boundaries of the refresh policy ---

/// Run `f` with the current wall-clock second until the second did not roll
/// over while it ran, so a boundary-exact timestamp stays exact.
pub(crate) fn at_stable_second<R>(mut f: impl FnMut(i64) -> R) -> R {
    for _ in 0..25 {
        let t0 = Utc::now().timestamp();
        let r = f(t0);
        if Utc::now().timestamp() == t0 {
            return r;
        }
    }
    panic!("wall clock never stable");
}

pub(crate) fn cu_at(session: f64, weekly: f64, fetched_at: i64) -> CachedUsage {
    let mut c = cu(session, weekly, 0);
    c.fetched_at = fetched_at;
    c
}

pub(crate) fn codex_login_response(email: &str, account_id: &str) -> serde_json::Value {
    use base64::Engine;
    let claims = serde_json::json!({
        "email": email, "exp": 4_000_000_000_i64,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id, "chatgpt_user_id": format!("user-{account_id}")},
    });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    serde_json::json!({"id_token": format!("e30.{payload}.sig"), "access_token": "renewed-at", "refresh_token": "renewed-rt"})
}

/// [`crate::lifecycle::active_refresh_cas`] under the state lock it requires.
pub(crate) fn locked_active_refresh_cas(
    provider: &dyn Provider,
    acct: &mut Account,
) -> crate::lifecycle::ActiveRefreshOutcome {
    crate::credentials::with_state_lock(|lock| {
        Ok(crate::lifecycle::active_refresh_cas(lock, provider, acct))
    })
    .unwrap()
}

/// [`crate::lifecycle::prepare_handoff`] under the refresh and state locks it
/// requires.
pub(crate) fn locked_prepare_handoff(
    provider: &dyn Provider,
    status: &crate::providers::ClientStatus,
    state: &mut State,
    key: &str,
    manual: bool,
) -> anyhow::Result<bool> {
    let refresh = crate::lifecycle::refresh_lock()?;
    crate::credentials::with_state_lock(|lock| {
        crate::lifecycle::prepare_handoff(lock, &refresh, provider, status, state, key, manual)
    })
}
