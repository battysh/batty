---
id: 7
title: Add tasks_dir field to OrchestratorConfig
status: archived
priority: high
created: 2026-02-22T15:55:41.153986112-05:00
updated: 2026-02-23T00:52:22.63186725-05:00
started: 2026-02-23T00:52:22.631866589-05:00
completed: 2026-02-23T00:52:22.631866589-05:00
tags:
    - orchestrator
    - config
class: standard
---

Add `tasks_dir: Option<PathBuf>` to `OrchestratorConfig` in `src/orchestrator.rs`. Update all existing construction sites to pass `None` so nothing breaks. Update existing tests that build `OrchestratorConfig` to include the new field.

## Acceptance Criteria

- `OrchestratorConfig` has a `tasks_dir: Option<PathBuf>` field
- All existing code that constructs `OrchestratorConfig` compiles with `tasks_dir: None`
- All existing tests pass without modification (beyond adding the new field)
