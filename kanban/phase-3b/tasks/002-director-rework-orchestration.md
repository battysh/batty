---
id: 2
title: Director rework orchestration
status: backlog
priority: critical
tags:
    - core
    - director
depends_on:
    - 1
class: standard
---

If the director returns `rework`, relaunch executor in the same worktree with director feedback appended to context.

Requirements:

1. Preserve previous summary and logs.
2. Track retry count in structured state.
3. Escalate to human when max retries are exceeded.
