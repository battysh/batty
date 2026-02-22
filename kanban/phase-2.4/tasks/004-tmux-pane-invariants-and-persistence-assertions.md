---
id: 4
title: tmux pane invariants and persistence assertions
status: backlog
priority: critical
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - tmux
  - ux
class: standard
---

Guarantee persistent interface semantics under supervisor activity.

## Requirements

1. Supervision must stay pinned to executor pane id.
2. Log pane must never become the supervision target.
3. Executor pane dead/alive handling must be explicit.
4. Tests assert expected pane count and pane roles.

## Done When

- Integration tests fail if pane targeting regresses.
- "recursive log self-supervision" regression is covered.
