//! Fireworks AI provider — API-KEY STUB.
//!
//! Registered, but hidden from the "Paste API key" submenu (stubs report
//! `supports_usage: false`, which `partition_capture_providers` drops) until
//! the real `capture_api_key` flow lands. Nothing here reads credentials or
//! hits the network yet.
//!
//! Onboarding plan (locked, see workflow context):
//!  - Capture mode: `ApiKey`.
//!  - Account identifier: **email** — Fireworks exposes
//!    `GET /v1/accounts` which returns the account owner's email; the
//!    real capture flow calls it once, stores the email in
//!    `IdentitySnapshot`, and keys the keychain entry off it. The
//!    user-supplied nickname is preserved as `display_name` for the row
//!    label but not used as the identifier.
//!  - Usage: `/v1/accounts/<id>/usage` wiring lands in a later phase.

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub(crate) fn new() -> Box<dyn Provider> {
    Box::new(FireworksProvider)
}

struct FireworksProvider;

// Not wired yet (no capability exposed):
// TODO: call GET /v1/accounts with the pasted key, use the returned
// account owner email as the identifier, persist the key to the
// keychain under `fireworks/<email>`.
impl Provider for FireworksProvider {
    fn provider_id(&self) -> &'static str {
        "fireworks"
    }

    fn display_name(&self) -> &'static str {
        "Fireworks"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // STUB: no capture/usage path wired yet — false hides it from the
            // onboarding menu (partition_capture_providers) until it is real.
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: true,
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
        let p = FireworksProvider;
        assert_eq!(p.provider_id(), "fireworks");
        assert_eq!(p.display_name(), "Fireworks");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = FireworksProvider.capabilities();
        assert!(!caps.supports_usage); // stub: hidden until capture is wired
        assert!(!caps.supports_switching);
        // Fireworks is the sole API-key provider that keys accounts by email
        // (via GET /v1/accounts) — the flag must stay on so the menu wires
        // the email-badge decoration.
        assert!(caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::Keychain);
        assert_eq!(caps.capture_mode, CaptureMode::ApiKey);
    }

    #[test]
    fn capture_paths_return_unsupported() {
        let p = FireworksProvider;
        assert!(p.login_capture().is_none());
        assert!(p.api_key_capture().is_none());
    }
}
