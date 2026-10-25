# usagio 0.12.1

### Fixed
- **Auto-swap gets the next account ready again after a manual switch.** If
  you switched by hand to an account that was already at its limit, usagio
  kept treating it as "your pick" even after its limit reset and it was used
  up again. When every account was then out of room, it stayed put instead of
  moving to the account that resets soonest, so the swap only happened once
  that account's reset had already passed. The hold on a hand-picked account
  now ends once its limit resets.
