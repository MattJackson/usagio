//! kimi (Moonshot AI Kimi CLI) provider — STUB ONLY.
//!
//! Registered, but hidden from the "Capture current login" submenu (stubs
//! report `supports_usage: false`, which `partition_capture_providers`
//! drops) until the real flow lands. Every real operation is deferred.
//! Nothing here reads credentials or hits the network yet.
//!
//! Recon summary (upstream: github.com/MoonshotAI/kimi-cli):
//!  - Creds path: `~/.kimi/` (TOML config) plus a macOS Keychain entry under
//!    service name `kimi-code` for the OAuth refresh material.
//!  - OAuth client_id: `17e5f671-d194-4dfb-9706-5516cb48c098`, IdP host
//!    `api.kimi.com`.
//!  - Single-slot per host; switching is "awkward" (Keychain rewrite plus
//!    TOML rewrite) and not wired yet.
//!  - No dedicated usage/quota endpoint (headers-only quota telemetry).

use crate::providers::trait_def::{Capabilities, CaptureMode, Provider, SecretBackend};

pub(crate) fn new() -> Box<dyn Provider> {
    Box::new(KimiProvider)
}

struct KimiProvider;

// Not wired yet (no capability exposed):
// TODO: read `~/.kimi/` TOML plus the macOS Keychain entry under
// service `kimi-code`, decode the OAuth refresh material, and emit a
// single CapturedAccount for the currently-active slot.
impl Provider for KimiProvider {
    fn provider_id(&self) -> &'static str {
        "kimi"
    }

    fn display_name(&self) -> &'static str {
        "Kimi"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_usage: false,
            supports_switching: false,
            supports_launch: false,
            supports_remove: true,
            supports_email_capture: false,
            secret_backend: SecretBackend::File,
            capture_mode: CaptureMode::CredsOnDisk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_locked() {
        let p = KimiProvider;
        assert_eq!(p.provider_id(), "kimi");
        assert_eq!(p.display_name(), "Kimi");
    }

    #[test]
    fn capabilities_are_locked() {
        let caps = KimiProvider.capabilities();
        assert!(!caps.supports_usage);
        assert!(!caps.supports_switching);
        assert!(!caps.supports_email_capture);
        assert_eq!(caps.secret_backend, SecretBackend::File);
        assert_eq!(caps.capture_mode, CaptureMode::CredsOnDisk);
    }

    #[test]
    fn capture_returns_none_placeholder() {
        assert!(KimiProvider.login_capture().is_none());
    }
}
