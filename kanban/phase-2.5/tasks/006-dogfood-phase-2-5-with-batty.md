---
id: 6
title: Dogfood phase-2.5 with Batty
status: backlog
priority: high
tags:
    - milestone
depends_on:
    - 1
    - 2
    - 3
    - 4
    - 5
    - 7
    - 8
    - 9
    - 10
class: standard
---

Use Batty itself to execute the `phase-2.5` board end-to-end.

## Success criteria

1. Run `batty work phase-2.5`.
2. Complete tasks with supervision active.
3. Produce phase summary and review packet.
4. Merge to main via review gate.
5. Capture short postmortem: what broke, what was manual, what to automate next.

This is the hard gate before we claim Batty is ready for sequenced multi-phase execution.
