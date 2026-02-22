---
id: 3
title: Phase completion detection contract
status: backlog
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:39:55.559960178-05:00
started: 2026-02-21T22:32:49.146404708-05:00
tags:
    - core
    - reliability
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
