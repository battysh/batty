# Phase 2: tmux-based Intelligent Supervisor

**Status:** Done
**Board:** `kanban/phase-2/`

## Goal

Replace the PTY-only supervision from Phase 1 with tmux-based execution. `batty work <phase>` launches a tmux session with the executor in the main pane, an orchestrator log in a bottom pane, and status in the tmux status bar.

## What Already Exists (from Phase 1)

The Rust codebase in `src/` has:
- CLI framework (`src/cli.rs`, `src/main.rs`)
- Task reader for kanban-md files (`src/task/`)
- Agent adapter trait + Claude Code adapter (`src/agent/`)
- PTY supervision with bidirectional I/O (`src/supervisor/`)
- Prompt detection via regex (`src/prompt/`, `src/detector.rs`)
- Policy engine with tiers (`src/policy/`)
- Test-gated completion (`src/dod/`)
- Execution log (`src/log/`, `src/events.rs`)
- Work command (`src/work.rs`)

You are building **on top of** this. The existing PTY code stays as fallback. New tmux code adds a higher-level execution path.

## tmux Architecture

```
┌─────────────────────────────────────────────┐
│  Executor pane (~80%)                       │
│  Agent (Claude Code / Codex / Aider)        │
│  running interactively in tmux pane         │
├─────────────────────────────────────────────┤
│  Orchestrator pane (~20%)                   │
│  tail -f on event log — shows supervisor    │
│  decisions, prompt detections, policy actions│
├─────────────────────────────────────────────┤
│  tmux status bar                            │
│  phase | task | progress | supervisor state │
└─────────────────────────────────────────────┘
```

**Data flow:**
```
pipe-pane → event extractor → prompt detector → Tier 1 (regex) → send-keys
                                              → Tier 2 (supervisor agent) → send-keys
```

- **pipe-pane** captures executor output to a file/pipe
- **Event extractor** reads piped output, extracts structured events via regex
- **Prompt detector** uses silence + pattern heuristic to detect questions
- **Tier 1** auto-answers ~70-80% of prompts via regex match → send-keys (instant)
- **Tier 2** calls a supervisor agent (e.g., `claude -p`) with project context for harder questions
- **send-keys** injects answers back into the executor pane

## Tasks (10 total)

Work through these in dependency order. Critical path: 1 → 2 → 3 → 4 → 5.

1. **tmux session lifecycle** (critical) — create session, attach, pipe-pane, send-keys, reconnect, teardown
2. **Event extraction from pipe** (critical, depends: 1) — read piped output, parse into structured events
3. **Prompt detection heuristic** (critical, depends: 2) — silence + regex pattern = prompt detected
4. **Tier 1 auto-answer via send-keys** (critical, depends: 1, 3) — pattern match → send-keys injection
5. **Tier 2 supervisor agent** (critical, depends: 2, 4) — API call with context → send-keys
6. **tmux status bar** (high, depends: 1, 2) — display phase/task/progress/state
7. **Orchestrator log pane** (critical, depends: 1, 2) — bottom split pane showing event log
8. **Stuck detection** (high, depends: 2, 3) — detect looping/stalled/crashed executor
9. **Human override** (high, depends: 4, 5) — detect human typing, supervisor steps back
10. **Phase 2 exit criteria** (critical, depends: 4-9) — integration test, all features working

## Key Technical Notes

- tmux commands: `tmux new-session`, `tmux split-window`, `tmux pipe-pane`, `tmux send-keys`, `tmux set-option status-left/right`
- The orchestrator pane runs `tail -f` on the JSON lines execution log
- Human override: detect that input came from the user (not from send-keys) and pause auto-answering
- Tier 2 supervisor is stateless — one API call per question, no persistent session. See `src/tier2.rs` for the existing implementation.
- All new code needs unit tests in `#[cfg(test)]` modules

## Exit Criteria

- Executor works through a board in a tmux session
- Routine prompts auto-answered (Tier 1)
- Complex questions answered by supervisor agent (Tier 2)
- Status bar + orchestrator pane show everything happening
- Session survives disconnect and can be reattached
- Human can type in the executor pane and supervisor backs off

## Kanban Commands

```bash
kanban-md board --compact --dir kanban/phase-2
kanban-md pick --claim <agent> --status backlog --move in-progress --dir kanban/phase-2
kanban-md show <ID> --dir kanban/phase-2
kanban-md move <ID> done --dir kanban/phase-2
kanban-md edit <ID> -a "note" -t --claim <agent> --dir kanban/phase-2
```

## Reference Docs

- `planning/architecture.md` — three-layer hierarchy, data flow
- `planning/execution-loop.md` — step-by-step execution loop with tmux pipeline
- `planning/roadmap.md` — phase goals and exit criteria
- `CLAUDE.md` — agent instructions, commit format, statement of work template
