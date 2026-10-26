# usagio 0.12.2

### Fixed
- **A "too many requests" reply no longer swaps you off a healthy account.**
  When usagio's own usage check got a 429 near the trigger, it treated that
  as "the account is at its limit" and switched — twice in one afternoon,
  both times on readings that were 31 seconds old and well under the
  trigger. A 429 now just means "no new reading": usagio keeps the account,
  and if it stays blind near the trigger for more than two minutes it
  assumes the worst plausible pace and moves to an account it has just
  checked, before the limit is reached.
- **Auto-swap no longer leaves you stuck on an account that's at 100%.** If
  the account you're on is out of room and another account has room,
  usagio switches right away — even if it switched recently, even if the
  other account's last reading is a few minutes old. Before, it could sit on
  a full account for minutes while a roomy one was available, until you
  switched by hand.
- **It picks the account that lasts longest, not the one that resets
  soonest.** Targets are now judged by how many minutes of use they have
  left at your pace, across both the 5-hour and the weekly limit. An account
  3 points from the trigger is no longer chosen over one with room just
  because its weekly reset is a few hours sooner.
- **No more ping-pong near the limit.** An account you were moved off is not
  moved back to in the same limit period, two nearly-full accounts no longer
  trade places, and the last few points are used in an orderly way: usagio
  rides the current account to 99%, moves once to an account with real room
  left, and only then uses up the rest.

### Changed
- **The menu bar shows the limit that runs out first.** The tray percentage
  used to be the 5-hour limit even when the weekly limit was about to run
  out. It now shows whichever limit will block first, with a `w` for weekly:
  **96%w**. The tooltip says which limit binds and when it would run out at
  your current pace. Codex, which has no 5-hour limit, shows its weekly one
  (`1%w`).
- Near the trigger, usagio checks usage once a minute instead of every 30
  seconds, and backs off for at most 2 minutes after a 429 there — the
  30-second checks were what drew the 429s. It never re-checks an account
  that's at 100% until its limit resets.
- Manual switches work as before: an account you pick stays until it reaches
  the trigger (on a real reading), and one you pick at or over the trigger
  stays until it's used up. A hand-picked account that's used up is still
  left for one with room, and never swapped away just to wait for a reset.
- Swap decisions are now written to `decisions.YYYY-MM.ndjson` next to the
  usage history (and the usage history records every check, including
  Codex accounts), so a bad switch can be replayed and explained.

### Rollback
If the new auto-swap misbehaves for you, switch back to the previous one:
**Settings ▸ Auto-swap ▸ Classic engine (before 0.12.2)**. It takes effect on
the next check, no restart needed, and your manual picks carry over. For
`usagio watch` you can also set `USAGIO_AUTOSWAP_ENGINE=v1`. The classic
engine will be removed in the next release.
