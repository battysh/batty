---
id: 6
title: Real supervisor (Claude) with mocked executor
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T22:13:46.868879238-05:00
started: 2026-02-21T22:13:46.826422025-05:00
completed: 2026-02-21T22:13:46.868878868-05:00
tags:
    - claude
    - integration
claimed_by: vine-fawn
claimed_at: 2026-02-21T22:13:46.868879188-05:00
class: standard
---

Validate real Claude supervisor behavior through the tmux harness with deterministic mocked executor.

## Prompt scenarios

1. `Type exactly TOKEN_CLAUDE_123 and press Enter`
2. `Press enter to continue`

## Requirements

1. Run through real `claude -p` path (Tier 2 subprocess).
2. Assert injected input reaches mocked executor.
3. Verify supervisor trace events in orchestrator log.
4. Keep test opt-in via env flag for local/CI safety.

## Done When

- Opt-in integration test passes locally with authenticated Claude.

[[2026-02-21]] Sat 22:13
## Statement of Work

- **What was done:** Updated real Claude supervisor integration harness to run named contract scenarios for both token echo and enter-only prompts against a mocked executor in tmux.
- **Files modified:** `src/orchestrator.rs` (real Claude harness scenario loop + assertions).
- **Key decisions:** Scenario definitions (prompt + timeout + expectations) come from contract TOML for deterministic mapping.
- **How to verify:** `BATTY_TEST_REAL_CLAUDE=1 cargo test orchestrator::tests::harness_real_supervisor_claude_with_mock_executor -- --ignored --nocapture`
- **Open issues:** Requires authenticated Claude CLI and network access.
