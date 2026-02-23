---
id: 8
title: Extend StatusBar with task count refresh logic
status: archived
priority: high
created: 2026-02-22T15:55:41.779566144-05:00
updated: 2026-02-23T00:52:22.660485667-05:00
started: 2026-02-23T00:52:22.660485316-05:00
completed: 2026-02-23T00:52:22.660485316-05:00
tags:
    - supervisor
    - tmux
    - status-bar
class: standard
---

In `src/orchestrator.rs`, add `tasks_dir: Option<PathBuf>`, `task_summary: String`, and `last_task_refresh: Option<Instant>` fields to `StatusBar`. Update `StatusBar::new()` to accept `tasks_dir`. Add `refresh_task_counts()` method that calls `task::load_tasks_from_dir()`, computes counts by status (backlog→"todo", in-progress→"active", done→"done"), formats a compact summary string omitting zero-count categories, and debounces to 5-second intervals. Call it from `update_inner()`.

## Acceptance Criteria

- `StatusBar` struct has `tasks_dir`, `task_summary`, and `last_task_refresh` fields
- `refresh_task_counts()` loads tasks, computes counts, and formats a compact summary
- Zero-count categories are omitted from the summary string
- Refresh is debounced to 5-second intervals
- `update_inner()` calls `refresh_task_counts()` before formatting the status line
