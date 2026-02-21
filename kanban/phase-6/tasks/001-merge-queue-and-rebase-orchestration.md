---
id: 1
title: Merge queue and rebase orchestration
status: backlog
priority: critical
created: 2026-02-21T18:41:28.997739764-05:00
updated: 2026-02-21T18:41:28.997739764-05:00
tags:
    - core
class: standard
---

Serialize merges when multiple tasks complete: merge one, rebase others on updated main, re-test, merge next. Handle rebase conflicts: pause task, re-run on fresh main, or escalate.
