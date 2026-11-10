# usagio 1.1.3

- Reset rows say what each reset is for: "Session reset ready", "Weekly reset ready", or "Reset saved · won't clear weekly". Identical resets merge into one row with a count.
- A reset only counts as ready (clickable, 🎁 on the account row and menu-bar title) when claiming it would clear a limit that has usage on it. Before, a weekly-capped account could spend its reset on an empty allowance and get nothing back.
- The claim dialog says what will be cleared, that the weekly limit is not reset (when it isn't), and how many resets are left.
- Every claim result is logged, and the claimed account is re-checked on the next poll instead of keeping its stale capped reading.
- Tidier account submenus: "Credits · 0" is hidden, the weekly forecast no longer repeats the % ("Weekly runs out in ~6h 5m"), and reset expiry shows only when it's under 14 days away.
