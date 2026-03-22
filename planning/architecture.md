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

## Intelligence Layer

The intelligence layer makes the team self-aware and self-improving. It builds on the event stream, workflow metrics, and standup infrastructure to provide operational insight, automated feedback loops, and reusable knowledge.

### Periodic Standups

The daemon generates and delivers standup summaries on a configurable interval. Standups are scoped by hierarchy — each role sees only their direct reports. Standups include not just agent status (idle/working) but also board-derived context: what tasks are assigned, what's blocked, what's in review, and how long items have been in their current state. The standup interval and content depth are configurable in team.yaml.

### Run Retrospectives

When a run completes (all board tasks done, or team stopped), Batty analyzes the event log and board history to produce a retrospective. The retrospective reports: total cycle time, per-task duration, failure/retry counts, escalation frequency, merge conflict rate, and idle-time percentage. It identifies the top bottlenecks (e.g., "review queue stalled for 12 minutes," "eng-1-2 hit 3 test failures on T-005"). Retrospectives are written as markdown files to `.batty/retrospectives/` and optionally sent to the user via Telegram.

### Failure Pattern Detection

The daemon tracks recurring failure signatures across the event stream: repeated test failures on similar code paths, frequent escalations from the same engineer, merge conflicts on the same files. When a pattern is detected, it surfaces a structured observation to the manager (or architect, depending on severity). This is not automated remediation — it is automated noticing. The team decides what to do.

### Team Templates

Successful team configurations (topology, prompt templates, workflow policies) can be exported as reusable templates via `batty export-template`. Templates capture the team.yaml, prompt files, and workflow policy settings. `batty init --template <name>` bootstraps a new project from a saved template. This enables cross-project learning without building a recommendation engine — the human curates which configurations are worth reusing.

## Hardened Runtime

The runtime relies on shell-outs to git and kanban-md, tmux paste-buffer injection for message delivery, and worktree lifecycle management. Each of these has observed failure modes that cause silent task stalls during multi-hour runs.

### Command Infrastructure

All external tool invocations (git, kanban-md) flow through typed command layers that capture stderr, classify errors as transient or permanent, and return structured results. Transient failures (lock contention, temporary I/O errors) retry with backoff. Permanent failures (bad ref, permission denied) surface immediately with diagnostic context. This replaces the current pattern of scattered `Command::new()` calls with ad-hoc error handling.

### Delivery Confirmation

Message delivery via tmux paste-buffer is fire-and-forget today. The hardened runtime adds post-injection verification: after pasting, sample the target pane to confirm the message appeared. Failed deliveries are flagged to the daemon for retry. This closes the gap where messages are "delivered" in the inbox but never actually reach the agent.

### Agent Lifecycle Management

Agents can exhaust their context window, crash, or silently stall. The daemon detects these states through output monitoring: an agent marked as "working" that stops producing output or produces degraded output (repeated errors, empty responses) is flagged as potentially exhausted. Recovery involves restarting the agent with the original task prompt plus a summary of progress (branch state, last commit, test status). Stuck tasks — in-progress beyond a configurable threshold with no commits — are escalated to the manager.

### Board-Git Consistency

Board state and git state can diverge: tasks marked done with unmerged branches, in-progress tasks with no active agent, blocked tasks whose dependencies are already resolved. The `batty doctor` command validates cross-referencing board metadata against actual branch/worktree state and reports inconsistencies. The daemon automatically unblocks tasks whose `blocked_on` dependencies have reached done state.

## Stability & Predictability

The system must be deterministic and resilient across multi-hour autonomous runs. Three pillars:

### Testing Strategy

Unit tests cover individual functions. Integration tests cover cross-module workflows: daemon poll → intervention fires → message delivered → board state updated → orchestrator logged. The `tests/` directory houses integration tests that construct mock daemon/board state without tmux, exercising the full intervention→delivery→board pipeline.

Every intervention type (triage, owned-task, review, dispatch, utilization) has dedicated tests covering: trigger conditions, cooldown, signature deduplication, escalation, and ordering relative to other interventions.

### Error Model

Production code uses typed domain errors (`GitError`, `BoardError`, `TmuxError`, `DeliveryError`) instead of bare `unwrap()` or string-based `bail!()`. Shell-outs include full context: command, args, stderr, and what operation was attempted. The daemon poll loop isolates subsystem failures — a standup generation crash does not take down the daemon.

### Prompt-Sourced Nudges

Each role prompt may contain a `## Nudge` section with role-specific reminders. The daemon extracts this at startup and prepends it to system-generated intervention messages. This combines human-authored role guidance ("check the board for review items") with machine-generated context ("3 tasks waiting, eng-1-2 idle 5 min"). Nudge intervals are configurable per role via `nudge_interval_secs` in team.yaml.

## Non-Code Project Support

Batty was designed for git-tracked code projects but is also used for non-code workspaces (e.g., marketing campaigns, content teams). The system must gracefully handle the absence of git: skip worktree operations, skip merge/branch workflows, and avoid noisy warnings. The `use_worktrees: false` config flag is the gate — when set, all git-dependent daemon operations (worktree setup, branch detection, merge, cwd correction to worktree paths) must be bypassed entirely.

Additionally, non-role message sources (e.g., email routers, webhook bridges) need a way to inject messages into the team without being listed as full roles in team.yaml. The `external_senders` config list permits delivery from named sources that aren't part of the role hierarchy.

## Review Automation

The review queue is the primary throughput constraint. Manual review serializes the pipeline: engineers complete tasks faster than the manager can review and merge, creating idle time that scales with engineer count.

The review automation system adds a policy-driven auto-merge path alongside the existing manual review flow. When a completed task meets configurable criteria (tests pass, diff size below threshold, no conflicts, no sensitive files touched), the daemon merges it without waiting for manual review. Tasks that don't meet auto-merge criteria remain in the manual review queue, which gains time-based escalation to prevent stalls.

The key architectural constraint: auto-merge must be conservative by default. The system should never auto-merge something a human would reject. Better to route a safe change to manual review than to auto-merge a risky one. The confidence scoring layer evaluates diff characteristics beyond raw size — touching migrations, config files, or multiple modules reduces confidence regardless of line count.

Review feedback becomes structured data (disposition + specific comments stored in task frontmatter) rather than free-text chat messages. This makes rework cycles deterministic: the engineer receives exactly what needs to change, not a vague instruction.

## Key Design Decisions

**Why tmux?** Output capture (pipe-pane), input injection (send-keys/paste-buffer), status bar, panes, session persistence — all for free. No custom terminal code.

**Why YAML org chart?** One file defines the entire team topology. Easy to version, easy to change, easy to reason about.

**Why daemon?** Continuous background monitoring enables reactive behaviors (status tracking, message delivery, Telegram relay) without blocking the CLI.

**Why inbox-based messaging?** Decouples sender from receiver. Messages queue up and deliver when the target agent is ready. Prevents message loss during agent restarts.

**Why separate architect/manager/engineer?** Strategy, tactics, and execution are different skills. Splitting them prevents scope creep and evaluation bias. Each role has a focused prompt template.
