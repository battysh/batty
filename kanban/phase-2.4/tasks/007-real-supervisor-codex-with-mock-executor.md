---
id: 7
title: Real supervisor (Codex) with mocked executor
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:46.938750862-05:00
started: 2026-02-21T22:13:46.892383581-05:00
completed: 2026-02-21T22:13:46.938750481-05:00
tags:
    - codex
    - integration
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:46.938750812-05:00
class: standard
---

Validate real Codex supervisor behavior through the tmux harness with deterministic mocked executor.

## Prompt scenarios

1. `Type exactly TOKEN_CODEX_123 and press Enter`
2. `Press enter to continue`

## Requirements

1. Run through real `codex exec` path (Tier 2 subprocess).
2. Assert injected input reaches mocked executor.
3. Verify supervisor trace events in orchestrator log.
4. Keep test opt-in via env flag for local/CI safety.

## Done When

- Opt-in integration test passes locally with authenticated Codex.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Updated real Codex supervisor integration harness to run named contract scenarios for both token echo and enter-only prompts against a mocked executor in tmux.
- **Files modified:** `src/orchestrator.rs` (real Codex harness scenario loop + assertions).
- **Key decisions:** Reused the same contract-driven harness path as Claude to keep behavior parity and easier drift detection.
- **How to verify:** `BATTY_TEST_REAL_CODEX=1 cargo test orchestrator::tests::harness_real_supervisor_codex_with_mock_executor -- --ignored --nocapture`
- **Open issues:** Requires authenticated Codex CLI and network access.
