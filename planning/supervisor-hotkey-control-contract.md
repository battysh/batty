# Supervisor Hotkey Control Contract (Phase 2.7 / Task #2)

This contract defines operator hotkeys for pausing and resuming Batty
supervision during an active tmux run.

## Default Hotkeys

- Pause supervision: `Prefix + Shift+P` (`C-b P` with tmux default prefix)
- Resume supervision: `Prefix + Shift+R` (`C-b R` with tmux default prefix)

Rationale:
- Prefix-based bindings avoid collisions with executor terminal input.
- Uppercase keys avoid common tmux lowercase defaults (for example
  `Prefix + p` previous-window).

## State Model

Two supervision states are supported:
- `working`: normal automation enabled.
- `paused`: automation disabled; human remains in control of executor pane.

### `working -> paused`

When pause hotkey is pressed:
- Supervisor mode switches to `paused`.
- Tier 1 auto-answers are suppressed.
- Tier 2 supervisor calls are suppressed.
- Automatic stuck nudges are suppressed.
- Human input remains fully available in the executor pane.

### `paused -> working`

When resume hotkey is pressed:
- Supervisor mode switches back to `working`.
- Detector state is reset via human-override semantics before re-enabling
  automation to avoid stale prompt replay.
- Tier 1/Tier 2 automation returns to normal behavior.

## User-Facing Feedback

On each accepted transition:
- Status bar updates immediately:
  - Pause: `● PAUSED — manual input only`
  - Resume: `✓ supervising`
- Orchestrator log pane records transition event with source `hotkey`.

## Edge Cases

- Repeated pause while already paused: no-op; emit log event
  (`already paused`) and keep status unchanged.
- Repeated resume while already working: no-op; emit log event
  (`already supervising`) and keep status unchanged.
- Prompt handling while paused:
  - Prompt detection can still occur internally.
  - No auto-response is injected while paused.
  - No Tier 2 call is made while paused.
  - Operator resolves prompt manually, then may resume supervision.

## Implementation Reference

Task `#3` (`Implement supervisor pause/resume hotkeys`) implements this
contract in `src/orchestrator.rs` and `src/tmux.rs`.
