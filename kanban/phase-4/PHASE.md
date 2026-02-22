# Phase 4: Parallel Execution

**Status:** Not Started
**Board:** `kanban/phase-4/`
**Depends on:** Phase 3A complete (Phase 3B optional)

## Goal

`batty work all --parallel N` runs multiple phases concurrently in separate tmux windows, each in its own git worktree. A merge queue serializes merges to main.

## What Already Exists (from Phases 1-3A)

- Full execution pipeline: tmux session, two-tier supervision, policy engine, test gates
- Worktree isolation and runtime hardening from Phase 2.5
- `batty work all` sequential phase execution with human review gate
- Agent adapters for Claude Code and Codex CLI

## Architecture

```
batty work all --parallel 3
  ├── tmux window 1: phase-2 (worktree: phase-2-run-001)
  ├── tmux window 2: phase-3 (worktree: phase-3-run-001)
  └── tmux window 3: phase-4 (worktree: phase-4-run-001)

Each window has:
  ├── Executor pane (top)
  ├── Orchestrator pane (bottom)
  └── Status bar (phase/task/progress)

Merge queue: phase-2 merges first → rebase phase-3 → merge → rebase phase-4 → merge
```

## Tasks (3 total)

1. **tmux multi-window parallel execution** — Launch N phases in separate tmux windows. Each has its own worktree, supervisor, and executor. User switches between windows with tmux keybindings.
2. **Merge queue for parallel phases** — When phases complete, serialize merges. Rebase each worktree before merging. Re-run tests after rebase. Handle conflicts (retry or escalate).
3. **Phase 4 exit criteria** — Multiple phases run in parallel. Merge queue works. No data loss from concurrent merges.

## Key Technical Notes

- tmux windows (not panes) for parallel phases — each window is a full executor+orchestrator layout
- `tmux new-window` for each parallel phase
- `tmux select-window` to switch between phases
- Merge queue must be strictly serialized to avoid conflicts
- If rebase fails, offer: retry, manual resolution, or skip
- If Phase 3B is enabled later, queue can delegate review to director but merge serialization rules stay the same.

## Exit Criteria

- `batty work all --parallel N` launches N phases concurrently
- Each phase runs in isolated tmux window + git worktree
- Merge queue serializes merges to main without data loss
- User can switch between phase windows in tmux

## Kanban Commands

```bash
kanban-md board --compact --dir kanban/phase-4
kanban-md pick --claim <agent> --status backlog --move in-progress --dir kanban/phase-4
kanban-md show <ID> --dir kanban/phase-4
kanban-md move <ID> done --dir kanban/phase-4
```

## Reference Docs

- `planning/roadmap.md` — parallel execution goals
- `planning/architecture.md` — system design
- `CLAUDE.md` — agent instructions
