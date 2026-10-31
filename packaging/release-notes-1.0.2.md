# usagio 1.0.2

### Fixed

- Saved credentials are matched to their account before adoption. A stale active-account marker can no longer overwrite another account's saved login.
- Claude and Codex share refresh serialization and persistence rules. Complete rotated credentials are saved together, active-client grants are left to the vendor, and lock failures defer refresh instead of risking token reuse.
- Delayed Codex usage and plan responses cannot overwrite a newer login or usage observation.
- Codex switches drain the shared daemon before changing credentials, then restart it and verify the selected account. Pending handoffs recover across usagio restarts and preserve autoswap protection against immediately returning to an exhausted account.
- Active Codex login renewals wait separately from the old credentials until handoff. Selecting the already-loaded account preserves running work.

- Token diagnostics use one-way fingerprints instead of logging token prefixes.

### Added

- Codex browser login and renewal through the shared sign-in flow.
- Provider-qualified selectors: `codex:email` and `claude:email`.
- Codex `start` and `continue`; continue uses `codex resume --last`. Launch waits for a queued handoff to finish.

### Compatibility

Codex switching supports its default file credential store and the shared daemon control protocol tested with Codex 0.166.0. Other configured credential stores are rejected before draining or writing. Sessions owned by a separate embedded app-server cannot be transferred by restarting the shared daemon. Previously overwritten credentials require renewing that account's login; this release prevents further cross-account overwrites but cannot reconstruct lost tokens.
