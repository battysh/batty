---
id: 4
title: 'Tier 1: regex auto-answer via send-keys'
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:34:39.185628408-05:00
started: 2026-02-21T20:32:26.017622304-05:00
completed: 2026-02-21T20:34:39.185627826-05:00
tags:
    - core
depends_on:
    - 1
    - 3
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:34:39.185628357-05:00
class: standard
---

Pattern-match detected prompts against known patterns from `.batty/config.toml`. Instant response via `tmux send-keys`, no agent needed.

## How it works

1. Prompt detector (task #3) fires with a detected prompt.
2. Check prompt against Tier 1 patterns (regex → response mapping).
3. Match found → `tmux send-keys -t batty-phase-1 "<response>" Enter`
4. Log the auto-answer to execution log and supervisor log pane.

## Patterns handled

- `"Continue? [y/n]"` → `y`
- `"Do you want to proceed?"` → `yes`
- Tool approval patterns (per-agent) → approve per policy
- Configurable pattern → response mapping in `.batty/config.toml`

Handles ~70-80% of prompts with zero latency and zero cost.

## Implementation

- Reuse Phase 1's `PromptPatterns` from `src/prompt/mod.rs`.
- Use `src/tmux.rs` `send_keys()` for injection.
- All auto-answers logged to execution log (`src/log/mod.rs`).
- This replaces Phase 1's auto-answer logging-only behavior with actual injection.

[[2026-02-21]] Sat 20:34
Statement of work:
- Created src/orchestrator.rs: tmux-based supervision loop with Tier 1 auto-answer
- OrchestratorConfig: spawn config, patterns, policy, detector config, phase, project root
- run(): creates tmux session, sets up pipe-pane, polls for output, detects prompts, auto-answers via send-keys
- handle_prompt(): evaluates policy decisions (Act→send-keys, Suggest→log, Escalate→alert, Observe→log)
- OrchestratorObserver trait: callback interface for logging/status (on_auto_answer, on_escalate, on_suggest, on_event)
- LogFileObserver: writes to orchestrator.log file
- 7 unit tests covering auto-answer injection, escalation, completion skip, session lifecycle, stop signal
- This replaces the Phase 1 supervisor loop with tmux-based supervision
