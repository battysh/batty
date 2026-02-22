---
id: 8
title: Real executor+supervisor smoke (Claude pair)
status: done
priority: medium
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:47.006813901-05:00
started: 2026-02-21T22:13:46.962510538-05:00
completed: 2026-02-21T22:13:47.00681349-05:00
tags:
    - claude
    - smoke
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:47.006813861-05:00
class: standard
---

Run real executor + real supervisor in tmux as a smoke gate.

## Requirements

1. Use real Claude executor command in tmux.
2. Use real Claude supervisor command in Tier 2.
3. Assert orchestration lifecycle signals (`created`, `supervising`, `completed`).
4. Keep test opt-in via env flag.

## Done When

- Smoke test is runnable and stable enough for manual pre-release gating.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Strengthened real Claude executor+supervisor smoke assertions to require lifecycle signals from contract (`created`, `supervising`, `executor exited`).
- **Files modified:** `src/orchestrator.rs` (smoke assertion helper + Claude smoke contract mapping).
- **Key decisions:** Lifecycle expectations are now contract-driven so release-gate signals are explicit and shared.
- **How to verify:** `BATTY_TEST_REAL_E2E_CLAUDE=1 cargo test orchestrator::tests::harness_real_executor_and_supervisor_claude_smoke -- --ignored --nocapture`
- **Open issues:** Opt-in manual smoke requires authenticated real agent runtime.
