# usagio 0.8.9

### Fixed
- **A manual switch under your threshold auto-swaps as usual.** In 0.8.8 any
  manual switch held, so an account picked at 79% rode past the threshold and
  sat at 99% without swapping. Now only a switch to an account already at or
  over the threshold holds (you're acking it's over); it still swaps off once
  it is exhausted (100%).
- **A lapsed account's menu is one line.** An account with no subscription
  now shows just `No subscription · Free plan` instead of two extra lines
  about paused checks.

### Tests
- About 300 new tests close gaps found by a full mutation-testing sweep
  (2,871 mutants): the auto-swap decision core, usage refresh policy,
  credentials, state backups, history logs, burn-rate forecasts and the menu.
  No behaviour changes beyond the fix above.
