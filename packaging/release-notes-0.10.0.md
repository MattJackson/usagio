# usagio 0.10.0

### Added
- **Renew a login from usagio.** When an account's login is about to expire,
  its row shows `renew · 2d` and you get one notification; an expired one
  shows `⚠ renew`. **Renew login…** opens usagio's own small sign-in window
  for that account, which keeps that account's claude.ai session separate
  from your browser, so it's usually one **Authorize** click. The new login
  is saved for that account, never changes which account is active, and goes
  live immediately if the account is the active one. From a terminal:
  `usagio renew <email>` (opens your browser). Built as one login engine for
  every provider; Claude is the first.

### Fixed
- **No more false "login expires in N days" in Claude.** usagio now records
  the refresh token's real expiry on every refresh, so the login Claude Code
  sees after a switch carries the right date.
- **No more false "re-login required".** Two refreshes of the same account
  could race and the loser flagged a healthy account as needing a re-login.
  Refreshes are now serialized and the second one reuses the first's result.
- **A Free account shows "Free"**, not a re-login warning.
