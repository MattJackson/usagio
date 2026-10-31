# usagio 1.0.4

- Remove the Classic auto-swap engine, selector, and environment override. Existing saved Classic settings are ignored; all providers use the current engine.
- At risk focuses on accounts approaching exhaustion and excludes already-locked accounts. Explicit provider pins retain lock countdowns.
- Sign in to a new account now offers a provider submenu matching Capture current login.

Includes regression coverage for old saved settings, locked Claude versus usable Codex tray selection, and sign-in submenu contents.
