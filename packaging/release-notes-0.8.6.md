# usagio 0.8.6

### Added
- Accounts whose subscription has lapsed are now detected and shown, for every
  provider. A lapsed account's row shows a red "-" instead of a stale reset
  countdown, its submenu says "No subscription · Free plan" instead of old
  usage and cost, "Switch to this account" and "Launch client" are hidden, it
  always sorts to the bottom (alphabetically), and you get a notification when
  a plan lapses or comes back. `usagio list` shows the same.
- The account submenu shows the plan when known (Max 20x, Pro, Plus, …).

### Fixed
- A lapsed account no longer calls the usage endpoint every cycle. Those calls
  only returned 403/429 and spent the request budget the other accounts need.
  It now gets a plan check every 30 minutes (or on `usagio list --refresh`)
  and resumes normal polling as soon as a plan is back.
- Auto-swap never picks a lapsed account, and moves off the active account if
  its plan lapses.
