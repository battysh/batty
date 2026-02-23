---
id: 2
title: Spawn N parallel agents in tmux windows with per-agent worktrees
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:04:38.035833547-05:00
started: 2026-02-23T00:57:58.479842145-05:00
completed: 2026-02-23T01:04:38.014765733-05:00
tags:
    - core
    - tmux
    - git
class: standard
---

When `--parallel N` is > 1, spawn N independent agent processes, each in its own tmux window and git worktree.

## Requirements

### Agent Spawning
- Extend `batty work <phase> --parallel N` to create N tmux windows (currently creates 1)
- Each window gets the standard executor + supervisor pane layout (reuse existing setup)
- Each agent gets a unique name via `kanban-md agent-name`
- Window naming: `agent-1`, `agent-2`, etc.
- `--parallel 1` must behave identically to current single-agent mode (no regression)

### Per-Agent Worktrees
- On spawn: create a git worktree for each agent at `.batty/worktrees/<phase>/<agent-name>/`
- Worktree branches: `batty/<phase>/<agent-name>`
- Agent reuses its worktree across multiple tasks within the session (no create/destroy per task)
- On session end: clean up worktrees (configurable: keep on failure for debugging)
- Handle existing worktrees from previous runs (resume vs fresh)

## Implementation Notes

- Extend existing tmux session creation in `src/tmux.rs`
- Extend existing worktree logic in `src/worktree.rs` for multiple concurrent worktrees
- Each agent's executor runs with CWD set to its worktree
- Agents don't decide what to work on — the scheduler (task 3) dispatches tasks to them

[[2026-02-23]] Mon 01:04
Implemented parallel phase launch path for batty work PHASE --parallel N (N>1): main routes to work::run_phase_parallel; per-agent git worktrees are provisioned at .batty/worktrees/<phase>/<agent>/ with branches batty/<phase>/<agent> (reuse by default, recreate with --new); one tmux session is created with N windows named from unique kanban-md agent-name values; each window launches an executor in that agent worktree and creates an executor plus log-pane split with pipe-pane capture logs under .batty/logs/parallel-<phase>-<timestamp>/<agent>/. Added tmux APIs create_window/rename_window/select_window plus tests for tmux window creation and agent worktree lifecycle (create/reuse/force-new/duplicate checks). Verified with full cargo test.
