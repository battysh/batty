---
id: 6
title: tmux status bar integration
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:46:20.035438743-05:00
started: 2026-02-21T20:43:14.21135567-05:00
completed: 2026-02-21T20:46:20.035438262-05:00
tags:
    - ux
    - tmux
depends_on:
    - 1
    - 2
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:46:20.035438683-05:00
class: standard
---

Show supervisor state in the tmux status bar. Updated on every supervisor event.

## Status bar content

```
[batty] phase-1 | task #7 — PTY supervision | 6/11 done | ✓ supervising
```

Components:
- Phase name
- Current task (ID + title)
- Progress (done/total)
- Supervisor state indicator

## Status indicators

- `●` state change (phase/task start/end)
- `→` action taken (task claimed, answer injected)
- `✓` normal operation (supervising, task completed)
- `?` supervisor thinking (Tier 2 call in progress)
- `⚠` needs human input — LOOK AT THIS
- `✗` failure (test fail, error, stuck)

## Terminal title

Also set terminal title via tmux: `tmux set -t batty-phase-1 set-titles-string "[batty] phase-1 | task #7 | 6/11"`.

Shows in the tab/title bar. Persistent. Costs nothing.

## Implementation

- `tmux set -t <session> status-left "<content>"` for left side.
- `tmux set -t <session> status-right "<content>"` for right side (timestamps, etc.).
- `tmux set -t <session> status-style "bg=colour235,fg=colour136"` for styling.
- Update function in `src/tmux.rs` — called on every event from the event extractor.
- Debounce: max ~5 updates/sec.

[[2026-02-21]] Sat 20:46
Statement of work:
- Added StatusBar struct to src/orchestrator.rs: tracks session, phase, debounce timer
- StatusIndicator enum: StateChange(●), Action(→), Ok(✓), Thinking(?), NeedsInput(⚠), Failure(✗)
- StatusBar::init(): sets tmux style (bg=colour235, fg=colour136), widens left/right status
- StatusBar::update(): debounced (200ms min), formats '[batty] <phase> | <indicator> <message>'
- StatusBar::force_update(): bypasses debounce for critical events (NEEDS INPUT, stop, complete)
- Also sets terminal title via tmux set_title for tab/title bar visibility
- Integrated into run() loop: init on start, update on supervising, stop, complete
- Integrated into handle_prompt(): updates on auto-answer, suggest, Tier 2 thinking, escalate, NEEDS INPUT
- Made tmux::tmux_set() public for status-left-length/status-right-length settings
- 4 new tests: status_indicator_symbols, status_bar_init_and_update, status_bar_debounce, status_bar_on_missing_session
- How to verify: cargo test -- status_bar (all 4 pass)
