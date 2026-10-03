use super::*;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::AtomicBool;

fn spec(token_url: &'static str, style: TokenRequestStyle) -> LoginSpec {
    LoginSpec {
        authorize_url: "https://auth.example.com/oauth/authorize",
        token_url,
        client_id: "client-123",
        scopes: &["user:profile", "user:inference"],
        redirect_port: None,
        redirect_path: "/callback",
        extra_authorize_params: &[("code", "true")],
        login_hint_param: Some("login_hint"),
        token_request: style,
    }
}

#[test]
fn pkce_matches_the_rfc_7636_test_vector() {
    assert_eq!(
        challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let p = pkce();
    assert_eq!(p.verifier.len(), 43, "32 bytes, base64url, no padding");
    assert_eq!(p.challenge, challenge_for(&p.verifier));
    assert_ne!(pkce().verifier, p.verifier);
}

#[test]
fn authorize_url_carries_every_oauth_param() {
    let s = spec("http://unused", TokenRequestStyle::Form);
    let url = authorize_url(
        &s,
        "chal",
        "st8",
        "http://localhost:5/callback",
        Some("dev@example.com"),
    )
    .unwrap();
    let u = url::Url::parse(&url).unwrap();
    let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
    assert_eq!(u.host_str(), Some("auth.example.com"));
    assert_eq!(q["code"], "true");
    assert_eq!(q["client_id"], "client-123");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["redirect_uri"], "http://localhost:5/callback");
    assert_eq!(q["scope"], "user:profile user:inference");
    assert_eq!(q["code_challenge"], "chal");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["state"], "st8");
    assert_eq!(q["login_hint"], "dev@example.com");
}

#[test]
fn authorize_url_omits_the_hint_when_the_vendor_has_none() {
    let mut s = spec("http://unused", TokenRequestStyle::Form);
    s.login_hint_param = None;
    let url = authorize_url(
        &s,
        "c",
        "s",
        "http://localhost:5/callback",
        Some("a@example.com"),
    )
    .unwrap();
    assert!(!url.contains("example.com&") && !url.contains("login_hint"));
}

#[test]
fn parse_callback_accepts_only_our_path_and_state() {
    assert_eq!(
        parse_callback("/callback?code=abc&state=S", "/callback", "S"),
        Callback::Code("abc".into())
    );
    assert_eq!(
        parse_callback("/callback?code=abc&state=OTHER", "/callback", "S"),
        Callback::Ignore
    );
    assert_eq!(
        parse_callback("/favicon.ico", "/callback", "S"),
        Callback::Ignore
    );
    assert_eq!(
        parse_callback("/callback?state=S", "/callback", "S"),
        Callback::Ignore
    );
    assert_eq!(
        parse_callback("/callback?error=access_denied&state=S", "/callback", "S"),
        Callback::Denied("access_denied".into())
    );
    assert_eq!(
        parse_callback(
            "/callback?error=access_denied&error_description=User%20said%20no&state=S",
            "/callback",
            "S"
        ),
        Callback::Denied("User said no".into())
    );
}

