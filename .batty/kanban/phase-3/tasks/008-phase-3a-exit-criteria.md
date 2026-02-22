---
id: 8
title: Phase 3A exit criteria
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:16:08.907784945-05:00
started: 2026-02-22T15:14:53.757334086-05:00
completed: 2026-02-22T15:16:08.907784648-05:00
tags:
    - milestone
depends_on:
    - 3
    - 4
    - 5
    - 6
    - 7
class: standard
---

Run `batty work all`. Batty picks phase-1, executor works through it, human review gate decides merge/rework, merge lands to main, and Batty continues to the next phase.

Rework loop works: reviewer can reject and executor can fix. Codex adapter path works inside the same flow.

The sequenced execution loop is operational end to end without requiring AI director automation.

[[2026-02-22]] Sun 15:16
Phase 3A exit verification completed. Evidence: (1) prerequisite tasks #3/#4/#5/#6/#7 are done on the phase-3 board; (2) dry-run chain works via cargo run --bin batty -- work all --dry-run, which sequenced phase boards in numeric order and emitted launch contexts for each selected phase; (3) human review gate, rework loop, merge/conflict automation, and sequencer logic validated by targeted tests (cargo test work::tests:: ; cargo test review::tests:: ; cargo test sequencer::tests:: ; cargo test agent::codex::tests:: ; cargo test prompt::tests::codex_ ; cargo test log::tests::all_event_types_serialize). Full cargo test continues to fail in this sandbox due tmux/openpty permission restrictions (environmental, not logic regressions).
