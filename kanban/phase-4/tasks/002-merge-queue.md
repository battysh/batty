---
id: 2
title: Merge queue for parallel phases
status: backlog
priority: critical
tags:
    - core
depends_on:
    - 1
class: standard
---

Serialize merges from parallel phases: merge one, rebase others on updated main, re-test.

- Conflict recovery: if rebase fails, pause phase, re-run on fresh main.
- Resource awareness: limit parallel agents based on config.
- Merge ordering: first to complete merges first.
