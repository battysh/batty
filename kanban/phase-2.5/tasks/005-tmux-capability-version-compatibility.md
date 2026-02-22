---
id: 5
title: tmux capability and version compatibility
status: backlog
priority: high
tags:
    - core
    - tmux
class: standard
---

tmux behaviors differ by version. Batty should probe capabilities and use compatible command paths.

## Requirements

1. Detect tmux version on startup and log it.
2. Probe required capabilities:
   - `pipe-pane` behavior
   - status bar formatting options used by Batty
   - pane split behavior used for orchestrator log pane
3. Provide compatibility matrix in docs with known-good version range.
4. Fail fast with clear remediation when required capabilities are missing.
5. Use fallbacks when possible instead of hard failure.

## Deliverables

- Capability probe module and tests.
- User-facing compatibility section in docs.
