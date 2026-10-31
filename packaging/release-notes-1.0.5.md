# usagio 1.0.5

- Show 🎁 in the menu bar when any account has a reset and beside each account with one.
- Use short reset labels: “🎁 1 reset banked” and “🎁 1 reset ready”. Claude ready resets open an account-specific confirmation before claiming; Codex resets remain informational.
- Show usable credits as a single amount, or 0 when unavailable. Keep explicit zero-credit rows for both Claude and Codex; omit past spending and spending caps from the menu.
- Keep weekly forecasts consistent when the rate is unknown, shorten forecasts to “Weekly · 44% · ~3h 37m”, and remove the redundant Active submenu row.

Includes regression coverage for reset discovery, claim confirmation and cancellation, available credit amounts, explicit zero balances, and forecast consistency.
