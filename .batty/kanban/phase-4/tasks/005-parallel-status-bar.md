---
id: 5
title: Extend status bar for parallel agent progress
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:20:11.425574098-05:00
started: 2026-02-23T01:18:54.278335448-05:00
completed: 2026-02-23T01:20:11.404431299-05:00
tags:
    - ux
    - tmux
depends_on:
    - 3
class: standard
---

Extend the existing tmux status bar to show multi-agent progress.

## Requirements

- **Global status bar:** `[5/12 tasks] [3 agents] [1 merging]` — task completion, active agent count, merge queue depth
- **Per-window status:** each agent window shows its current task title and elapsed time
- **Agent idle indicator:** show when an agent is waiting for deps to unblock vs actively working

## Implementation Notes

- Extend existing status bar logic in tmux.rs — don't build a separate dashboard
- Keep it minimal: the status bar is the dashboard
- No `batty status` command or DAG visualization — defer to `kanban-md board --compact`

[[2026-02-23]] Mon 01:20
Extended parallel runtime status visualization in run_phase_parallel. Added global tmux status updates on each scheduler tick with task completion counts, agent count, and merge activity (`[done/total tasks] [N agents] [M merging]`). Added per-window dynamic labels by renaming each agent window to include current task id/title and elapsed minutes when active, and `waiting-deps` vs `idle` indicators when not active. Added scheduler dispatch metadata (task_title plus done/total counters) so status updates are accurate without extra board parses. Verified with full cargo test.
