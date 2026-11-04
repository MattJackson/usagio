//! Synthetic (synthetic.new) provider — API-KEY STUB.
//!
//! Registered, but hidden from the "Paste API key" submenu (stubs report
//! `supports_usage: false`, which `partition_capture_providers` drops) until
//! the real `capture_api_key` flow lands. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`. Account identifier is
//!    `<nickname>-<last-4-of-key>` (no account-info endpoint exposes an
//!    email or label).
//!  - Usage: wiring lands in a later phase.

#![allow(dead_code)]

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub fn new() -> Box<dyn Provider> {
    Box::new(SyntheticProvider)
}

pub struct SyntheticProvider;

// Not wired yet (no capability exposed):
// TODO: derive account_id = `<nickname>-<last-4-of-key>`, persist
// the key to the keychain under `synthetic/<account_id>`.
impl Provider for SyntheticProvider {
    fn provider_id(&self) -> &'static str {
        "synthetic"
    }

    fn display_name(&self) -> &'static str {
        "Synthetic"
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
        let p = SyntheticProvider;
        assert_eq!(p.provider_id(), "synthetic");
        assert_eq!(p.display_name(), "Synthetic");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = SyntheticProvider.capabilities();
        assert!(!caps.supports_usage); // stub: hidden until capture is wired
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = SyntheticProvider;
        assert!(p.login_capture().is_none());
        assert!(p.api_key_capture().is_none());
    }
}
