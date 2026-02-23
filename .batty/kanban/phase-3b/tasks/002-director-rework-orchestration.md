---
id: 2
title: Director rework orchestration
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:52:15.371381864-05:00
started: 2026-02-22T15:51:49.458717345-05:00
completed: 2026-02-22T15:52:15.337595923-05:00
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

[[2026-02-22]] Sun 15:52
Director  now flows through existing rework orchestration in : relaunches same worktree, appends reviewer feedback into launch context (), increments attempt, appends to same execution log, and escalates when  is exceeded. Existing tests for rework-context prompt and retry handling remain valid; director decision parsing for rework added in review tests.

[[2026-02-22]] Sun 15:52
Correction: director rework now flows through existing run_phase_with_rework orchestration: it relaunches in the same worktree, injects reviewer feedback via ReworkContext, increments attempt counters, appends to the same execution log, and escalates when max_retries is exceeded. Added director rework decision parsing tests in review module.
