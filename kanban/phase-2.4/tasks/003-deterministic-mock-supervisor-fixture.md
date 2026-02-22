---
id: 3
title: Deterministic mock supervisor fixture
status: backlog
priority: critical
created: 0001-01-01T00:00:00Z
updated: 0001-01-01T00:00:00Z
tags:
  - testing
  - supervisor
class: standard
---

Provide a deterministic supervisor fixture for controlled Tier 2 behavior.

## Required modes

1. `direct` → returns concise answer.
2. `enter` → returns "press enter" semantics.
3. `escalate` → returns `ESCALATE:` response.
4. `fail` → exits non-zero.
5. `verbose` → returns long prose (non-injectable).

## Done When

- Each mode has deterministic assertions in tmux harness tests.
