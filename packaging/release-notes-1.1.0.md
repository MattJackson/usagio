# usagio 1.1.0

### Changed
- **Codex locks come from Codex itself.** usagio used to treat an account as
  locked once a window reached 99.5%. For Codex it now uses OpenAI's own flag:
  a limited account reads locked even below 100%, and one Codex still allows
  isn't treated as locked (it shows 99%), so auto-swap no longer leaves it
  early. Claude still uses the percentage; its API has no equivalent flag yet.

### Added
- **Codex limit resets show in the menu.** When Codex banks a rate-limit reset
  for an account, its submenu shows "🎁 Reset available" (or "banked" until it
  would apply). Claiming stays in Codex: usagio doesn't know Codex's claim
  endpoint, so there's no Claim button and no notification yet.
- **usagio logs API fields that start carrying data.** A few usage fields from
  Anthropic and OpenAI are always empty today (Claude's `locked_reason` and
  dollar limits, Codex's `spend_control` and extra rate limits). The first time
  one comes back with a value, `usagio.log` gets an `api-watch:` line, so we
  can decide from real data whether to use it.
