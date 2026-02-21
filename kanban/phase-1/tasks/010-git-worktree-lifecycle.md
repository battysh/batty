---
id: 10
title: Git worktree lifecycle
status: backlog
priority: high
created: 2026-02-21T18:40:23.066064424-05:00
updated: 2026-02-21T18:40:23.066064424-05:00
tags:
    - core
depends_on:
    - 7
class: standard
---

Create worktree + branch on task start. Agent works in isolated worktree. On DoD pass: commit in worktree, rebase on main, run tests again post-rebase, merge to main. Clean up worktree and branch. On rebase conflict: report to user, don't force.
