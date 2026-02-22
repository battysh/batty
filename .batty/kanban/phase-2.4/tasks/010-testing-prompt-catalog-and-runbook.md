---
id: 10
title: Testing prompt catalog and runbook
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:54.145543739-05:00
started: 2026-02-21T22:13:54.101454486-05:00
completed: 2026-02-21T22:13:54.145543378-05:00
tags:
    - docs
    - testing
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:54.145543689-05:00
class: standard
---

Publish prompt catalog and execution runbook for harness validation.

## Required prompt catalog

1. **Token echo:** `Type exactly TOKEN_<AGENT>_123 and press Enter`
2. **Enter-only:** `Press enter to continue`
3. **Escalation ambiguity:** `Choose between A/B without context`
4. **Non-injectable stress:** long-paragraph supervisor response

## Runbook requirements

1. How to run deterministic tmux harness suite.
2. How to run opt-in real supervisor tests.
3. How to run opt-in real executor+supervisor smoke tests.
4. What logs to inspect and expected pass/fail signals.

## Done When

- Runbook is in repo and linked from phase docs.
- Team can execute validation process repeatably.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Published prompt catalog and execution runbook for deterministic harness, opt-in real supervisor tests, and opt-in real smoke tests.
- **Files created:** `planning/phase-2.4-harness-runbook.md` (catalog + commands + expected signals).
- **Files modified:** `kanban/phase-2.4/PHASE.md` (links to contract and runbook).
- **Key decisions:** Keep the runbook phase-scoped and reference the same contract file used by tests to avoid drift between docs and automation.
- **How to verify:** Read `planning/phase-2.4-harness-runbook.md` and execute listed `cargo test` commands.
- **Open issues:** Real-agent sections remain opt-in and environment-dependent by design.
