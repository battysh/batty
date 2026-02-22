---
id: 2
title: Prompt composition and context injection
status: backlog
priority: high
tags:
    - core
    - launch
class: standard
---

Define and implement deterministic executor launch context.

## Requirements

1. Compose launch prompt from:
   - `CLAUDE.md` or active agent instructions
   - `kanban/<phase>/PHASE.md`
   - current board state (task titles/status/dependencies)
   - `.batty/config.toml` policy and execution defaults
2. Persist composed prompt/input snapshot to logs for auditability.
3. Validate required context files and fail with actionable errors.
4. Support agent-specific prompt wrappers via adapter layer.

## Exit signal for this task

- A dry-run mode shows the exact composed launch context before execution.
