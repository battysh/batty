# Batty Architecture

## Overview

Three execution roles, one tmux session, kanban board as command center.

```
┌── tmux session: batty-phase-1 ───────────────────────┐
│  Executor pane (Claude Code / Codex / Aider)         │
│  Works through phase board. Commits per task.        │
├──────────────────────────────────────────────────────┤
│  Orchestrator pane (batty events)                    │
│  Auto-answers, supervisor decisions, task progress    │
├──────────────────────────────────────────────────────┤
│  [batty] phase-1 | task #7 | 6/11 | ✓ supervising   │ ← status bar
└──────────────────────────────────────────────────────┘

Batty process (alongside tmux):
  pipe-pane → event extraction → prompt detection
  Tier 1: regex → send-keys (instant)
  Tier 2: supervisor API call → send-keys (on-demand)
```

## Three Layers

**Director** — Strategy. Reviews completed phases → merge / rework / escalate. Picks next phase. Never writes code.
In early rollout this role can be handled by a human reviewer.

**Supervisor** — Tactics. Watches executor's terminal, answers questions using project context. Escalates when it can't decide. Never writes code.

**Executor** — Labor. BYO agent (Claude, Codex, Aider). Works through the phase board in a tmux pane. Doesn't know Batty exists.

Human sits above all three. See everything, type anytime, override anything.

## Data Flow

```
kanban/phase-1/                    ← phase board
    ↓
batty work phase-1
    ↓
tmux session created
  executor in main pane
  pipe-pane captures output
  orchestrator pane shows events
    ↓
executor works through tasks
  batty auto-answers prompts (send-keys)
  batty updates status bar
    ↓
review gate (human first, director later)
  → merge / rework / escalate
    ↓
merge to main, clean up
```

All state is Markdown files. No databases. Git tracks everything.

## Progressive Autonomy

| Mode | Human involvement |
|---|---|
| `batty work phase-1` | Watch one phase, answer questions |
| `batty work all` | Watch sequential phases, intervene when needed |
| `batty work all --parallel 3` | Multiple phases, focus on exceptions |
| Future: `fully-auto` | Director runs overnight, review in the morning |

Each level earned by demonstrating reliability at the previous level. Policy tiers (observe → suggest → act) gate the transition.

## Key Design Decisions

**Why tmux?** Output capture (pipe-pane), input injection (send-keys), status bar, panes, session persistence — all for free. No custom terminal code.

**Why worktrees?** Executor might produce bad work. Reviewer might reject it. Worktree keeps main clean until approved.

**Why two-tier prompt handling?** Regex handles ~70-80% of prompts instantly (zero cost). Supervisor agent handles the rest with project context (one API call per question).

**Why separate director and supervisor?** Doing work and evaluating work are different skills. Splitting them prevents evaluation bias.
