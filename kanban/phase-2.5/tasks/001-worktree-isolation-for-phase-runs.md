---
id: 1
title: Worktree isolation for phase runs
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T21:55:13.141446438-05:00
started: 2026-02-21T21:47:10.778573827-05:00
completed: 2026-02-21T21:55:13.141446088-05:00
tags:
    - core
    - safety
claimed_by: bloom-peak
claimed_at: 2026-02-21T21:55:13.141446388-05:00
class: standard
---

Run every `batty work <phase>` execution in an isolated git worktree by default.

## Requirements

1. Create worktree branch per run: `phase-X-run-NNN`.
2. Execute agent and supervision inside that worktree root.
3. Preserve current Phase 2 tmux behavior (session, log pane, status bar).
4. On successful review/merge, clean up worktree and branch.
5. On rejection/failure, keep worktree for inspection.

## Notes

- Isolation should not depend on having an AI director.
- This is the earliest safety boundary and should happen before phase sequencers.

## Statement of Work

- **What was done:** Added per-run git worktree isolation for `batty work <phase>` with deterministic branch naming (`phase-X-run-NNN`), executor launch from the worktree root, and merge-aware cleanup/retention behavior.
- **Files created:** `src/worktree.rs` (worktree lifecycle: create, numbering, merge detection, cleanup, retention, and tests).
- **Files modified:** `src/work.rs` (worktree creation before execution, run in worktree, finalize cleanup/retention, logging); `src/log/mod.rs` (new phase worktree lifecycle log events).
- **Key decisions:** Cleanup only occurs when the run branch both advanced beyond its start commit and is merged into the base branch; completed but unmerged runs are retained for review, and failed/detached runs are retained for inspection.
- **How to verify:** `cargo test` (full suite, including tmux/PTY integration tests when run with required host permissions) and targeted checks: `cargo test worktree::`, `cargo test work::tests::missing_phase_board_is_error`.
- **Open issues:** Full review-gate integration (director/human decision artifacts) is not yet implemented; this task uses merge state as the cleanup trigger and keeps unmerged worktrees by default.
