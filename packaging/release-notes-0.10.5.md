# usagio 0.10.5

### Changed
- **An auto-swap uses up the account that resets soonest, even if it's
  close to the trigger.** When the active account has to be left, an account
  within 5 points of the trigger (e.g. 94% with a 95% trigger) is now a
  target if its weekly window resets soonest, instead of being skipped for a
  roomier account that resets later. Its last points no longer expire unused.
  Such an account is still never a proactive flip-back target.
