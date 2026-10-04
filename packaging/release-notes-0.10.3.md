# usagio 0.10.3

### Fixed
- **A manual switch sticks until the trigger.** Switching by hand to an
  account under the trigger no longer flips back to another account (e.g.
  one with a sooner weekly reset) five minutes later; it stays until it
  reaches the trigger, then auto-swaps as usual. A switch to an account
  already at/over the trigger still stays until it's exhausted.
