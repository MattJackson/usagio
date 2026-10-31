# usagio 1.0.3

Fixes Codex account switches getting stuck in draining mode on macOS and Linux. Usagio now requests a prompt bounded daemon stop, treats exited zombie processes as stopped, and writes credentials and restarts the daemon in the same operation instead of waiting for another poll.

A live multi-agent switch confirmed reconnection to the selected account without observed client interruption. The isolated test verifies switching during an active turn, account identity, and session resume. In-flight turns can be recorded as interrupted; uninterrupted completion of every turn or input submitted during the brief restart is not guaranteed.

Claude behavior and Windows' existing handoff remain unchanged.
