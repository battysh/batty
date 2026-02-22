# Batty Execution Loop

## `batty work <phase>`

Run it, watch it work, intervene only when needed.

### Step 1: Setup

Create isolated worktree for the run. Create tmux session. Launch executor in main pane. Start pipe-pane for output capture. Open orchestrator log pane at bottom. Set status bar.

### Step 2: Launch Executor

Compose prompt from phase board + project config + CLAUDE.md. Tell the executor: work through the board, commit per task, write statements of work, produce phase-summary.md when done.

### Step 3: Supervise (tmux-based)

Batty's Rust process monitors the piped output alongside tmux:

```
tmux executor pane
    │
    ├──→ Human (tmux attach — interactive, can type anytime)
    │
    ├──→ pipe-pane → log file
    │       │
    │       ├──→ Event extractor (Rust, regex)
    │       │       → event buffer + status bar + orchestrator pane
    │       │
    │       └──→ Prompt detector (silence + pattern)
    │               │
    │               ├── Known? → Tier 1: send-keys (instant)
    │               └── Unknown? → Tier 2: supervisor API call → send-keys
    │
    └──→ Human keystrokes (always take priority)
```

**Tier 1** — regex match → `tmux send-keys`. Handles ~70-80% of prompts. Zero cost.

**Tier 2** — single API call with: project docs (cached) + event buffer + question. Stateless. Answer injected via send-keys.

### Step 4: Completion

Executor finishes all tasks. Produces phase-summary.md. Worktree has: one commit per task, statements of work, all tests passing.

### Step 5: Review Gate

Reviewer receives: diff, phase summary, statements of work, execution log, project docs.

In Phase 3A this reviewer is human. In Phase 3B this reviewer can be a director agent.

Decision: **merge** / **rework** (relaunch executor with feedback) / **escalate** (surface to human).

### Step 6: Merge

Merge phase branch to main. Resolve conflicts (review gate escalates if needed). Run tests. Clean up. Next phase.

## What the Human Sees

```
┌── tmux: batty-phase-1 ──────────────────────────┐
│                                                   │
│  (executor's live terminal session)               │
│  Claude Code working on task #7...                │
│                                                   │
├───────────────────────────────────────────────────┤
│  [batty] ✓ auto-answered: "Continue?" → y          │
│  [batty] ? supervisor thinking: "async or sync?"   │
│  [batty] ✓ supervisor answered → async             │
│  [batty] → task #7 done, picking #8               │
├───────────────────────────────────────────────────┤
│ [batty] phase-1 | task #8 | 7/11 done | ✓ active │
└───────────────────────────────────────────────────┘
```

Type into the executor pane anytime. Detach and re-attach. Scroll the orchestrator pane. Human is always in control.
