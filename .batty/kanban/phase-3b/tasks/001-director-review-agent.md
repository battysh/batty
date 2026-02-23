---
id: 1
title: Director review agent
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:51:45.35505583-05:00
started: 2026-02-22T15:47:01.816838044-05:00
completed: 2026-02-22T15:51:45.322232928-05:00
tags:
    - core
    - director
class: standard
---

On phase completion, call a director agent with structured context:

- `git diff main...phase-X-run-NNN`
- `phase-summary.md`
- statements of work
- execution log excerpts
- project standards docs

Director returns one structured decision:
- `merge`
- `rework` (with explicit feedback)
- `escalate` (with reason)

[[2026-02-22]] Sun 15:51
Implemented director review agent path with structured context (diff, summary, statements, log excerpt, standards docs), JSON decision parsing, and review-mode wiring (BATTY_REVIEW_MODE=director). Added unit tests in review module; targeted tests pass. Full cargo test is blocked in this sandbox by tmux/PTY permissions.
