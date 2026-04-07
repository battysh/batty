# Batty Architecture

## Overview

Batty v0.10.0 is a daemon-owned workflow system for hierarchical coding teams.
Humans set the goal, the architect and manager shape the work, engineers execute
in isolated worktrees, and the daemon keeps the board, verification loop, and
merge path moving.

```text
User (CLI / Telegram)
        |
        v
Architect
        |
        v
Manager
        |
        v
Engineers
        |
        v
Worktree -> Verification -> Review / Auto-merge -> main
```

The runtime surface is intentionally file-oriented: YAML for config, Markdown for
board state, JSON/JSONL for logs, SQLite for telemetry, and git worktrees for
execution isolation.

## Design Goals

- Keep the happy path unattended.
- Prefer explicit workflow state over inferring truth from chat.
- Recover from common failures automatically before involving a human.
- Keep every state transition inspectable from files or CLI commands.

## Control Plane

The control plane starts with `team.yaml`, which defines:

- role hierarchy
- layout zones
- board behavior
- verification and auto-merge policy
- automation and recovery thresholds
- agent/backend selection per role

At startup, Batty validates the config, resolves instances, creates the tmux
layout, initializes inboxes and board paths, and launches the background daemon.

## Execution Plane

Each agent runs behind the shim runtime when `use_shim: true`:

- the shim owns the subprocess and PTY
- tmux tails shim logs for visibility
- SDK-capable backends use typed protocols instead of pane scraping
- health checks, shutdown, and restart are handled through structured commands

Supported SDK modes today:

- Claude Code via stream-json NDJSON
- Codex CLI via JSONL event streams
- Kiro CLI via ACP JSON-RPC 2.0

When SDK mode is off, the shim still owns the PTY and uses screen
classification as the fallback runtime.

## Workflow Plane

The board is the source of truth for task state. Batty treats workflow metadata
as first-class control data instead of burying it inside chat transcripts.

```text
backlog -> todo -> in-progress -> review -> done
```

The daemon handles:

- auto-dispatch from `todo` to idle engineers
- dependency-aware task readiness
- claim TTL expiry and reclaim
- review nudges and escalation
- board reconciliation after restarts
- archive and replenishment behavior

This is the main architectural shift in v0.10.0: work progression no longer
depends on a human or manager remembering every transition.

## Verification And Merge

Completion is not the end of the task. Batty keeps a verification loop around
completion handling:

1. collect completion evidence
2. run configured test commands
3. route the task to review or auto-merge
4. verify the merge result when policy requires it

`workflow_policy.verification` defines retry behavior. `workflow_policy.auto_merge`
defines the safe unattended envelope using diff size, touched files/modules,
confidence score, and test requirements.

## Recovery Systems

The daemon continuously watches for operational drift:

- shim crash / missing pong
- false-working stalls
- pending message delivery failures
- stale worktree branches
- review queue buildup
- idle managers with runnable work
- underutilized teams that need replenishment or redistribution

These recovery paths are configured in the `automation` block and are expected
to stay on in unattended teams.

## Worktree Model

Engineers use stable worktree directories under `.batty/worktrees/<engineer>`.
Each assignment creates a fresh task branch inside that stable directory. Batty
also supports shared build targets so multiple Rust worktrees can compile in
parallel without lock contention or duplicated artifact caches.

## Observability

Batty exposes its state through several layers:

- tmux panes for live operator visibility
- `.batty/daemon.log` and `events.jsonl` for runtime diagnosis
- SQLite telemetry queried by `batty telemetry`
- `batty metrics` for a consolidated throughput view
- Grafana dashboards and alerts for long-running sessions

## Component Map

| Component | Responsibility |
| --- | --- |
| `src/cli.rs` | Command surface and flags |
| `src/team/config/` | `team.yaml` parsing and defaults |
| `src/team/daemon/` | Poll loop, health checks, interventions, merge queue |
| `src/team/dispatch/` | Readiness checks, queueing, WIP and stabilization |
| `src/team/merge/` | Merge operations and locking |
| `src/team/verification.rs` | Completion verification policy |
| `src/team/telemetry_db.rs` | SQLite telemetry persistence |
| `src/shim/` | Agent subprocess runtime and protocol bridges |
| `src/worktree.rs` | Worktree lifecycle and hygiene |

## Current Architectural Bet

The core bet for v0.10.0 is that Batty should behave like a resilient factory:
the daemon owns coordination, agents stay bounded to role responsibilities, and
humans intervene for policy or ambiguity, not for routine state transitions.
