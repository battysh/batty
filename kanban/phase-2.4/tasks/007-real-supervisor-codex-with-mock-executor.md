---
id: 7
title: Real supervisor (Codex) with mocked executor
status: backlog
priority: high
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - codex
  - integration
class: standard
---

Validate real Codex supervisor behavior through the tmux harness with deterministic mocked executor.

## Prompt scenarios

1. `Type exactly TOKEN_CODEX_123 and press Enter`
2. `Press enter to continue`

## Requirements

1. Run through real `codex exec` path (Tier 2 subprocess).
2. Assert injected input reaches mocked executor.
3. Verify supervisor trace events in orchestrator log.
4. Keep test opt-in via env flag for local/CI safety.

## Done When

- Opt-in integration test passes locally with authenticated Codex.
