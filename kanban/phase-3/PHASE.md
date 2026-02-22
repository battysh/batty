# Phase 3A: Sequencer + Human Review Gate

**Status:** Not Started
**Board:** `kanban/phase-3/`
**Depends on:** Phase 2.5 complete

## Goal

Separate phase sequencing from AI review. Ship `batty work all` with a reliable human review gate first, then layer AI director review in Phase 3B.

## What Already Exists (from Phases 1-2.5)

- Rust CLI with `batty work <phase>` command
- tmux-based execution: session lifecycle, pipe-pane, send-keys pipeline
- Two-tier prompt handling (Tier 1 regex + Tier 2 supervisor agent)
- Policy engine, test gates, execution log
- Agent adapter trait + Claude Code adapter
- Orchestrator log pane + tmux status bar

## Architecture

```
Human (strategy + steering)
  ↓
Human reviewer (merge/rework decision)
  ↓
Supervisor (controls one phase in tmux)
  ↓
Executor (BYO agent in tmux pane)
```

This phase adds:
1. Phase discovery and ordering
2. Phase summaries and review artifacts
3. Human review gate contract
4. Rework loop from human feedback
5. Merge, test, and cleanup automation
6. `batty work all`

## Tasks (8 total)

1. **Phase discovery and ordering** (critical) — list phase directories, sort deterministically, skip completed phases.
2. **Phase summary production** — Executor generates phase-summary.md on completion (what changed, tests, key decisions).
3. **Human review gate** — standard review packet + explicit merge/rework/escalate decision.
4. **Rework loop** — relaunch executor with reviewer feedback. Max retries configurable.
5. **Merge and conflict resolution** — Merge worktree to main, run tests, handle conflicts (retry rebase or escalate).
6. **`batty work all` phase sequencer** — Reads all phase boards in order and runs each through the human review loop.
7. **Codex CLI adapter** — Second agent adapter. Validates that the AgentAdapter trait works for non-Claude agents.
8. **Phase 3A exit criteria** — `batty work all` chains phases. Human review gate and rework loop work. Two adapters functional.

## Key Technical Notes

- Human review should use the same structured artifacts that AI director will consume in Phase 3B
- Phase summary should include: files changed, tests added/modified, key decisions, open issues
- `batty work all` reads `kanban/` directory, sorts phases numerically, runs each
- Codex CLI adapter: different prompt format and interaction patterns than Claude Code

## Exit Criteria

- `batty work all` chains phases in order
- Human review gate accepts merge/rework/escalate decisions
- Rework loop re-executes with feedback
- Merge automation runs tests and cleans worktree/branch
- Two agent adapters work (Claude Code + Codex CLI)

## Kanban Commands

```bash
kanban-md board --compact --dir kanban/phase-3
kanban-md pick --claim <agent> --status backlog --move in-progress --dir kanban/phase-3
kanban-md show <ID> --dir kanban/phase-3
kanban-md move <ID> done --dir kanban/phase-3
kanban-md edit <ID> -a "note" -t --claim <agent> --dir kanban/phase-3
```

## Reference Docs

- `planning/architecture.md` — three-layer hierarchy
- `planning/execution-loop.md` — full execution loop including review gate
- `planning/roadmap.md` — phase goals
- `CLAUDE.md` — agent instructions, commit format, statement of work template