fn get(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    let mut s = std::net::TcpStream::connect(("127.0.0.1", u.port().unwrap())).unwrap();
    let target = match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    };
    write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn loopback_serves_the_redirect_and_returns_the_code() {
    let s = spec("http://unused", TokenRequestStyle::Form);
    let lb = Loopback::bind(&s).unwrap();
    assert!(lb.redirect_uri.starts_with("http://localhost:"));
    assert!(lb.redirect_uri.ends_with("/callback"));
    let base = lb.redirect_uri.clone();
    let client = std::thread::spawn(move || {
        let stray = get(&base.replace("/callback", "/favicon.ico"));
        let done = get(&format!("{base}?code=the-code&state=S1"));
        (stray, done)
    });
    let code = lb
        .wait_for_code("S1", Duration::from_secs(10), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(code, "the-code");
    let (stray, done) = client.join().unwrap();
    assert!(stray.starts_with("HTTP/1.1 404"));
    assert!(done.starts_with("HTTP/1.1 200") && done.contains("Signed in"));
}

#[test]
fn loopback_reports_a_denied_sign_in() {
    let lb = Loopback::bind(&spec("http://unused", TokenRequestStyle::Form)).unwrap();
    let url = format!("{}?error=access_denied&state=S2", lb.redirect_uri);
    let client = std::thread::spawn(move || get(&url));
    let err = lb
        .wait_for_code("S2", Duration::from_secs(10), &AtomicBool::new(false))
        .unwrap_err();
    assert!(format!("{err}").contains("access_denied"));
    client.join().unwrap();
}

#[test]
fn loopback_stops_when_the_window_is_closed() {
    let lb = Loopback::bind(&spec("http://unused", TokenRequestStyle::Form)).unwrap();
    let start = Instant::now();
    let err = lb
        .wait_for_code("S", Duration::from_secs(30), &AtomicBool::new(true))
        .unwrap_err();
    assert!(format!("{err}").contains("closed"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

/// One-shot token endpoint: returns `body` and hands back the raw request.
fn token_server(
    status: u16,
    body: &'static str,
) -> (&'static str, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url: &'static str = Box::leak(
        format!("http://{}/v1/oauth/token", listener.local_addr().unwrap()).into_boxed_str(),
    );
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut head = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let mut req_body = vec![0u8; len];
        reader.read_exact(&mut req_body).unwrap();
        let mut s = &stream;
        write!(
            s,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        head + &String::from_utf8(req_body).unwrap()
    });
    (url, handle)
}

#[test]
fn exchange_posts_json_with_verifier_and_state() {
    let (url, req) = token_server(
        200,
        r#"{"access_token":"at","refresh_token":"rt","expires_in":60}"#,
    );
    let s = spec(
        url,
        TokenRequestStyle::Json {
            include_state: true,
        },
    );
    let tok = exchange(
        &s,
        "the-code",
        "the-verifier",
        "S9",
        "http://localhost:7/callback",
    )
    .unwrap();
    assert_eq!(tok["access_token"], "at");
    let req = req.join().unwrap();
    assert!(req
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    let body: serde_json::Value =
        serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "the-code");
    assert_eq!(body["code_verifier"], "the-verifier");
    assert_eq!(body["redirect_uri"], "http://localhost:7/callback");
    assert_eq!(body["client_id"], "client-123");
    assert_eq!(body["state"], "S9");
}

#[test]
fn exchange_can_post_a_form() {
    let (url, req) = token_server(200, r#"{"access_token":"at"}"#);
    let s = spec(url, TokenRequestStyle::Form);
    exchange(&s, "c0de", "v", "S", "http://localhost:7/callback").unwrap();
    let req = req.join().unwrap();
    assert!(req
        .to_ascii_lowercase()
        .contains("application/x-www-form-urlencoded"));
    let body = req.split("\r\n\r\n").nth(1).unwrap();
    assert!(body.contains("grant_type=authorization_code") && body.contains("code_verifier=v"));
    assert!(!body.contains("state="), "form style doesn't echo state");
}

#[test]
fn exchange_reports_a_rejected_code_without_its_body() {
    let (url, req) = token_server(400, r#"{"error":"invalid_grant","secret":"leak"}"#);
    let s = spec(
        url,
        TokenRequestStyle::Json {
            include_state: true,
        },
    );
    let err = format!(
        "{:#}",
        exchange(&s, "c", "v", "S", "http://localhost:7/callback").unwrap_err()
    );
    assert!(err.contains("400") && !err.contains("leak"), "{err}");
    req.join().unwrap();
}

#[test]
fn a_sign_in_as_someone_else_is_refused() {
    assert!(check_signed_in_as("dev@example.com", Some("DEV@example.com")).is_ok());
    assert!(check_signed_in_as("new@example.com", None).is_ok());
    let err = check_signed_in_as("other@example.com", Some("dev@example.com")).unwrap_err();
    assert!(format!("{err}").contains("nothing was changed"));
}

#[test]
fn each_account_gets_its_own_cookie_store() {
    assert_eq!(
        store_id("claude", "Dev@Example.com"),
        store_id("claude", "dev@example.com")
    );
    assert_ne!(
        store_id("claude", "dev@example.com"),
        store_id("claude", "dev2@example.com")
    );
    assert_ne!(
        store_id("claude", "dev@example.com"),
        store_id("codex", "dev@example.com")
    );
}

#[test]
fn expiry_warning_fires_once_per_expiry_inside_the_window() {
    let day = 86_400_000;
    let now = 100 * day;
    assert!(!should_warn_expiry(None, now, None));
    assert!(
        !should_warn_expiry(Some(now + 4 * day), now, None),
        "outside the window"
    );
    assert!(should_warn_expiry(Some(now + 2 * day), now, None));
    assert!(
        !should_warn_expiry(Some(now + 2 * day), now, Some(now + 2 * day)),
        "already warned"
    );
    assert!(
        should_warn_expiry(Some(now + 2 * day), now, Some(now - 30 * day)),
        "a newer expiry"
    );
    assert!(
        !should_warn_expiry(Some(now - 1), now, None),
        "already dead"
    );
}
