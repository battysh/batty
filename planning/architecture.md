# Batty Architecture

## Overview

Hierarchical agent team running in a single tmux session. A YAML-defined org chart (architect, managers, engineers) with daemon-managed communication, status monitoring, and task routing.

```
┌── tmux session: batty ─────────────────────────────────────────┐
│  ┌─ Architect ──────┐  ┌─ Manager ──────────────────────────┐  │
│  │  Strategic pane   │  │  Tactical pane                     │  │
│  │  Owns roadmap +   │  │  Owns kanban board, assigns tasks, │  │
│  │  architecture     │  │  supervises engineers               │  │
│  └──────────────────┘  └────────────────────────────────────┘  │
│  ┌─ Engineer 1 ─────┐  ┌─ Engineer 2 ─┐  ┌─ Engineer 3 ─┐    │
│  │  Coding agent     │  │  Coding agent │  │  Coding agent │    │
│  │  (Claude/Codex)   │  │              │  │              │    │
│  └──────────────────┘  └──────────────┘  └──────────────┘    │
├──────────────────────────────────────────────────────────────────┤
│  batty daemon (background): message routing, pane monitoring,   │
│  status tracking, Telegram bridge, event logging                │
└──────────────────────────────────────────────────────────────────┘
```

## Three Roles

**Architect** — Strategy. Defines what to build and why. Owns `planning/architecture.md` and `planning/roadmap.md`. Sends directives to managers. Never writes code, never manages tasks directly.

**Manager** — Tactics. Owns the kanban board. Breaks directives into tasks, assigns them to engineers, supervises progress, gates on quality. Sends status updates to architect.

**Engineer** — Labor. BYO coding agent (Claude Code, Codex, Aider). Receives task assignments, writes code, runs tests. Reports completion to manager.

**User** — Routing endpoint. Receives messages from any role. No tmux pane — communicates via `batty inbox` and Telegram.

Human sits above all roles. Can `batty attach` to see everything, type in any pane, override anything.

## Communication

All inter-role communication flows through the daemon's message bus:

```
batty send <role> "<message>"     → delivers to role's tmux pane
batty inbox <role>                → reads role's received messages
```

Messages are injected into target panes via tmux `load-buffer` + `paste-buffer`. The daemon polls inboxes and delivers pending messages. Telegram bridge allows remote monitoring and communication.

## Daemon

The daemon (`batty start`) is the system's nervous system:

- **Pane monitoring** — watches each agent's tmux pane for status changes
- **Message routing** — delivers messages between roles via tmux paste
- **Status tracking** — detects idle, working, waiting states per agent
- **Event logging** — all events persisted to `.batty/team_config/events.jsonl`
- **Telegram bridge** — optional remote monitoring and message relay

## Data Model

```
.batty/
  team_config/
    team.yaml              ← org chart: roles, instances, hierarchy
    batty_architect.md     ← architect prompt template
    batty_manager.md       ← manager prompt template
    batty_engineer.md      ← engineer prompt template
    events.jsonl           ← event log
    kanban.md              ← team kanban board
  inboxes/                 ← per-role message queues
  daemon.pid               ← daemon process ID
  daemon.log               ← daemon output
```

All state is files. No databases. Git tracks everything that matters.

## Layout

The tmux layout builder creates zones for each role tier:

- Architect gets a dedicated pane
- Each manager gets a pane, with their engineers grouped below
- Engineer panes are partitioned by their managing manager
- Pane IDs (`%N`) are globally unique and used as direct tmux targets

## Workflow Control Plane

The workflow control plane adds structured task lifecycle management on top of the existing team model. Instead of inferring orchestration truth from chat messages, the control plane treats board/task metadata as the primary source of truth for task state, dependencies, ownership, review, and merge disposition.

### Capability Model

Workflow responsibilities are resolved from role type, hierarchy, and optional config overrides — not hardcoded role names:

- **Planner** — defines or prioritizes frontier work (typically architect-type roles)
- **Dispatcher** — decomposes runnable work and routes it to executors (typically manager-type roles)
- **Executor** — performs bounded implementation work (typically engineer-type roles)
- **Reviewer** — accepts, rejects, merges, or escalates completed work (typically manager or architect-type roles)
- **Orchestrator** — monitors workflow state, computes next actions, drives automatic interventions (daemon + visible pane)
- **Operator** — external human endpoint when one exists (user-type roles)

One role may hold multiple capabilities depending on topology. In a solo topology, one role plans, executes, and reviews. In a manager topology, architect plans, manager dispatches/reviews, engineers execute.

### Task Lifecycle

Tasks flow through explicit states: `backlog` → `todo` → `in-progress` → `review` → `done` (or `blocked`, `archived` at any point). Each task tracks:

- Execution ownership (who builds it)
- Review ownership (who reviews it)
- Dependencies (`depends_on` other task IDs)
- Artifacts (branch, worktree, commit, test results)
- Review disposition (merge, rework, discard, escalate)
- Next action (which capability should act next)

### Orchestrator Surface

The orchestrator is a visible tmux pane showing workflow decisions, not just a hidden daemon. It uses the same CLI/API surface as agents and humans — no hidden mutation paths. Teams can disable the built-in orchestrator and drive workflow manually through CLI commands.

### Operating Modes

- **Legacy** — current Batty behavior, message-driven coordination, optional nudges
- **Hybrid** — workflow features enabled selectively alongside current runtime
- **Workflow-first** — workflow state is the primary orchestration truth, messaging is assistive

### Data Extensions

Task markdown files gain workflow metadata in YAML frontmatter: `depends_on`, `review_owner`, `blocked_on`, `worktree_path`, `branch`, `commit`, `artifacts`, `next_action`. Older files without these fields are handled with safe defaults. kanban-md compatibility is preserved.

## Key Design Decisions

**Why tmux?** Output capture (pipe-pane), input injection (send-keys/paste-buffer), status bar, panes, session persistence — all for free. No custom terminal code.

**Why YAML org chart?** One file defines the entire team topology. Easy to version, easy to change, easy to reason about.

**Why daemon?** Continuous background monitoring enables reactive behaviors (status tracking, message delivery, Telegram relay) without blocking the CLI.

**Why inbox-based messaging?** Decouples sender from receiver. Messages queue up and deliver when the target agent is ready. Prevents message loss during agent restarts.

**Why separate architect/manager/engineer?** Strategy, tactics, and execution are different skills. Splitting them prevents scope creep and evaluation bias. Each role has a focused prompt template.
