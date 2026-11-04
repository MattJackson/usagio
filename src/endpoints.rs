//! Outbound HTTPS endpoints, resolved through one seam.
//!
//! Production always calls the built-in URL: nothing (no env var, no config)
//! can redirect a credential-bearing request. A unit test points an endpoint
//! at an in-process mock server for its own thread with [`set_test_override`].
//! Without an override a test falls back to the built-in URL only through
//! [`url`]; [`mocked_url`] gives it nothing, so the caller makes no request.

/// An endpoint usagio calls.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Endpoint {
    /// Claude's `/api/oauth/usage`.
    ClaudeUsage,
    /// Claude's OAuth token endpoint (refresh grant).
    ClaudeToken,
    /// Claude's `/api/oauth/profile`.
    ClaudeProfile,
    /// Codex's ChatGPT-backend usage endpoint.
    CodexUsage,
}

impl Endpoint {
    fn built_in(self) -> &'static str {
        match self {
            Endpoint::ClaudeUsage => crate::providers::claude::config::USAGE_URL,
            Endpoint::ClaudeToken => crate::providers::claude::config::TOKEN_URL,
            Endpoint::ClaudeProfile => crate::providers::claude::config::PROFILE_URL,
            Endpoint::CodexUsage => crate::providers::codex::USAGE_URL,
        }
    }
}

/// The URL to call: the built-in one, or this test thread's override.
pub(crate) fn url(endpoint: Endpoint) -> String {
    #[cfg(test)]
    if let Some(url) = test_override(endpoint) {
        return url;
    }
    endpoint.built_in().to_string()
}

/// The URL to call for a request a test must never send for real: the
/// built-in one in production, and only this thread's override in a test
/// (`None` = make no request).
pub(crate) fn mocked_url(endpoint: Endpoint) -> Option<String> {
    #[cfg(test)]
    return test_override(endpoint);
    #[cfg(not(test))]
    Some(endpoint.built_in().to_string())
}

#[cfg(test)]
thread_local! {
    static OVERRIDES: std::cell::RefCell<std::collections::HashMap<Endpoint, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
fn test_override(endpoint: Endpoint) -> Option<String> {
    OVERRIDES.with(|o| o.borrow().get(&endpoint).cloned())
}

/// Point `endpoint` at `url` (a mock server) for the current test thread, or
/// back at nothing with `None`.
#[cfg(test)]
pub(crate) fn set_test_override(endpoint: Endpoint, url: Option<&str>) {
    OVERRIDES.with(|o| {
        let mut o = o.borrow_mut();
        match url {
            Some(url) => o.insert(endpoint, url.to_string()),
            None => o.remove(&endpoint),
        };
    });
}
