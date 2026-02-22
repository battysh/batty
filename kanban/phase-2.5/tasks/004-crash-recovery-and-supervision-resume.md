---
id: 4
title: Crash recovery and supervision resume
status: backlog
priority: high
tags:
    - core
    - reliability
class: standard
---

If Batty crashes or is terminated mid-phase, tmux keeps running. Batty must reconnect and resume supervision safely.

## Requirements

1. Add `batty resume <phase|session>` command (or equivalent behavior in `batty work`).
2. Reconnect to active tmux session and pane ids.
3. Resume pipe-pane reading from last known offset.
4. Rebuild detector/supervisor state from logs and recent pane output.
5. Continue status bar and orchestrator log updates.
6. Refuse unsafe duplicate supervisor attachments.

## Validation

- Kill Batty process during active phase run.
- Restart Batty and resume supervision.
- Verify no prompt double-injection and no lost logs.

This is a Phase 2.x requirement, not a polish-only feature.
