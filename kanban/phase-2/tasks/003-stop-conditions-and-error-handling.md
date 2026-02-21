---
id: 3
title: Stop conditions and error handling
status: backlog
priority: high
created: 2026-02-21T18:40:34.014259265-05:00
updated: 2026-02-21T18:40:34.014259265-05:00
tags:
    - core
class: standard
---

Configurable stop conditions: max consecutive failures, max total tasks, user interrupt (Ctrl-C graceful shutdown). On repeated failure: skip task, log reason, move to next. On user interrupt: finish current task or abort cleanly.
