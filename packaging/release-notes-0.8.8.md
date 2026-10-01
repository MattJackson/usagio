# usagio 0.8.8

### Fixed
- **Switching to an account yourself sticks.** Picking an account past your
  auto-swap threshold from the menu used to get undone within a second, and
  `usagio switch <acct>` got undone at the next poll. A manual switch (menu or
  CLI) is now recorded in `state.json`, and auto-swap stays on that account
  until it is exhausted or its subscription lapses, or you pick another.
- **Auto-swap no longer thrashes around an account near the trigger.** An
  account within 5 points of the trigger was still a valid swap target, but a
  single 429 there counts as reaching the trigger — so auto-swap kept flipping
  back to it and getting forced out again (about 30 swaps in 3 hours around an
  account at 82–85% of an 85% trigger). Targets must now be more than 5 points
  under the trigger; the active account still rides up to it.
- **No "Where is use_default?" dialog after an upgrade.** Right after `brew
  upgrade`, macOS didn't know the new usagio.app yet, so notification setup
  failed and fell back to a placeholder app. usagio now registers its own
  bundle first.
- **`usagio switch` names the matches for an ambiguous prefix.** `usagio switch
  dev` with several `dev…` accounts said "no account matches" instead of
  listing them.
- **Codex accounts auto-swap too.** Every switchable provider now runs
  through the same swap logic as Claude: swap at the trigger, on a 429 near
  it, fresh-reading checks on targets, cooldown only on optional swaps. Each
  provider has its own cooldown, no-return window and poll backoff, so one
  provider's 429 never slows another's polling.
- **Codex polling follows the same request budget rules as Claude.** The
  active account is fetched on its cadence tier, inactive accounts at most
  every 10 minutes with their own failure backoff, and a 429 on the active
  account pauses that provider's other fetches for the cycle. Previously
  every Codex account was fetched on every wake.
- **No more staying on a full account when everything else is nearly full.**
  If every other account is past the trigger, auto-swap moves to the one with
  the most room left (at least 5 points more than the active one) instead of
  staying put.
- **`usagio watch` behaves exactly like the menu-bar app.** It now runs the
  same loop: it uses the trigger and auto-swap setting from the menu
  (`--trigger` still overrides), wakes right after usage resets, and notices
  the machine waking from sleep. It used to ignore the menu settings, miss
  most resets, and stay blind for up to 20 minutes after a resume.
