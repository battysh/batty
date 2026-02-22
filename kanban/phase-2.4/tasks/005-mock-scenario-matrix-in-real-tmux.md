---
id: 5
title: Mock scenario matrix in real tmux
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:33.45636278-05:00
started: 2026-02-21T22:13:33.413631191-05:00
completed: 2026-02-21T22:13:33.456362439-05:00
tags:
    - tmux
    - testing
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:33.45636273-05:00
class: standard
---

Run deterministic scenario matrix in real tmux with strict assertions.

## Required scenarios

1. Direct response injected.
2. Press-enter response injected as `<ENTER>` / empty input.
3. Escalation path does not inject.
4. Supervisor process failure does not inject.
5. Verbose/non-injectable response is rejected safely.

## Done When

- Matrix passes in `cargo test` without manual intervention.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Enforced a deterministic mock scenario matrix in real tmux and mapped tests to named contract scenarios for direct/enter/escalate/fail/verbose flows.
- **Files created:** `planning/supervision-harness-contract.toml` (scenario IDs used by tests).
- **Files modified:** `src/orchestrator.rs` (matrix-backed harness tests).
- **Key decisions:** Scenario IDs are now the source of truth to keep matrix coverage explicit and reviewable.
- **How to verify:** `cargo test orchestrator::tests::harness_`
- **Open issues:** None for this task.
