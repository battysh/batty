---
id: 1
title: Unset CLAUDECODE env var when spawning tmux sessions
status: backlog
priority: critical
tags: [bug, runtime, milestone]
---

## Bug Description

When batty is invoked from within a Claude Code session (which is the primary use case), the `CLAUDECODE=1` environment variable leaks into the tmux session. When the Claude Code executor is then spawned inside that tmux session, it detects `CLAUDECODE` and refuses to start with:

```
Error: Claude Code cannot be launched inside another Claude Code session.
Nested sessions share runtime resources and will crash all active sessions.
To bypass this check, unset the CLAUDECODE environment variable.
```

This completely prevents batty from running agents when invoked from Claude Code.

## Root Cause

`tmux::create_session()` in `src/tmux.rs` uses `Command::new("tmux")` without explicitly unsetting `CLAUDECODE`. The tmux server inherits the parent environment, including `CLAUDECODE`.

Simply using `env -u CLAUDECODE` on the parent process is NOT sufficient because the tmux server process may already have the variable set from when it was first started.

## Fix Approach

The fix needs to ensure `CLAUDECODE` is unset in the tmux session environment. Options:

1. **Use `tmux set-environment -t <session> -u CLAUDECODE`** after session creation to unset the variable in the session's environment
2. **Use `tmux new-session -e CLAUDECODE=`** to explicitly clear it during session creation (tmux 3.2+)
3. **Wrap the agent command** to unset the variable: instead of directly running `claude`, run `env -u CLAUDECODE claude ...`

Option 3 is the most reliable since it works regardless of tmux server state. Modify the command construction in `create_session` or in the spawn config to prefix with `env -u CLAUDECODE`.

An alternative: in `apply_dangerous_mode_wrapper()` or in the spawn config assembly in `work.rs`, wrap the program command to strip the env var. E.g., instead of spawning `claude --prompt ...`, spawn `env -u CLAUDECODE claude --prompt ...`.

## Files to Modify

- `src/tmux.rs` — `create_session()` function (line ~339)
  - OR -
- `src/work.rs` — where spawn config is used to build the tmux command

## How to Verify

1. Start from within a Claude Code session (where `$CLAUDECODE=1`)
2. Run `batty work <phase>` (detached mode)
3. Attach to the session: `batty attach <phase>`
4. Confirm the agent starts successfully without the "nested sessions" error
5. Check the pty-output.log confirms agent launched

## Tests to Add

- Unit test in `tmux.rs` that verifies `CLAUDECODE` is not present in new sessions
- Or integration-style test that checks environment in spawned tmux sessions
