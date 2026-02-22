---
id: 5
title: Mock scenario matrix in real tmux
status: backlog
priority: critical
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - tmux
  - testing
class: standard
---

Run deterministic scenario matrix in real tmux with strict assertions.

## Required scenarios

1. Direct response injected.
2. Press-enter response injected as `<ENTER>` / empty input.
3. Escalation path does not inject.
4. Supervisor process failure does not inject.
5. Verbose/non-injectable response is rejected safely.

## Done When

- Matrix passes in `cargo test` without manual intervention.
