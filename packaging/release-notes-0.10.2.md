# usagio 0.10.2

### Added
- **Add an account by signing in from usagio.** **Sign in to a new Claude
  account…** (above "Capture current login") opens usagio's own sign-in
  window on a blank session; sign in as the new account (email code, then
  Authorize) and it's added — without switching to it, and without logging
  out of `claude` first. Its sign-in is remembered, so later renewals are one
  click. From a terminal: `usagio login` (uses your browser). Capturing a
  `claude` login still works as before.
