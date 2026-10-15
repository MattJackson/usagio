//! OAuth + API constants for Claude Code's public OAuth client.
//! These values are the ones the official Claude Code CLI uses for
//! subscription (Pro/Max) authentication; they are not secret.

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Endpoint the `/usage` command hits. Returns limits + reset times.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Profile endpoint, used to label an account with its email/name.
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// OAuth token endpoint (refresh_token grant).
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Beta header required for OAuth-bearer requests.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// `User-Agent` sent on OAuth-bearer requests, matching what the Claude Code
/// CLI sends. The `cedar_ember` reset-grant block on `/usage` is gated on a
/// *current* CLI version: a stale (or absent) user-agent comes back
/// `eligible: false, ineligible_reason: "cli_version"`. The base usage numbers
/// are returned regardless — only the reset-grant eligibility depends on this,
/// so a drift here degrades to "no reset offered", never to broken usage.
/// Bump this when the gate moves the floor forward.
pub const CLI_USER_AGENT: &str = "claude-cli/2.1.300 (external, cli)";

/// Reset-grant program key used both to request the `cedar_ember` block on
/// `/usage` (`?cedar_ember=1`) and as the `program` field when claiming.
pub const RESET_PROGRAM: &str = "cedar_ember";

/// Claim endpoint for a banked usage-limit reset. `{org}` is the account's
/// organization uuid (from the profile endpoint). POST `{program, grant_id,
/// request_id}`; outcomes include `reset`, `already_used`, `not_limited`,
/// `cooldown`, `ineligible`, `unavailable`.
pub fn reset_claim_url(org_uuid: &str) -> String {
    format!("https://api.anthropic.com/api/organizations/{org_uuid}/reset_rate_limits")
}

/// claude.ai login page for Claude Code's OAuth client (subscription accounts).
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// Scopes Claude Code requests on a claude.ai login — what its keychain
/// credential carries, so a usagio-made login is indistinguishable.
pub const LOGIN_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
    "user:plugins",
];
