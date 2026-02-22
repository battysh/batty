---
id: 8
title: Phase 3A exit criteria
status: backlog
priority: critical
tags:
    - milestone
depends_on:
    - 3
    - 4
    - 5
    - 6
    - 7
class: standard
---

Run `batty work all`. Batty picks phase-1, executor works through it, human review gate decides merge/rework, merge lands to main, and Batty continues to the next phase.

Rework loop works: reviewer can reject and executor can fix. Codex adapter path works inside the same flow.

The sequenced execution loop is operational end to end without requiring AI director automation.
