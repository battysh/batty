---
id: 3
title: Implement supervisor pause/resume hotkeys
status: done
priority: high
created: 2026-02-22T00:19:28.21628303-05:00
updated: 2026-02-22T00:33:29.609536253-05:00
started: 2026-02-22T00:28:50.477737553-05:00
completed: 2026-02-22T00:33:29.609535792-05:00
tags:
    - supervisor
    - tmux
    - controls
depends_on:
    - 2
claimed_by: cape-staff
claimed_at: 2026-02-22T00:33:29.609536203-05:00
class: standard
---

Add runtime hotkeys so an operator can pause and resume supervisor automation without stopping the executor session.

## Requirements

1. Wire hotkey detection in the interactive run path.
2. Add commands/actions for:
   - Pause supervisor automation
   - Resume supervisor automation
3. Ensure pause mode suppresses auto-answers and Tier 2 interventions while still allowing human input.
4. Resume returns to normal supervision with safe state reset.
5. Emit clear events/messages for both actions.

## Verification

1. Manual run: hotkeys reliably toggle supervisor mode.
2. Logs/status output show pause/resume transitions.
3. Existing workflows (`batty work`, `batty resume`) continue to function.

[[2026-02-22]] Sun 00:33
Implemented contract from planning/supervisor-hotkey-control-contract.md: configured tmux hotkeys (C-b P/C-b R), added orchestrator supervisor-mode state machine, paused-mode gating for prompt automation/Tier2/stuck nudges, and transition/no-op tests.

## Statement of Work

- **What was done:** Implemented runtime supervisor pause/resume hotkeys and automation gating in the tmux orchestrator path.
- **Files created:** None.
- **Files modified:** `src/tmux.rs` - added hotkey configuration and action polling helpers; `src/orchestrator.rs` - added supervisor mode/action state machine, hotkey handling in loop, and paused-mode suppression behavior.
- **Key decisions:** Implemented `working/paused` as explicit orchestrator state with detector reset on each transition to prevent stale prompt replay after resume.
- **How to verify:** Run `cargo test`; check orchestrator tests for hotkey transitions and tmux tests for hotkey action polling.
- **Open issues:** Operator usage docs and expanded behavior-focused tests are handled in Task #4.
