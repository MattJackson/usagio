# usagio 1.0.8

- Fix locked-account ordering: the earliest unlock comes first, including the active account. Menu and CLI share one display comparator.
- Fix auto-swap staying on a manually selected account after its usage is blocked. When all accounts are blocked, Usagio prepares the account that unlocks first, subject to the existing cooldown and evidence checks.
- Preserve manual holds before a confirmed lock and keep spendable credits ahead of preparing a blocked account.
