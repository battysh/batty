---
id: 1
title: tmux multi-window parallel execution
status: backlog
priority: critical
tags:
    - core
    - tmux
class: standard
---

Each parallel phase gets its own tmux window (with executor pane + supervisor log pane). `batty work all --parallel N` runs N phases simultaneously.

- Each window: executor pane + supervisor log pane + status bar showing that window's phase.
- Git worktree per parallel phase — each in its own isolated branch.
- tmux window switching to monitor multiple phases.
- `batty status` shows overview of all running phases.
