---
id: 4
title: Rework loop
status: backlog
priority: critical
tags:
    - core
depends_on:
    - 3
class: standard
---

On rework decision: relaunch executor in the same worktree with reviewer feedback as additional context. Executor addresses issues, commits, produces updated summary. Loop back to review.

Max rework cycles configurable in .batty/config.toml. If exceeded, escalate to human.

In Phase 3A this feedback source is the human reviewer. The same loop later supports AI director feedback in Phase 3B.
