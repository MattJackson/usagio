# usagio 1.0.6

- Claude plan names refresh automatically for every saved account, including inactive accounts and accounts whose usage is locked. Ultra now appears under Anthropic’s reported name instead of the legacy Max 20x tier.
- Background checks process two profiles per polling cycle, repeat successful checks after 30 minutes, and retry failures after five minutes. The schedule survives restarts. Manual Refresh checks every eligible profile immediately.
- Profile updates preserve credentials, usage snapshots, and the active login. Delayed responses cannot replace a renewed login or newer account metadata; plan downgrades clear an old display name.
- Login renewal and active-account synchronization refresh saved identity metadata as well as credentials.

Accounts that require sign-in keep their existing status. Lapsed subscriptions continue through the existing renewal checks. Profile checks wait for a valid access token supplied by the existing credential lifecycle.
