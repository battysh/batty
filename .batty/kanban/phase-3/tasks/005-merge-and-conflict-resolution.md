---
id: 5
title: Merge and conflict resolution
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:10:32.863710438-05:00
started: 2026-02-22T15:08:42.367529356-05:00
completed: 2026-02-22T15:10:32.863710114-05:00
tags:
    - core
depends_on:
    - 3
class: standard
---

On merge approval from the review gate:
1. Merge phase branch to main
2. If conflicts: attempt guided resolution with diff context. If unresolved, escalate to human.
3. Post-merge: run tests on main to confirm nothing broke
4. Clean up worktree and branch
5. Update kanban board — all phase tasks marked done

[[2026-02-22]] Sun 15:10
Implemented merge/conflict-resolution automation for merge-approved worktree runs. Added merge_phase_branch_and_validate flow in work.rs: switch to base, merge run branch, auto-retry once via rebase when initial merge conflicts, and escalate with clear error if unresolved. On successful merge, Batty logs merge event, runs post-merge verification on base branch using defaults.dod (fallback cargo test), logs test execution/result, and only then allows run completion/cleanup. Added unit tests for clean merge path and unresolved-conflict escalation path. Validation: cargo test work::tests:: ; cargo test review::tests:: ; cargo test sequencer::tests:: ; cargo test log::tests::all_event_types_serialize. Full cargo test remains blocked in sandbox by tmux/openpty permissions.
