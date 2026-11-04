//! OpenRouter provider — API-KEY STUB.
//!
//! Registered, but hidden from the "Paste API key" submenu (stubs report
//! `supports_usage: false`, which `partition_capture_providers` drops) until
//! the real `capture_api_key` flow lands. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey` — user pastes an OpenRouter key + optional
//!    label. Later commits will call `GET /api/v1/key` to read the
//!    user-set label and key `hash`, which becomes the account identifier
//!    (labels are stable, keys can rotate).
//!  - Usage: OpenRouter exposes credit / rate-limit info on the same
//!    `/api/v1/key` endpoint; wiring lands in a later phase.
//!  - Switching is not planned (the CLI reads `OPENROUTER_API_KEY`; the
//!    menu will only rewrite the env / config, not multi-slot swap).

#![allow(dead_code)]

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub fn new() -> Box<dyn Provider> {
    Box::new(OpenRouterProvider)
}

pub struct OpenRouterProvider;

// Not wired yet (no capability exposed):
// API-key providers don't participate in the "read the vendor CLI's
// on-disk credentials" flow — they onboard through capture_api_key.
// TODO: call GET /api/v1/key with the pasted key, use the returned
// `label` as the account identifier (falling back to the nickname
// when the key has no label set), and persist the key bytes to the
// keychain under `openrouter/<label>`.
impl Provider for OpenRouterProvider {
    fn provider_id(&self) -> &'static str {
        "openrouter"
    }

    fn display_name(&self) -> &'static str {
        "OpenRouter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // STUB: no capture/usage path wired yet — false hides it from the
            // onboarding menu (partition_capture_providers) until it is real.
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: false,
            secret_backend: SecretBackend::Keychain,
            capture_mode: CaptureMode::ApiKey,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_locked() {
        let p = OpenRouterProvider;
        assert_eq!(p.provider_id(), "openrouter");
        assert_eq!(p.display_name(), "OpenRouter");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = OpenRouterProvider.capabilities();
        assert!(!caps.supports_usage); // stub: hidden until capture is wired
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = OpenRouterProvider;
        assert!(p.login_capture().is_none());
        assert!(p.api_key_capture().is_none());
    }
}
