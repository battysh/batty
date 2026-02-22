---
id: 4
title: tmux pane invariants and persistence assertions
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:33.389790536-05:00
started: 2026-02-21T22:13:33.333992415-05:00
completed: 2026-02-21T22:13:33.389790195-05:00
tags:
    - tmux
    - ux
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:33.389790496-05:00
class: standard
---

Guarantee persistent interface semantics under supervisor activity.

## Requirements

1. Supervision must stay pinned to executor pane id.
2. Log pane must never become the supervision target.
3. Executor pane dead/alive handling must be explicit.
4. Tests assert expected pane count and pane roles.

## Done When

- Integration tests fail if pane targeting regresses.
- "recursive log self-supervision" regression is covered.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Added explicit tmux pane-role invariants and persistence assertions for supervision target pinning.
- **Files modified:** `src/orchestrator.rs` (supervision target pane event + invariant assertions), `src/tmux.rs` (pane detail introspection helper + tests).
- **Key decisions:** Emit supervision target pane id at runtime and assert target pane is never the `tail` log pane.
- **How to verify:** `cargo test orchestrator::tests::harness_direct_reply_injected_into_agent`
- **Open issues:** None for this task.
