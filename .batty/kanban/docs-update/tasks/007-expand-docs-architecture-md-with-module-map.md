---
id: 7
title: Expand docs/architecture.md with module map
status: done
priority: medium
created: 2026-02-22T14:46:12.581003779-05:00
updated: 2026-02-22T14:55:25.764775847-05:00
started: 2026-02-22T14:54:51.985347584-05:00
completed: 2026-02-22T14:55:25.76477553-05:00
tags:
    - docs
    - architecture
class: standard
---

## Problem

docs/architecture.md is only 18 lines — just a brief overview and two links. For a project with 14K+ lines of Rust across 16+ modules, this should be the go-to reference for understanding the codebase.

## What to Add

1. **Module responsibility map** — table or list of each src/ module with purpose and key exports:
   - orchestrator.rs (3,896 lines) — core tmux supervision loop
   - work.rs (1,611 lines) — work command pipeline
   - tmux.rs (1,127 lines) — tmux CLI wrapper
   - events.rs (896 lines) — event buffer & pipe watcher
   - detector.rs (526 lines) — prompt detection state machine
   - tier2.rs (423 lines) — supervisor agent integration
   - completion.rs (357 lines) — phase completion detection
   - agent/ — AgentAdapter trait, Claude & Codex implementations
   - config/ — TOML config parsing
   - policy/ — policy engine (observe/suggest/act)
   - prompt/ — prompt pattern definitions
   - log/ — JSON lines execution logging
   - task/ — kanban-md board parsing
   - worktree.rs — git worktree management
   - install.rs — asset installation
   - dod/ — definition of done gates

2. **Data flow diagram** — how output flows from executor → pipe-pane → events → detector → policy → send-keys

3. **Two-tier prompt handling explanation** — Tier 1 (regex) vs Tier 2 (supervisor agent) with when each triggers

## Acceptance Criteria

- Module map covers all src/ modules with purpose
- Data flow is clear for a new contributor
- Replaces or significantly expands the current 18-line stub

[[2026-02-22]] Sun 14:55
Replaced docs/architecture.md stub with runtime data-flow diagram (executor -> pipe-pane -> events -> detector -> tier actions -> send-keys), Tier 1 vs Tier 2 behavior details, and full src/ module responsibility table.
