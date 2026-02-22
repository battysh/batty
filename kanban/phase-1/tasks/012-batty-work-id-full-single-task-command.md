---
id: 12
title: batty work <phase> — full phase runner command
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
    - 11
class: standard
---

Wire it all together: read phase board → spawn agent in PTY with phase context → supervise session as agent works through tasks → test gates at checkpoints → ensure commits → log decisions → update kanban-md statuses. This is the product.

The agent manages task flow (picking, implementing, marking done). Batty manages the session (supervision, policy, tests, logging).

Exit criteria: `batty work phase-1` in your current terminal. Claude appears interactively and works through the phase board. Batty auto-answers routine prompts. Tests run at checkpoints. All tasks get statements of work. Execution log inspectable after.
