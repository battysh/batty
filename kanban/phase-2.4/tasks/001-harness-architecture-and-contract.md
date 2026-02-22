---
id: 1
title: Harness architecture and contract
status: in-progress
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:04:26.706957774-05:00
started: 2026-02-21T22:04:26.706957433-05:00
tags:
    - testing
    - harness
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:04:26.706957183-05:00
class: standard
---

Define the supervisor-harness integration contract before adding more runtime hardening.

## Requirements

1. Define scenario matrix inputs and expected outputs.
2. Define assertions per layer:
   - detector event
   - supervisor call interface
   - injected terminal input
   - tmux pane state invariants
3. Define failure taxonomy (regression class + triage owner).
4. Keep contract machine-checkable and human-readable.

## Done When

- A single reference document exists in repo and is used by tests.
- Every integration test case maps to a named scenario in the matrix.
