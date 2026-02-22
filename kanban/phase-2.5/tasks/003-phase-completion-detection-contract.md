---
id: 3
title: Phase completion detection contract
status: backlog
priority: critical
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
