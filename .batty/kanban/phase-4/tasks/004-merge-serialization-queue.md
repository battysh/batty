---
id: 4
title: Merge serialization queue for parallel agent results
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-23T01:11:28.704518272-05:00
started: 2026-02-23T01:09:36.717906162-05:00
completed: 2026-02-23T01:11:28.680363124-05:00
tags:
    - core
    - git
    - merge
depends_on:
    - 2
    - 3
class: standard
---

When agents complete tasks in their worktrees, serialize merges back to the target branch. Only one merge at a time.

## Requirements

- **Queue:** completed tasks enter a FIFO merge queue (first to finish merges first)
- **Merge flow:** rebase agent worktree on latest target branch -> run tests -> fast-forward merge
- **Conflict handling:** if rebase fails, pull latest, retry once. If still fails, pause task and escalate
- **Test gate:** run project test suite after rebase, before merge. Fail -> do not merge, report error
- **Notification:** after successful merge, notify scheduler so downstream deps can unblock
- **Strictly serialized:** never more than one merge in progress

## Implementation Notes

- New module: `src/merge_queue.rs`
- Uses `git rebase`, `git merge --ff-only` via shell
- Runs as a tokio task alongside the scheduler
- Merge target is `main` or a phase branch (configurable)
- After merge, other agents may need `git pull --rebase` — handle at merge time or at next task dispatch

[[2026-02-23]] Mon 01:11
Implemented serialized merge queue in src/merge_queue.rs. Added FIFO queueing of completed-task merge requests, per-item flow (rebase agent branch onto target branch with retry + pull/rebase refresh, test gate command after rebase, ff-only merge into target), and explicit failure propagation for unresolved conflicts or test-gate failures. Added unit tests for FIFO ordering, test gate blocking merge, and conflict/retry failure behavior. Integrated merge queue into run_phase_parallel: completed tasks now enqueue merge requests tied to the agent branch, queue depth is reported, and only one merge is processed at a time in the scheduler loop. Added error escalation path on merge failures.
