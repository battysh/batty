---
id: 5
title: Merge and conflict resolution
status: backlog
priority: critical
tags:
    - core
depends_on:
    - 3
class: standard
---

On merge approval from the review gate:
1. Merge phase branch to main
2. If conflicts: attempt guided resolution with diff context. If unresolved, escalate to human.
3. Post-merge: run tests on main to confirm nothing broke
4. Clean up worktree and branch
5. Update kanban board — all phase tasks marked done
