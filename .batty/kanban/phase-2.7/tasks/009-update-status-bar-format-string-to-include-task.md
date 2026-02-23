---
id: 9
title: Update status bar format string to include task counts
status: archived
priority: high
created: 2026-02-22T15:55:42.556070033-05:00
updated: 2026-02-23T00:52:22.68646184-05:00
started: 2026-02-23T00:52:22.68646148-05:00
completed: 2026-02-23T00:52:22.68646148-05:00
tags:
    - supervisor
    - tmux
    - ux
class: standard
---

Update the format string in `StatusBar::update_inner()` to insert `task_summary` between phase name and supervision indicator.

Before: `" [batty] phase-2.7 | ✓ supervising"`
After: `" [batty] phase-2.7 | 3 todo · 1 active · 7 done | ✓ supervising"`

When `task_summary` is empty (no tasks_dir or all-zero counts), no extra separator appears — the format falls back to the current layout.

## Acceptance Criteria

- Task counts appear between phase name and supervision indicator
- Categories use middle-dot (·) separator
- When task_summary is empty, no extra ` | ` separator appears
- Format matches: `" [batty] <phase> | <counts> | <status>"`
