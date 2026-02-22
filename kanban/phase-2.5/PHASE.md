# Phase 2.5: Runtime Hardening + Dogfood

**Status:** Completed (partially rolled over)
**Board:** `kanban/phase-2.5/`
**Depends on:** Phase 2.4 complete

## Goal

Close the gaps between "it runs" and "we can trust it daily." This phase hardens runtime behavior and uses Batty to execute Batty's own work.

## Why this phase exists

Phase 2 proved tmux-based supervision works. Before adding sequencers and directors, we need:

- isolation by default (worktrees)
- deterministic launch and completion contracts
- crash recovery and supervision resume
- tmux compatibility handling
- explicit dogfood milestone

## Tasks Completed in Phase 2.5

1. **Worktree isolation for phase runs** — `batty work <phase>` runs inside a worktree by default.
2. **Prompt composition and context injection** — deterministic executor launch context (`CLAUDE.md`, `PHASE.md`, board state, config).
3. **Phase completion detection contract** — explicit completion signals and stop conditions.
4. **Crash recovery and supervision resume** — reconnect to existing tmux session and resume watchers/state.
5. **tmux capability/version compatibility** — probe features and degrade cleanly per version.

## Backlog Rollover

Remaining backlog tasks from this board were moved to `kanban/phase-2.6/` for simpler continuation:

- #6 Dogfood phase-2.6 with Batty
- #7 CLI install for Claude+Codex skills and steering docs
- #8 Improve `batty config` output formatting
- #9 Remove compiler warnings from `cargo build`
- #10 Add Rust linting and auto-lint workflow

## Exit Criteria

- Completed scope exits for tasks 1-5 are satisfied and merged.
- Rolled-over tasks are tracked and completed in `phase-2.6`.
