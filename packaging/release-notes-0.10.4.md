# usagio 0.10.4

### Fixed
- **A manual switch really sticks until the trigger.** A usage-check 429
  ("too many requests") near the trigger no longer moves you off an account
  you picked by hand; only a real reading at the trigger does. Before, a
  pick at 93% (95% trigger) flipped away on the first 429, and again a
  second after picking it back.
- **A manual switch to an account already at the trigger holds even if its
  last reading was stale.** Whether a pick holds until exhausted is now
  settled by the first reading after the switch, not a cached one that may
  be minutes old, so a pick read at 93% that was really at 96% no longer
  auto-swaps away.
