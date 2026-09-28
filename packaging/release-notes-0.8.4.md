# usagio 0.8.4

### Fixed
- Locked accounts are now ordered by when they next become usable. They still
  sink below usable accounts, but among themselves they were ordered by weekly
  reset, so an account that was only session-locked (usable again in a couple
  of hours) could sit below accounts locked for days. Usable accounts still
  order by soonest weekly reset.
