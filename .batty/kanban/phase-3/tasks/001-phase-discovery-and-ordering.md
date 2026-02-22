---
id: 1
title: Phase discovery and ordering
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-22T15:00:55.468103072-05:00
started: 2026-02-22T14:55:57.2057851-05:00
completed: 2026-02-22T15:00:55.468102755-05:00
tags:
    - core
class: standard
---

Build deterministic phase sequencing for `batty work all`.

## Requirements

1. Discover phase directories under `kanban/`.
2. Sort by numeric phase order (`phase-1`, `phase-2`, `phase-2.4`, `phase-2.5`, `phase-3`, ...).
3. Skip phases already complete.
4. Stop on first failed/escalated phase unless policy says continue.
5. Log phase selection decisions for auditability.

[[2026-02-22]] Sun 14:56
Reviewed current CLI/work modules; implementing reusable phase discovery + deterministic numeric ordering + completion skipping + sequencing stop policy primitives with unit tests.

[[2026-02-22]] Sun 15:00
Implemented deterministic phase discovery primitives in src/sequencer.rs: numeric phase parsing/sorting, complete-phase skipping, fail-fast stop policy helper, and selection-decision logging helper. Added new structured log event (phase_selection_decision). Validation: cargo test sequencer::tests:: ; cargo test log::tests::all_event_types_serialize. Full cargo test still fails in this sandbox due tmux/openpty permission restrictions (existing environment limitation).
