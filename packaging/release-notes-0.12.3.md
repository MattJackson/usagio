# usagio 0.12.3

### Fixed
- **The menu bar shows credits left when the active account runs on
  credits.** Once an account's plan limit was used up but usage credits were
  still paying for requests, its row in the menu showed the credits left, but
  the menu bar still showed a lock and the countdown to the reset. The menu
  bar now shows the same credits figure (e.g. "$627 credits"), and goes back
  to the countdown once the credits run out or hit their cap.
