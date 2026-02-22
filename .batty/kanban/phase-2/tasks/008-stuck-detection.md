---
id: 8
title: Stuck detection and recovery
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:49:15.649449228-05:00
started: 2026-02-21T20:46:30.97054972-05:00
completed: 2026-02-21T20:49:15.649448747-05:00
tags:
    - core
depends_on:
    - 2
    - 3
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:49:15.649449167-05:00
class: standard
---

Detect executor stuck states from piped output and take recovery actions.

## Stuck states

- **Looping:** same output repeating, no task progress for extended period.
- **Stalled:** no output at all beyond normal thinking time.
- **Crashed:** tmux pane exited (executor process died).

## Recovery strategies

1. **Supervisor nudge:** inject a hint via `send-keys` (e.g., "You seem stuck. Try a different approach.").
2. **Escalate to human:** show in log pane + update status bar: `⚠ executor stuck — needs input`.
3. **Relaunch:** if executor crashed, option to relaunch in the same tmux pane.

## Configuration

In `.batty/config.toml`:
- `stuck_timeout_secs` — no-progress timeout (default: 300 = 5 minutes)
- `max_nudges` — how many nudges before escalating (default: 2)
- `auto_relaunch` — relaunch on crash (default: false)

## Implementation

- Monitor event buffer: if no `task_completed` or `command_ran` events for N seconds, consider stuck.
- Monitor tmux pane: detect pane exit via `tmux list-panes` or pane-died hook.
- Crash handling: preserve logs, report status, offer relaunch.

[[2026-02-21]] Sat 20:49
Statement of work:
- Added StuckConfig: timeout (default 300s), max_nudges (default 2), auto_relaunch (default false)
- Added StuckState enum: Normal, Stalled{since}, Looping, Crashed
- Added StuckAction enum: None, Nudge{message}, Escalate{reason}, Relaunch
- Created StuckDetector: tracks last_progress, nudge_count, recent output lines
- on_progress(): resets timer and nudge count on meaningful progress
- on_output(): feeds lines for loop detection (20-line window)
- check(session_alive): returns (StuckState, StuckAction) based on crash/loop/stall detection
- detect_loop(): detects 6+ identical lines or ABABABAB 2-line patterns
- nudge_sent(): increments nudge counter
- Integrated into OrchestratorConfig (stuck: Option<StuckConfig>)
- Integrated into run() loop: checks stuck state each iteration, sends nudges via send-keys, escalates, updates status bar
- 11 new tests: config, normal, stall, escalate_after_nudges, crash, crash_relaunch, loop, ab_pattern, varied_output, progress_reset, empty_output
- How to verify: cargo test -- stuck (all 11 pass)
