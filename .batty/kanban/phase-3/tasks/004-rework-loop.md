---
id: 4
title: Rework loop
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:08:12.648751587-05:00
started: 2026-02-22T15:05:30.307150366-05:00
completed: 2026-02-22T15:08:12.648751271-05:00
tags:
    - core
depends_on:
    - 3
class: standard
---

On rework decision: relaunch executor in the same worktree with reviewer feedback as additional context. Executor addresses issues, commits, produces updated summary. Loop back to review.

Max rework cycles configurable in .batty/config.toml. If exceeded, escalate to human.

In Phase 3A this feedback source is the human reviewer. The same loop later supports AI director feedback in Phase 3B.

[[2026-02-22]] Sun 15:08
Implemented automated rework loop for worktree runs in run_phase: when human review decision is rework, Batty relaunches the phase in the same worktree, injects reviewer feedback into launch context under a dedicated rework section, and tracks rework attempt count. Added max-retry enforcement using defaults.max_retries; exceeding retries fails the run with explicit escalation reason. Added structured log event rework_cycle_started for auditability and test coverage for rework prompt context. Validation: cargo test review::tests:: ; cargo test work::tests::compose_launch_context_ ; cargo test sequencer::tests:: ; cargo test log::tests::all_event_types_serialize. Full cargo test remains blocked by tmux/openpty sandbox permissions.
