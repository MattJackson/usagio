//! Google Vertex AI provider — API-KEY STUB.
//!
//! Registered, but hidden from the "Paste API key" submenu (stubs report
//! `supports_usage: false`, which `partition_capture_providers` drops) until
//! the real `capture_api_key` flow lands. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`. Account identifier is
//!    `<nickname>-<last-4-of-key>` — Vertex AI keys are project-scoped
//!    but the pasted-key flow doesn't do a project-info round-trip, so
//!    the nickname + last-4 disambiguator carries.
//!  - Usage: Cloud Billing / quota wiring lands in a later phase.

#![allow(dead_code)]

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub fn new() -> Box<dyn Provider> {
    Box::new(VertexAiProvider)
}

pub struct VertexAiProvider;

// Not wired yet (no capability exposed):
// TODO: derive account_id = `<nickname>-<last-4-of-key>`, persist
// the key to the keychain under `vertex-ai/<account_id>`.
impl Provider for VertexAiProvider {
    fn provider_id(&self) -> &'static str {
        "vertex-ai"
    }

    fn display_name(&self) -> &'static str {
        "Vertex AI"
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
        let p = VertexAiProvider;
        assert_eq!(p.provider_id(), "vertex-ai");
        assert_eq!(p.display_name(), "Vertex AI");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = VertexAiProvider.capabilities();
        assert!(!caps.supports_usage); // stub: hidden until capture is wired
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = VertexAiProvider;
        assert!(p.login_capture().is_none());
        assert!(p.api_key_capture().is_none());
    }
}
