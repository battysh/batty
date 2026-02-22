# Phase 2.5: Runtime Hardening + Dogfood

**Status:** Next
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

## Tasks (10 total)

1. **Worktree isolation for phase runs** — `batty work <phase>` runs inside a worktree by default.
2. **Prompt composition and context injection** — deterministic executor launch context (`CLAUDE.md`, `PHASE.md`, board state, config).
3. **Phase completion detection contract** — explicit completion signals and stop conditions.
4. **Crash recovery and supervision resume** — reconnect to existing tmux session and resume watchers/state.
5. **tmux capability/version compatibility** — probe features and degrade cleanly per version.
6. **CLI install for Claude+Codex skills and steering docs** — one Batty command installs skills and steering docs for both agents (with per-agent targeting).
7. **Improve `batty config` output formatting** — cleaner human output and optional machine-readable mode.
8. **Remove compiler warnings from `cargo build`** — reduce/no dead-code and unused warnings in normal dev build.
9. **Add Rust linting and auto-lint workflow** — clippy + rustfmt checks and auto-fix path.
10. **Dogfood phase-2.5 with Batty** — execute this phase with Batty, review, merge, and capture postmortem.

## Exit Criteria

- `batty work phase-2.5` executes safely in an isolated worktree.
- Batty can restart mid-run and resume supervision without losing control.
- Completion detection is deterministic and documented.
- tmux feature differences are handled with clear messages/fallbacks.
- Default dev build output is clean (or warnings are intentionally scoped and documented).
- Lint/format workflow is documented and runnable in one command.
- Batty completes this phase itself and merges the result.
