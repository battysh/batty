---
id: 10
title: Expand docs/troubleshooting.md with real-world issues
status: done
priority: low
created: 2026-02-22T14:46:32.322316209-05:00
updated: 2026-02-22T14:57:57.210086131-05:00
started: 2026-02-22T14:57:13.64409887-05:00
completed: 2026-02-22T14:57:57.210085804-05:00
tags:
    - docs
    - troubleshooting
class: standard
---

## Problem

Troubleshooting guide has only 4 scenarios (44 lines). After 7 phases of development and 319+ tests, there are many more known failure modes.

## What to Add

Based on code review of error handling in the codebase:

1. **tmux version incompatibility** — Batty checks capabilities on startup (tmux.rs). Document the version matrix and what to do when capabilities are missing.
2. **kanban-md not found** — Common for new users. Document `batty install` auto-install and manual fallback.
3. **Supervisor not responding** — timeout_secs config, checking supervisor.program path, trace_io debugging.
4. **Worktree conflicts** — what happens when `--worktree` finds stale worktrees, how cleanup works.
5. **Dangerous mode warnings** — what the flags do, when to use them, security implications.
6. **Tier 2 snapshot inspection** — how to read `.batty/logs/<run>/tier2-context-<n>.md` files for debugging.
7. **Detection stuck in loop** — stuck detection behavior, what "nudge or escalate" means in practice.

## Acceptance Criteria

- At least 8-10 troubleshooting scenarios documented
- Each scenario has: symptoms, cause, fix
- References actual config keys and CLI flags

[[2026-02-22]] Sun 14:57
Expanded troubleshooting guide to 10 scenario-based entries (symptoms/cause/fix), covering tmux capability failures, kanban-md setup, supervisor timeout/trace settings, worktree reuse, dangerous mode, tier2-context snapshots, and stuck detector loops.
