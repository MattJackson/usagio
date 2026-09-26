# usagio 0.8.3

### Fixed
- Menu text no longer renders random letters in bold. Regular rows (account
  emails, countdowns, Settings, Capture current login) could draw individual
  glyphs at the bold weight, because the menu renderer (muri) leaked the bold
  font-variation state of a previously drawn glyph into the next one. Fixed
  upstream in muri 0.14.7; usagio now requires it.
