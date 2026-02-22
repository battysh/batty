---
id: 2
title: Deterministic mock executor fixture
status: backlog
priority: critical
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - testing
  - executor
class: standard
---

Provide a deterministic executor fixture used inside real tmux sessions.

## Requirements

1. Emits controlled prompt lines (configurable per test).
2. Waits for injected input with timeout.
3. Persists received input for assertion.
4. Exits with deterministic status code.

## Done When

- Harness tests can assert exact input received by executor.
- Timeouts are deterministic and do not hang CI/local runs.
