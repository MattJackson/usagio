# usagio 0.8.7

### Fixed
- **No more running out after a 429 near the limit.** One HTTP 429 from the
  usage endpoint on the active account at 93% (trigger 95%) used to stretch the
  poll interval from 30s to 6 minutes and wait for a second 429 before
  swapping, so the account ran out unobserved. Now a 429 on the active account
  within 5 points of the trigger counts as reaching it and swaps immediately,
  and a 429 backoff doubles the current interval (30s → 60s) instead of
  jumping to twice the base interval. A switch also restarts the backoff from
  30s rather than from the base interval.
- **Leaving an account at its limit is never delayed.** The 5-minute swap
  cooldown now only applies to optional swaps (flip-backs and all-blocked
  preparation). It used to hold you on a just-landed account even after it
  hit the trigger.
- **Swap targets use the full allowance up to your trigger.** An account is a
  target while its session and weekly are both under your trigger, instead of
  a fixed 85% session cap that ignored the trigger you set (it stranded the
  last ~10% of every session, and with a trigger under 85% it could pick an
  account already past it).
- **Swap targets must have a current reading.** A target last read more than
  two minutes ago is re-checked right before the swap and skipped if it's now
  at the trigger. If the re-check fails, an urgent swap only uses it when its
  last reading had at least 10 points of room. An account usagio moved you off
  isn't returned to until a reading taken after leaving it says it has room,
  and accounts excluded only for stale data are re-checked when the active
  account needs to be left, even while it's being rate-limited.
- The active account is polled even if it was flagged for re-login, so its
  reading (and auto-swap) no longer freezes after you switch to it manually.
- Account details no longer make a healthy limit look locked. A locked window
  reads "Session locked · unlocks in 28m" in red (matching the red countdown
  on the account row); a healthy one reads "Weekly renews in 6d 22h". Detail
  lines are plain text and no longer highlight on hover.
- Every provider's header in the menu is now the same plain label row with
  its icon. A provider used to turn into a dropdown (and lose its icon)
  whenever its active account was blocked or an environment override was set;
  the env-override notice now shows inline in the header instead.

### Removed
- Dollar cost estimates: the "~$X this cycle (est)" menu row,
  `usagio report --verdict` and `usagio report --pricing`. No provider reports
  what a subscription's usage costs, so these multiplied the usage percentage
  by a guessed token cap and API list prices — numbers that looked precise but
  weren't.
