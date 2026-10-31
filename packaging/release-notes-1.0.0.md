# usagio 1.0.0

### Added
- **Your plan shows under Anthropic's own name.** Claude accounts on the new
  Ultra plan show "Plan · Ultra" instead of "Max 20x". The name comes from
  Anthropic, so future renames show up without an update.

### Changed
- **Accounts with only a weekly limit show one number.** Claude Ultra and
  Codex have no 5-hour session window, so their row shows "66%" instead of
  "— / 66%". The tooltip and submenu no longer mention the missing session.
- **Shorter weekly forecast row.** It reads "Weekly · 66% · empty in ~3h" or
  "Weekly · 66% · lasts to reset", and fits the submenu.
- **Cleaner credits row.** "Credits · $12.34 / $50 used", and once the cap is
  hit "Credits · $1,000 limit reached". Codex credit balances show two
  decimals ("636.49" rather than "636.4935245000").

### Fixed
- **The weekly forecast no longer calls a window safe when it runs out first.**
  An account set to run out in under an hour, with its reset days away, read
  "empty in ~59m · safe for this cycle". It now reads "empty in ~59m".
