# usagio 0.11.0

### Added
- **usagio now shows — and lets you claim — a banked usage-limit reset.**
  When Anthropic grants your account a one-shot limit reset (the kind handed
  out with a launch or referral promo), the account's submenu shows a
  **🎁 Reset available · claim below** row, and a **Claim reset…** action
  spends it. A reset that's banked for later (usable only once you're actually
  limited) shows as **🎁 Reset banked · usable once you're limited** with its
  expiry, and no claim button until it can be spent. Claiming is always
  manual — usagio never spends a one-shot reset on its own.
- **A notification the moment a reset lands.** You get a *"🎁 You got a
  usage-limit reset!"* banner the first time a grant appears, and a separate
  *"You're out of usage — claim your banked reset now"* alert when you're
  blocked and a reset is ready to spend. Each fires once per grant.
