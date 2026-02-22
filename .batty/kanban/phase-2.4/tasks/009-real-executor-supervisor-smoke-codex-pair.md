---
id: 9
title: Real executor+supervisor smoke (Codex pair)
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:47.070790971-05:00
started: 2026-02-21T22:13:47.028793191-05:00
completed: 2026-02-21T22:13:47.0707906-05:00
tags:
    - codex
    - smoke
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:47.070790921-05:00
class: standard
---

Run real executor + real supervisor in tmux as a smoke gate.

## Requirements

1. Use real Codex executor command in tmux.
2. Use real Codex supervisor command in Tier 2.
3. Assert orchestration lifecycle signals (`created`, `supervising`, `completed`).
4. Keep test opt-in via env flag.

## Done When

- Smoke test is runnable and stable enough for manual pre-release gating.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Strengthened real Codex executor+supervisor smoke assertions to require lifecycle signals from contract (`created`, `supervising`, `executor exited`).
- **Files modified:** `src/orchestrator.rs` (Codex smoke contract mapping + lifecycle checks).
- **Key decisions:** Keep smoke gate parity between Claude and Codex through shared contract shape.
- **How to verify:** `BATTY_TEST_REAL_E2E_CODEX=1 cargo test orchestrator::tests::harness_real_executor_and_supervisor_codex_smoke -- --ignored --nocapture`
- **Open issues:** Opt-in manual smoke requires authenticated real agent runtime.
