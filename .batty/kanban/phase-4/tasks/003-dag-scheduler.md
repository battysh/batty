---
id: 3
title: DAG-driven task scheduler with claim coordination
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:09:32.461429514-05:00
started: 2026-02-23T01:04:41.152346238-05:00
completed: 2026-02-23T01:09:32.437428117-05:00
tags:
    - core
    - scheduler
depends_on:
    - 1
    - 2
class: standard
---

Central scheduler loop that watches for task completions, recomputes the ready frontier from the DAG, and dispatches tasks to idle agents. The scheduler is the single point of dispatch — agents never self-serve from the board.

## Requirements

### Scheduling Loop
- Poll board state -> compute ready frontier via DAG -> assign ready tasks to idle agents -> wait for completions -> repeat
- On dispatch: `kanban-md pick --claim <agent> --status backlog --move in-progress`
- Completion detection: watch for agents marking tasks `done` (poll board state)
- Termination: all tasks done, or no unblocked tasks remain and all agents idle

### Claim Coordination
- Scheduler is the single point of dispatch — no agent races
- Verify claim succeeded by re-reading task file after `kanban-md pick`
- Release claim on agent crash or task failure: `kanban-md edit <ID> --release`
- Timeout: if a task is claimed but no progress for N minutes, consider it stuck and escalate

### Error Handling
- Deadlock detection: no tasks ready, no agents running, tasks remain -> report and exit
- Agent crash: release task claim, mark agent slot as available, continue scheduling

## Implementation Notes

- New module: `src/scheduler.rs`
- Uses `dag.rs` (task 1) for frontier computation
- Uses agent spawner (task 2) for dispatch
- Polling interval configurable (default: 5 seconds)
- Runs in the main batty process as a tokio task
- Consider tokio watch/notify channels for agent -> scheduler completion signals

[[2026-02-23]] Mon 01:09
Implemented DAG-driven scheduler in src/scheduler.rs with board polling, ready frontier computation via src/dag.rs, dispatch to idle agents using kanban-md pick --claim <agent> --status backlog --move in-progress, claim verification by re-reading task frontmatter claimed_by, completion detection, deadlock detection, stuck-agent detection, and claim release on crash/failure. Added SchedulerTick model and command-runner abstraction (shell + mock) with unit tests for frontier, dispatch, verification failure/release, crash handling, and stuck/deadlock scenarios. Integrated scheduler into run_phase_parallel loop: after spawning windows, batty now runs a scheduler tick loop in-process, reports dispatch/completion events, handles crashed panes by releasing claims, errors on deadlock/stuck, and exits when all active tasks are done. Added DAG validation/topological-sort checks before phase launches.
