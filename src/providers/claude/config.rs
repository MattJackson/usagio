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
