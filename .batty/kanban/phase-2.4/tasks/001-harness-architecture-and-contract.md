---
id: 1
title: Harness architecture and contract
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:19.251940194-05:00
started: 2026-02-21T22:04:26.706957433-05:00
completed: 2026-02-21T22:13:19.251939583-05:00
tags:
    - testing
    - harness
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:19.251940144-05:00
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

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Added a machine-readable supervision harness contract and wired harness tests to load named scenarios from it.
- **Files created:** `planning/supervision-harness-contract.toml` (scenario matrix + failure taxonomy).
- **Files modified:** `src/orchestrator.rs` (contract loader + scenario-mapped harness tests), `kanban/phase-2.4/PHASE.md` (reference link).
- **Key decisions:** Keep contract in TOML so it is human-readable and directly parsed by tests for regression coverage.
- **How to verify:** `cargo test orchestrator::tests::harness_contract_is_machine_readable_and_complete`
- **Open issues:** None for this task.
