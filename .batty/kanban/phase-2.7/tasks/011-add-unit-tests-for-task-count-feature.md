---
id: 11
title: Add unit tests for task count feature
status: archived
priority: medium
created: 2026-02-22T15:55:45.724136514-05:00
updated: 2026-02-23T00:52:22.738851539-05:00
started: 2026-02-23T00:52:22.738851168-05:00
completed: 2026-02-23T00:52:22.738851168-05:00
tags:
    - testing
    - status-bar
class: standard
---

Add tests in `src/orchestrator.rs`:

1. **Compact summary formatting** — test with various count combinations: all zero (empty string), some zero (omitted), all done, mixed statuses
2. **Debounce behavior** — test that `refresh_task_counts()` respects the 5-second debounce interval
3. **Backward compatibility** — verify existing tests still pass with `tasks_dir: None`

## Acceptance Criteria

- Tests cover all summary formatting edge cases (all-zero, some-zero, all-done, mixed)
- Debounce behavior is tested
- All existing tests pass with `tasks_dir: None`
- `cargo test` passes
