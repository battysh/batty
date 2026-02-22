---
id: 6
title: Real supervisor (Claude) with mocked executor
status: backlog
priority: high
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - claude
  - integration
class: standard
---

Validate real Claude supervisor behavior through the tmux harness with deterministic mocked executor.

## Prompt scenarios

1. `Type exactly TOKEN_CLAUDE_123 and press Enter`
2. `Press enter to continue`

## Requirements

1. Run through real `claude -p` path (Tier 2 subprocess).
2. Assert injected input reaches mocked executor.
3. Verify supervisor trace events in orchestrator log.
4. Keep test opt-in via env flag for local/CI safety.

## Done When

- Opt-in integration test passes locally with authenticated Claude.
