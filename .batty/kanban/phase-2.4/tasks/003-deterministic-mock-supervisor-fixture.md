---
id: 3
title: Deterministic mock supervisor fixture
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:33.312516162-05:00
started: 2026-02-21T22:13:33.265062699-05:00
completed: 2026-02-21T22:13:33.312515541-05:00
tags:
    - testing
    - supervisor
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:33.312516112-05:00
class: standard
---

Provide a deterministic supervisor fixture for controlled Tier 2 behavior.

## Required modes

1. `direct` → returns concise answer.
2. `enter` → returns "press enter" semantics.
3. `escalate` → returns `ESCALATE:` response.
4. `fail` → exits non-zero.
5. `verbose` → returns long prose (non-injectable).

## Done When

- Each mode has deterministic assertions in tmux harness tests.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Implemented deterministic mock supervisor fixture modes (`direct`, `enter`, `escalate`, `fail`, `verbose`) and assertions mapped to contract scenarios.
- **Files modified:** `src/orchestrator.rs` (mock supervisor script + per-mode scenario assertions).
- **Key decisions:** Use one fixture script with explicit mode switch to keep behavior deterministic and easy to extend.
- **How to verify:** `cargo test orchestrator::tests::harness_supervisor_`
- **Open issues:** None for this task.
