# usagio 1.1.2

- Fixes the menu-bar app crashing about a second after launch on macOS 27 (introduced in 1.1.1). WebKit is now initialised before the startup clean-up of old sign-in sessions.
- CI now launches the real menu-bar app on macOS and fails if it exits, so a startup crash can't ship again.
