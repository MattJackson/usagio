# usagio 1.0.7

- Fix macOS privacy prompts appearing under Usagio's name for tools launched by Codex after an account switch. The shared Codex daemon now starts through an independent, one-shot launchd job.
- Preserve the selected Codex home and verify the selected account after restart. Launcher failures remain visible as pending handoffs; temporary jobs unload after completion or timeout.
- Add signed-app responsibility, argument handling, environment isolation, daemon survival, and timeout cleanup regressions to macOS CI.

Existing processes retain their original macOS privacy attribution. After upgrading, restart the affected Codex process through the fixed launch path. Usagio's usage tracking does not require Photos, Music, or Documents access.
