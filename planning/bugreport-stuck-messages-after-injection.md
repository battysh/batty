# Bug Report: Injected Messages Stick in Pane Without Being Processed by Claude Code

## Summary

After `inject_message()` pastes a message into a Claude Code pane and sends Enter keystrokes, Claude Code sometimes does not process the message. The text is visible in the pane after the `❯` prompt, but Claude Code remains idle — it doesn't "see" the new input.

This results in the agent appearing to ignore messages. The human sends a message via Telegram, the daemon delivers it to the pane, but the agent never responds. Only a manual Enter keystroke (or the new `recover_stuck_messages` mechanism) triggers processing.

## Observed Behavior

- Marketing team: maya-lead received "How is progress?" via Telegram
- Message was successfully injected into pane %1781 (no marker-missing error)
- Claude Code showed `❯` prompt with the message text visible after it
- Claude Code did NOT start processing — remained idle
- Manual `tmux send-keys Enter` immediately woke Claude Code, which processed the message and responded

## Root Cause

`inject_message()` in `src/team/message.rs` does:

1. `send_keys("", enter=true)` — pre-injection Enter to wake idle agents
2. `sleep(200ms)`
3. `load-buffer` + `paste-buffer` — paste the formatted message
4. `sleep(500-3000ms)` — delay based on message length
5. `send_keys("", enter=true)` — submit the pasted text
6. `sleep(300ms)`
7. `send_keys("", enter=true)` — second Enter for confirmation

The problem is a **timing race between paste-buffer and Claude Code's input handling**:

- `paste-buffer` places text into the terminal emulator's buffer
- Claude Code's TUI reads input from the PTY in its own event loop
- If Claude Code's event loop is between poll cycles when the paste arrives, the text lands in the terminal buffer but Claude Code's input handler doesn't notice it
- The subsequent Enter keystrokes may be consumed by the terminal before Claude Code polls for new input
- Result: text is in the pane (visible on screen) but Claude Code's input state doesn't include it

This is particularly likely when:
- The agent has been idle for a while (Claude Code's poll interval may slow down)
- The message is long (larger paste = more terminal events to process)
- The pane is narrow (paste wraps to many lines, more terminal processing)

## Fix Applied

Added `recover_stuck_messages()` to the daemon's main loop (in `src/team/daemon/health.rs`):

- Runs every 30 seconds
- Scans all agent panes: captures last 30 lines
- Looks for the pattern: `❯` followed by `--- Message from` or `--- end message ---`
- If found, sends two Enter keystrokes to nudge Claude Code to process the waiting input

This is a **recovery mechanism**, not a prevention. The underlying race condition in tmux paste-buffer vs Claude Code input handling persists.

## Potential Prevention (Future)

1. After `inject_message`, wait longer and verify Claude Code shows activity (e.g., "Thinking...", "Reading...")
2. Use `send-keys` character-by-character instead of `paste-buffer` for short messages — this uses the terminal's keystroke input path which Claude Code monitors more reliably
3. Send a unique trigger sequence after paste that Claude Code always processes (e.g., a specific key combination)
4. Add a post-injection verification loop that re-sends Enter if Claude Code doesn't start processing within N seconds

## Related

- Bug #9: `bugreport-delivery-marker-missing-starting-agents.md` — messages fail during startup
- Bug: `bugreport-lead-inbox-triage-idle.md` — leads don't act on received messages

This bug is the "middle ground" — messages are delivered successfully (marker found), but the agent doesn't process them.
