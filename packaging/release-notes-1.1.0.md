# usagio 1.1.0

Reliability release from a full three-round code audit (about 400 fixes).

- Account switching: renewed logins are never lost on switch, removed accounts can't become active, concurrent CLI/menu switches no longer overwrite each other, and macOS honours `CLAUDE_CONFIG_DIR`'s per-directory keychain item.
- Auto-swap: ranks ready accounts by earliest weekly reset, respects manual holds while auto-swap is off, keeps the no-bounce-back latch on the right window, and treats future-stamped readings as stale.
- Menu: refresh errors and failed refreshes are shown, expired logins are labelled expired, sign-in flows no longer leak or cancel each other.
- Storage and history: crash- and clock-safe backups and pruning, malformed entries preserved instead of dropped.
- Notifications: reset and pace alerts fire once, at the right time.
