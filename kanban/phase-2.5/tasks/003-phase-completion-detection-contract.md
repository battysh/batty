---
id: 3
title: Phase completion detection contract
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T23:24:45.765827624-05:00
started: 2026-02-21T22:32:49.146404708-05:00
completed: 2026-02-21T23:24:45.765827003-05:00
tags:
    - core
    - reliability
claimed_by: oaken-south
claimed_at: 2026-02-21T23:24:45.765827564-05:00
class: standard
---

Define a deterministic rule for when Batty marks a phase run as complete.

## Contract

Completion requires all of the following:

1. All non-archived tasks in the phase board are `done`.
2. Exit criteria task (milestone) is `done`.
3. Phase summary artifact exists.
4. Required DoD/test command passes.
5. Executor process has reached a stable idle/completed state.

## Failure states

- Executor exits early with incomplete board.
- Tests fail after board is complete.
- Exit criteria task done but required artifacts missing.

## Notes

- Persist completion decision reason to execution log.
- This contract is reused by `batty work all`.

[[2026-02-21]] Sat 22:39
Picked accidentally; released while completing #2 first in dependency order.

## Statement of Work

- **What was done:** Implemented a deterministic completion contract evaluator and integrated it into the phase execution pipeline so phase runs are accepted/rejected from explicit gate checks.
- **Files created:** `src/completion.rs` - completion contract evaluation module (board/milestone/summary/DoD/executor-stability checks).
- **Files modified:** `src/work.rs` - wired completion evaluation + structured completion decision logging and run accept/reject behavior; `src/main.rs` - module wiring.
- **Key decisions:** Completion is only accepted when all contract checks pass; DoD command execution is gated to run only when structural completion conditions are already satisfied.
- **How to verify:** `cargo test -q completion::tests` and `cargo test -q` (full suite).
- **Open issues:** None identified during restore validation.
