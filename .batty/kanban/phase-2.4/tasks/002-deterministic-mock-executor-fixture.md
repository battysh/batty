---
id: 2
title: Deterministic mock executor fixture
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:33.242279023-05:00
started: 2026-02-21T22:13:33.196223243-05:00
completed: 2026-02-21T22:13:33.242278642-05:00
tags:
    - testing
    - executor
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:33.242278973-05:00
class: standard
---

Provide a deterministic executor fixture used inside real tmux sessions.

## Requirements

1. Emits controlled prompt lines (configurable per test).
2. Waits for injected input with timeout.
3. Persists received input for assertion.
4. Exits with deterministic status code.

## Done When

- Harness tests can assert exact input received by executor.
- Timeouts are deterministic and do not hang CI/local runs.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Implemented deterministic mock executor fixture behavior in tmux harness tests (controlled prompt line, deterministic read timeout, persisted received input, deterministic exit).
- **Files modified:** `src/orchestrator.rs` (harness agent script + harness runner assertions).
- **Key decisions:** Keep fixture script generated per test tempdir so test runs remain isolated and deterministic.
- **How to verify:** `cargo test orchestrator::tests::harness_direct_reply_injected_into_agent`
- **Open issues:** None for this task.
