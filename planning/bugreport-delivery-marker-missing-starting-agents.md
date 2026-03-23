# Bug Report: Message Delivery Fails with "marker missing" When Agents Are in `starting` State

## Summary

In a live Batty marketing team session, the daemon repeatedly fails to deliver messages to agents (especially leads like `jordan-pm`) because the injected message marker (`--- Message from <sender> ---`) is never found in the target pane. This causes a cascade failure:

1. All 3 retry attempts fail with "message marker missing after injection"
2. The failed delivery is escalated to the parent (`maya-lead`)
3. Maya-lead delivery also fails the same way
4. All agents remain stuck in `starting` state indefinitely
5. The entire team is effectively paralyzed despite the daemon being healthy

This is distinct from the existing lead-inbox-triage bug (see `bugreport-lead-inbox-triage-idle.md`). In that bug, messages are *delivered successfully* but leads don't act on them. In this bug, messages **physically cannot be injected** into the pane.

## Environment

- Repo: `~/batty_marketing`
- Date observed: `2026-03-22`
- Session: `batty-batty-marketing`
- Team pattern: maya-lead → jordan-pm → 4 engineers (priya-writer, kai-devrel, sam-designer, alex-dev)
- Batty version: 0.3.2

## Observed Behavior

### Daemon log pattern (repeating for every agent)

```
WARN message marker missing after injection; resending Enter recipient="jordan-pm" attempt=1 marker=--- Message from kai-devrel-1-1 ---
WARN message marker missing after injection; resending Enter recipient="jordan-pm" attempt=2 marker=--- Message from kai-devrel-1-1 ---
WARN message marker missing after injection; resending Enter recipient="jordan-pm" attempt=3 marker=--- Message from kai-devrel-1-1 ---
WARN failed delivery escalated to manager inbox recipient=jordan-pm from=kai-devrel-1-1 escalation_target=maya-lead attempts=3
```

This pattern repeated for every sender → jordan-pm, and then for daemon → maya-lead when escalated messages also failed.

### `batty status` output

All agents showed `starting` state — none ever transitioned to `working` or `idle`:

```
maya-lead         maya-lead    claude     starting    0    0
jordan-pm         jordan-pm    claude     starting    0    0
priya-writer-1-1  priya-writer claude     starting    0    0
kai-devrel-1-1    kai-devrel   claude     starting    0    0
sam-designer-1-1  sam-designer claude     starting    0    0
alex-dev-1-1      alex-dev     claude     starting    0    0
```

### Pane state at time of failure

Inspecting tmux panes showed Claude Code prompts waiting for input (`❯`), but some panes were empty or showed a bare shell prompt (`$`). The agents were alive but the injected text was not appearing in the pane capture.

## Root Cause Analysis

The delivery pipeline in `src/team/delivery.rs` works as follows:

1. `inject_message()` (in `src/team/message.rs`) pastes the message into the target pane via `tmux load-buffer` + `paste-buffer`, then sends Enter keystrokes
2. `verify_message_delivered()` waits 2 seconds, then captures the last 50 lines of the pane (`capture_pane_recent`) and looks for the marker string `--- Message from <sender> ---`
3. If not found, it resends Enter and retries (up to 3 attempts)
4. If all attempts fail, the message is queued for daemon-level retry, and eventually escalated to the parent

The failure happens because **the marker is never visible in the captured pane output**. Several scenarios can cause this:

### Scenario A: Agent in `starting` state — Claude Code not yet ready

When agents are freshly spawned (after `batty start` or `batty stop` + `batty start`), they go through a startup sequence: shell → Claude Code launches → Claude Code renders its UI. During this window:

- The pane may show a bare shell prompt or Claude Code's loading screen
- `paste-buffer` sends text into the pane, but it may be consumed by the shell (not Claude Code) or discarded by the loading UI
- The marker text is never rendered in a way that `capture_pane_recent` can find it
- By the time Claude Code is ready, the injected text is already gone

