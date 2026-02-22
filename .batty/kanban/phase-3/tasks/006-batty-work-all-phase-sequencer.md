---
id: 6
title: "batty work all — phase sequencer"
status: backlog
priority: high
tags:
    - core
depends_on:
    - 1
    - 2
    - 3
    - 4
    - 5
class: standard
---

Loop: find next incomplete phase → `batty work <phase>` → review gate → merge → next phase. Phase ordering follows directory structure (phase-1, phase-2, ...). A phase is complete when all tasks are done.

Stop conditions: all phases complete, user Ctrl-C, error threshold. If a phase fails review repeatedly, pause and report to human.
