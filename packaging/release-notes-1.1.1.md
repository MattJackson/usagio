# usagio 1.1.1

Closes every remaining item from the 1.1.0 audit.

- Codex: accounts keyed by email + workspace (no more collisions between workspaces sharing an email; existing accounts migrate automatically), correct usage header and token expiry, login port fallback.
- Sign-in: a new sign-in replaces an open window instead of failing; leftover web sessions are cleaned up; IPv6-safe redirects.
- Reliability: safer re-login flagging, process-tree cleanup, locale-safe daemon checks, bounded file watching, no Windows busy-polling.
- Correct decimal places for non-USD prepaid balances; fewer network and disk writes per poll.
- `usagio context` matches each tool's real config rules.
- Security updates: rustls, glib (backported fix), website dependencies.
- Internal restructuring for maintainability; no behaviour change.