The 2-second verification delay is insufficient for agents that take 5-15 seconds to fully start.

### Scenario B: Pane in wrong state after restart

After `batty stop` + `batty start` with `--resume`, tmux sessions are recreated fresh, but old daemon state may reference agent sessions that need to be re-established. The daemon tries to deliver messages before the agents are actually ready to receive input.

### Scenario C: Paste buffer race condition

`inject_message()` does:
```
send_keys("", enter=true)     // wake-up Enter
sleep(200ms)
load-buffer + paste-buffer     // paste the message
sleep(500-3000ms)              // delay based on message length
send_keys("", enter=true)     // submit
sleep(300ms)
send_keys("", enter=true)     // second Enter
```

If the target pane is a Claude Code instance that hasn't fully rendered its input area, the `paste-buffer` content may be:
- Swallowed by the terminal emulator state
- Pasted into a non-input context (e.g., Claude Code's output area)
- Overwritten by Claude Code's own rendering

The verification then captures the pane and finds no marker.

## Impact

- **Complete team paralysis**: No agent transitions past `starting` because the initial messages (task assignments, context injection) cannot be delivered
- **Escalation cascade**: Failed deliveries escalate up the chain, but parent agents have the same problem
- **Silent failure**: The daemon continues running, logs show warnings but no crash — operator must inspect logs to discover the problem
- **Wasted restarts**: `batty stop` + `batty start` doesn't fix it because the same race condition recurs

## Proposed Fixes

### Fix 1: Wait for agent readiness before delivery (recommended)

Before attempting any message delivery to an agent, verify the agent is in a ready state:

```rust
fn is_agent_ready(pane_id: &str) -> bool {
    // Capture pane and check for Claude Code's input prompt indicator
    // e.g., "❯" or "⏵⏵ bypass permissions on"
    let capture = tmux::capture_pane_recent(pane_id, 10);
    capture.contains("❯") || capture.contains("bypass permissions")
}
```

The daemon should defer message delivery until `is_agent_ready()` returns true, with a configurable timeout (e.g., 60 seconds).

### Fix 2: Retry with exponential backoff

Instead of 3 retries × 2-second wait (total 6 seconds), use exponential backoff:
- Attempt 1: wait 2s
- Attempt 2: wait 5s
- Attempt 3: wait 10s
- Attempt 4: wait 20s

This gives slow-starting agents more time to become ready.

### Fix 3: Distinguish `starting` vs `ready` agent states

Currently agents go: `starting` → `working`/`idle`. Add a `ready` intermediate state that's only entered when the agent pane shows a live Claude Code prompt. Only deliver messages to `ready` or later states.

### Fix 4: Queue messages for not-yet-ready agents

Instead of attempting delivery and failing, maintain a per-agent message queue. Drain the queue once the agent transitions to `ready`. This avoids the retry/escalation cascade entirely.

### Fix 5: Increase verification capture window

`DELIVERY_VERIFICATION_CAPTURE_LINES = 50` may miss the marker if Claude Code has rendered a lot of startup output. Consider:
- Increasing to 100+ lines for `starting` agents
- Searching the full scrollback on first delivery attempt
- Using a tmux `pipe-pane` approach to stream all pane output through a monitor

## Relationship to Existing Bugs

This bug is a **precondition failure** that makes the lead-inbox-triage bug (`bugreport-lead-inbox-triage-idle.md`) even worse:

1. This bug: messages can't even reach the lead → team stuck in `starting`
2. Inbox-triage bug: messages reach the lead but aren't acted upon → team appears idle

Fixing this bug alone won't fix the triage problem, but without fixing this bug, the triage improvements have nothing to work with.

## Reproduction

1. Have a running batty team session with 6+ agents
2. `batty stop && batty start`
3. Watch daemon log — delivery failures begin within 10-20 seconds as the daemon tries to send resume/context messages before agents are ready
4. `batty status` shows all agents stuck in `starting`
5. Only a second `batty stop && batty start` (if agents happen to start faster) or manual intervention resolves it
