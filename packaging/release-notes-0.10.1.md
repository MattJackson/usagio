# usagio 0.10.1

### Fixed
- **Paste works in the sign-in window.** ⌘V (and ⌘C/⌘X/⌘A/⌘Z) did nothing
  in "Renew login…", so the emailed code had to be typed by hand.

### Removed
- **"Launch client" in the account menu.** It never opened anything from the
  menu bar. `usagio start` / `usagio continue` still launch `claude` from a
  terminal.
