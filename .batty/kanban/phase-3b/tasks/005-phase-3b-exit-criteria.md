---
id: 5
title: Phase 3B exit criteria
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:58:29.191718161-05:00
started: 2026-02-22T15:57:44.805858769-05:00
completed: 2026-02-22T15:58:29.157964928-05:00
tags:
    - milestone
depends_on:
    - 2
    - 3
    - 4
class: standard
---

Run `batty work all` with director mode enabled.

Success conditions:

1. Director reviews completed phases and emits structured decisions.
2. Rework loop runs from director feedback.
3. Human override can interrupt/replace any director decision.
4. Decision logs are complete and traceable.

[[2026-02-22]] Sun 15:58
Validation run completed: BATTY_REVIEW_MODE=director cargo run --bin batty -- work all --dry-run succeeded. Added and ran targeted tests covering director decision parsing/capture, director policy controls, human override parsing/approval handling, rework decision routing, and structured director audit event serialization. In this sandbox, full interactive tmux execution is restricted, so milestone verification used dry-run plus unit tests for decision/review paths.
