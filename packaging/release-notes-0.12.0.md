# usagio 0.12.0

### Added
- **Auto-swap can fall back to usage credits.** New setting: **Settings ▸
  Auto-swap ▸ Use credits when every plan is used up** (off by default — it
  spends real money). When it's on and every account's plan usage is used
  up, usagio switches to the account with the most usage credits left to
  spend instead of leaving you stuck. Plan usage always comes first: an
  account with plan room left is picked over any credits, and usagio moves
  off credits as soon as a plan account frees up.
- **An account running on credits shows what's left to spend.** A locked
  account that can still work on usage credits now shows e.g.
  **$627 credits** in the menu instead of a red unlock countdown, and its
  submenu reads *Weekly locked · unlocks in 4d 4h · using credits*. "Left to
  spend" is the smaller of your wallet balance and what's left under your
  monthly cap.

### Changed
- While you're working on credits, usagio no longer switches you to another
  locked account just to be ready for its reset, and no longer sends the
  "no better target" notification.
- Money amounts use thousands separators ($1,000.00).
