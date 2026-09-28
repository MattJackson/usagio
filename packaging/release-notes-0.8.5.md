# usagio 0.8.5

### Fixed
- A rate-limited (HTTP 429) or failing inactive account no longer slows
  polling for every account. Only the active account's 429 backs off the poll
  loop; an inactive account that fails now backs off on its own. Previously
  one idle account's 429s pushed polling to every 20 minutes, so the active
  account could run out between checks without swapping.
- Inactive accounts are checked at most every 10 minutes instead of speeding
  up as they near the trigger — only the active account is burning usage, so
  the request budget now goes to it first, and a 429 on the active account
  pauses inactive checks for that cycle.
- Switching accounts (from the menu, an auto-swap, or a new login) now
  refreshes the new active account right away instead of waiting out the
  previous account's backoff.
- `usagio list` now orders accounts exactly like the menu bar: locked accounts
  sink below usable ones and are ordered by when they unlock, the active
  account stays in normal rotation, and everything else orders by soonest
  weekly reset.
