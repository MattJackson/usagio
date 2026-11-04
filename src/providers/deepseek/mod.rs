//! DeepSeek provider — API-KEY STUB.
//!
//! Registered, but hidden from the "Paste API key" submenu (stubs report
//! `supports_usage: false`, which `partition_capture_providers` drops) until
//! the real `capture_api_key` flow lands. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`. Account identifier is
//!    `<nickname>-<last-4-of-key>` — DeepSeek has no public account-info
//!    endpoint that returns an email or label, so the user's nickname
//!    plus a stable key-suffix disambiguator is the best we can do.
//!  - Usage: DeepSeek's balance / usage endpoint wiring lands in a later
//!    phase.

#![allow(dead_code)]

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub fn new() -> Box<dyn Provider> {
    Box::new(DeepSeekProvider)
}

pub struct DeepSeekProvider;

// Not wired yet (no capability exposed):
// TODO: derive account_id = `<nickname>-<last-4-of-key>`, persist
// the key to the keychain under `deepseek/<account_id>`.
impl Provider for DeepSeekProvider {
    fn provider_id(&self) -> &'static str {
        "deepseek"
    }

    fn display_name(&self) -> &'static str {
        "DeepSeek"
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
        let p = DeepSeekProvider;
        assert_eq!(p.provider_id(), "deepseek");
        assert_eq!(p.display_name(), "DeepSeek");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = DeepSeekProvider.capabilities();
        assert!(!caps.supports_usage); // stub: hidden until capture is wired
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = DeepSeekProvider;
        assert!(p.login_capture().is_none());
        assert!(p.api_key_capture().is_none());
    }
}
