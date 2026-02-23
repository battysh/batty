---
id: 10
title: Pass tasks_dir from work.rs into OrchestratorConfig
status: archived
priority: high
created: 2026-02-22T15:55:45.512900592-05:00
updated: 2026-02-23T00:52:22.713841116-05:00
started: 2026-02-23T00:52:22.713840765-05:00
completed: 2026-02-23T00:52:22.713840765-05:00
tags:
    - work
    - orchestrator
    - integration
class: standard
---

In `src/work.rs`, pass `tasks_dir: Some(phase_dir.join("tasks"))` in `run_phase()` (~line 780) and `tasks_dir: Some(tasks_dir.clone())` in `resume_phase()` (~line 151) when constructing `OrchestratorConfig`.

## Acceptance Criteria

- `run_phase()` passes the phase's tasks directory into `OrchestratorConfig`
- `resume_phase()` passes the tasks directory into `OrchestratorConfig`
- Both use `Some(phase_dir.join("tasks"))` pattern
- Feature works end-to-end: `batty work` shows task counts in tmux status bar
