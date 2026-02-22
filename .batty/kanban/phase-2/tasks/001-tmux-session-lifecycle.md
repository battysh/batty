---
id: 1
title: tmux session lifecycle
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:27:38.348600385-05:00
started: 2026-02-21T20:25:05.989998595-05:00
completed: 2026-02-21T20:27:38.348600025-05:00
tags:
    - core
    - tmux
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:27:38.348600325-05:00
class: standard
---

`batty work phase-1` creates and manages a tmux session for the executor.

## Requirements

1. **Create session:** `tmux new-session -d -s batty-phase-1` with the executor command.
2. **pipe-pane setup:** `tmux pipe-pane -t batty-phase-1 "cat >> .batty/logs/pty-output.log"` — captures all executor output to a log file.
3. **Attach user:** `tmux attach -t batty-phase-1` — user sees the executor's live session.
4. **Session cleanup:** On executor exit or user Ctrl-C, clean up the tmux session and preserve logs.
5. **Reconnection:** `batty attach` re-attaches to a running session if disconnected.
6. **Session naming:** Convention: `batty-<phase>` (e.g., `batty-phase-1`).

## Implementation

- New module: `src/tmux.rs` — tmux session management (create, attach, kill, send-keys, pipe-pane, status).
- Refactor `src/work.rs` to use tmux instead of spawning PTY directly via portable-pty.
- The Phase 1 portable-pty code becomes a fallback path (`--no-tmux` flag) for environments without tmux.

## Edge cases

- tmux not installed → error with install instructions.
- Session name already exists → reuse or error with `batty attach` hint.
- User detaches (Ctrl-b d) → session keeps running, `batty attach` reconnects.

[[2026-02-21]] Sat 20:27
Statement of work:
- Created src/tmux.rs: full tmux session lifecycle (create, attach, kill, send-keys, pipe-pane, capture-pane, split-window, status bar, title)
- Added batty attach CLI command for reconnecting to running sessions
- 11 unit tests covering all tmux operations
- All functions ready for use by subsequent tasks (event extraction, status bar, log pane)
