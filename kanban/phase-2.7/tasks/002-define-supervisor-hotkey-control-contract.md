---
id: 2
title: Define supervisor hotkey control contract
status: done
priority: high
created: 2026-02-22T00:19:23.958362989-05:00
updated: 2026-02-22T00:28:05.339429038-05:00
started: 2026-02-22T00:26:14.878857202-05:00
completed: 2026-02-22T00:28:05.339428658-05:00
tags:
    - supervisor
    - tmux
    - ux
claimed_by: cape-staff
claimed_at: 2026-02-22T00:28:05.339428988-05:00
class: standard
---

Define how operators can pause and resume Batty supervision during an active tmux run.

## Requirements

1. Choose default hotkeys for `pause` and `resume` that do not conflict with common terminal controls.
2. Define behavior for each state transition:
   - working -> paused
   - paused -> working
3. Define user-facing feedback (status bar/log pane message) when supervisor state changes.
4. Document edge cases: repeated key presses, no-op transitions, and behavior during prompt handling.

## Verification

1. Contract is documented in task notes or linked design notes.
2. Implementation task references this contract explicitly.

[[2026-02-22]] Sun 00:27
Contract documented in planning/supervisor-hotkey-control-contract.md. Default hotkeys: Prefix+Shift+P pause, Prefix+Shift+R resume. Task #3 will implement this contract exactly (state transitions, paused-mode gating, status/log feedback, no-op handling).

## Statement of Work

- **What was done:** Defined and documented the supervisor pause/resume hotkey contract for Phase 2.7.
- **Files created:** `planning/supervisor-hotkey-control-contract.md` - contract defining hotkeys, state transitions, feedback, and edge cases.
- **Files modified:** `kanban/phase-2.7/tasks/002-define-supervisor-hotkey-control-contract.md` - linked contract and recorded implementation intent for Task #3.
- **Key decisions:** Use tmux prefix-based uppercase bindings (`C-b P` / `C-b R`) to avoid conflicts with executor input and common lowercase tmux defaults.
- **How to verify:** Read `planning/supervisor-hotkey-control-contract.md` and confirm it defines hotkeys, `working/paused` transitions, feedback, and paused prompt-handling behavior.
- **Open issues:** Task #3 must implement runtime control handling and enforcement exactly per this contract.
