---
id: 3
title: Phase 4 exit criteria
status: backlog
priority: critical
tags:
    - milestone
depends_on:
    - 1
    - 2
class: standard
---

`batty work all --parallel 3` runs three phases in three tmux windows. Each in its own worktree. Each with its own supervisor. Merges land cleanly on main in order. Conflict recovery works.
