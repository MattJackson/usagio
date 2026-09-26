# usagio 0.8.2

### Fixed
- Manual account switching now allows quota-blocked accounts.
- When every usable login is quota-blocked, auto-switch prepares the account
  whose exhausted limits clear first. The lock/countdown remains until a
  successful usage refresh confirms capacity.
- All-blocked preparation preserves manual selections and retains the
  existing cooldown and no-return protections. The prepared account appears
  first through the existing active-account ordering.
