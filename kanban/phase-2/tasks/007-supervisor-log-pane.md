---
id: 7
title: Batty orchestrator log pane
status: done
priority: critical
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:43:03.398134436-05:00
started: 2026-02-21T20:38:26.91373698-05:00
completed: 2026-02-21T20:43:03.398134035-05:00
tags:
    - ux
    - tmux
    - core
depends_on:
    - 1
    - 2
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:43:03.398134386-05:00
class: standard
---

Dedicated tmux pane showing all batty orchestration messages in real time. This is a core feature — it's how the human knows what batty is doing. Without it, you're just staring at the executor with no visibility into supervision.

## Layout

```
┌──────────────────────────────────────────┐
│  Executor pane (~80% height)             │
│  Claude Code / Codex / Aider             │
│  Human types here                        │
├──────────────────────────────────────────┤
│  Batty orchestrator pane (~20% height)   │
│  [batty] ● phase-1 started — 11 tasks   │
│  [batty] → task #3 claimed by executor   │
│  [batty] ✓ auto-answered: "Continue?" → y│
│  [batty] ? supervisor thinking...        │
│  [batty] ✓ supervisor answered → async   │
│  [batty] → task #3 done, picking #4     │
└──────────────────────────────────────────┘
```

## What goes in the orchestrator pane

- Phase start/end, task transitions
- Every auto-answer (Tier 1) — what was asked, what was answered
- Every supervisor call (Tier 2) — question, thinking indicator, answer
- Stuck detection alerts
- Human escalation requests (`⚠ NEEDS INPUT`)
- Test runs and results
- Commit confirmations
- Errors and warnings

## Implementation

- On session start: `tmux split-window -v -p 20 -t batty-phase-1` to create the pane.
- Batty appends lines to `.batty/logs/orchestrator.log`.
- The pane runs `tail -f .batty/logs/orchestrator.log` — simple, read-only by nature.
- No send-keys complexity for the log pane itself.
- Pane height configurable: `orchestrator_pane_height_pct = 20` (default) in `.batty/config.toml`.
- Can be disabled with `orchestrator_pane = false` for minimal setups (status bar only).

## Edge cases

- Terminal too small (< 15 rows) → skip the pane, status bar only.
- User closes the pane manually → detect and don't crash. Status bar remains.
- User scrolls the orchestrator pane → tmux handles scroll natively (Ctrl-b [).

[[2026-02-21]] Sat 20:42
Statement of work:
- Added log_pane (bool) and log_pane_height_pct (u32) to OrchestratorConfig
- Created setup_log_pane(): splits tmux window vertically with 'tail -f orchestrator.log'
- Uses -l (lines) instead of -p (percentage) for tmux compatibility in detached sessions
- Integrated into run() between pipe-pane setup and supervision loop
- 2 new tests: log_pane_setup (unit), orchestrator_with_log_pane (integration)
- Used unique session names per test to avoid parallel test interference
- How to verify: cargo test -- log_pane (both tests pass)
