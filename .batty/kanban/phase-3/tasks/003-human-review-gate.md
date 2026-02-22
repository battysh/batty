---
id: 3
title: Human review gate
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:05:17.24911065-05:00
started: 2026-02-22T15:02:09.839280629-05:00
completed: 2026-02-22T15:05:17.249110017-05:00
tags:
    - core
depends_on:
    - 2
class: standard
---

On phase completion, generate a standardized review packet and require an explicit human decision.

Review packet includes:
- Diff against main: `git diff main...phase-X-run-NNN`
- phase-summary.md
- Statements of work from each task
- Execution log

Reviewer decisions:
- **Merge** — work meets standards
- **Rework** — writes specific feedback for the executor
- **Escalate** — pause and surface for manual handling

Decision must be persisted in structured logs for downstream automation.

[[2026-02-22]] Sun 15:05
Implemented human review gate primitives and wiring for worktree runs. Added src/review.rs with standardized review packet generation (diff command, phase summary, task statements, execution log) and explicit decision capture/validation for merge|rework|escalate (interactive prompt + env override). Added structured log events for review_packet_generated and review_decision. Integrated into run_phase completion path: for completed worktree runs, Batty now generates review-packet.md and requires explicit review decision before marking run complete; non-merge decisions fail the run with logged rationale. Validation: cargo test review::tests:: ; cargo test work::tests::compose_launch_context_includes_required_sources ; cargo test sequencer::tests:: ; cargo test log::tests::all_event_types_serialize. Full cargo test still blocked by tmux/openpty sandbox permissions.
