# usagio 0.9.0

### Added
- **Account details show what the provider itself reports beyond your
  limits**, and only when there's something to show:
  - Credits / extra-usage spend, e.g. "Credits · $12.34 used of $50.00",
    "Credits · 120 left" (Codex credits), or "Credits · limit reached".
  - Where this week's usage went, when it's split across more than one
    product, e.g. "This week · Claude Code 82% · Chats 18%".
  - Per-model weekly limits once they're in use, e.g. "Fable 7d · 42% ·
    resets in 2d 3h".

  These are the provider's own numbers, never estimates. A provider that
  changes or drops these fields never breaks the usage reading itself.
