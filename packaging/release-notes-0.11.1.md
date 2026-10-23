# usagio 0.11.1

### Fixed
- **Credits now show your real usage-credit balance.** A Claude account with
  usage credits turned on showed only the monthly spend cap (e.g.
  *"Credits · $0.00 used of $100.00"*), never the money in the wallet:
  Anthropic's usage endpoint reports the balance as empty. usagio now reads it
  from the same place Claude Code does, and the row leads with it:
  **Credits · $903.35 left · $54.63 used of $100.00**. If a balance read
  fails, the last known balance stays rather than disappearing.
- The fallback spend figure (used when the usage response carries no `spend`
  block) now parses the decimal value Anthropic actually sends.
