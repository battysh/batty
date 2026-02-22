---
id: 9
title: Human override protocol
status: done
priority: high
created: 0001-01-01T00:00:00Z
updated: 2026-02-21T20:52:10.057972333-05:00
started: 2026-02-21T20:49:26.89324362-05:00
completed: 2026-02-21T20:52:10.057971752-05:00
tags:
    - core
depends_on:
    - 4
    - 5
claimed_by: zinc-ivory
claimed_at: 2026-02-21T20:52:10.057972273-05:00
class: standard
---

Human keystrokes always take priority over supervisor responses.

## How it works with tmux

The human types directly in the tmux executor pane — this is native tmux behavior. No interception needed. The challenge is detecting that the human answered so the supervisor doesn't also answer.

## Detection heuristic

After a prompt is detected:
1. Batty starts a short timer before injecting an auto-answer (configurable: 1-2 seconds for Tier 1).
2. If new input appears in the piped output (indicating someone typed), assume human typed it.
3. Cancel the pending auto-answer.
4. For Tier 2: while the supervisor agent is thinking, if the piped output shows the prompt was answered, cancel the supervisor response.

## Supervisor pause

Human can explicitly pause supervision:
- Configurable keybinding or signal.
- Show in log pane: `[batty] supervision paused — you have control`.
- Resume after configurable quiet period or explicit resume signal.

## Transparency

Every supervisor action is visible in the supervisor log pane. The human always knows what Batty did and can override.

## Implementation

- Add answer-delay timer to Tier 1 auto-answer path.
- Add cancellation check to Tier 2 flow (check if prompt was already answered before injecting).
- Pause/resume logic in the supervisor state machine.

[[2026-02-21]] Sat 20:52
Statement of work:
- Added answer_delay to OrchestratorConfig (Duration, default: 1s for real use, 0 for tests)
- Created check_human_answered(): captures tmux pane, compares last line to prompt text
- Created wait_with_human_check(): polls during answer_delay, returns true if human typed
- Modified handle_prompt() for Tier 1: waits answer_delay before injection, cancels if human answered
- Modified handle_prompt() for Tier 2: after supervisor returns, checks if human answered during thinking
- Both paths call detector.human_override() on cancellation
- Status bar shows 'human override' when cancellation occurs
- Observer gets '→ human override — auto-answer/Tier 2 cancelled' events
- 5 new tests: human_check_no_session, human_check_prompt_visible, wait_zero_delay, wait_with_delay, handle_prompt_delay_zero
- How to verify: cargo test -- human_check wait_with handle_prompt_with_answer
