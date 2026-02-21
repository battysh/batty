---
id: 12
title: batty work <id> — full single-task command
status: backlog
priority: critical
created: 2026-02-21T18:40:23.111209121-05:00
updated: 2026-02-21T18:40:23.111209121-05:00
tags:
    - core
    - milestone
depends_on:
    - 3
    - 7
    - 8
    - 9
    - 10
    - 11
class: standard
---

Wire it all together: read task → worktree → spawn agent in PTY → supervise → test gate → merge → update kanban-md status. This is the product.

Exit criteria: batty work 3 in your current terminal. Claude appears interactively. Batty auto-answers routine prompts. Tests run on completion. On pass, worktree merges to main. Execution log inspectable after.
