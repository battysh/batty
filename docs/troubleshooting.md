# Troubleshooting

Quick fixes for common Batty issues.

## `batty work` exits immediately

**Cause:** Missing phase board directory or missing tools.

```sh
batty install                          # check/install tools
batty config                           # verify configuration
batty board my-phase --print-dir       # confirm board path exists
```

Boards live under `.batty/kanban/<phase>/`. Legacy projects may use `kanban/<phase>/`.

## tmux version error on startup

**Cause:** tmux is too old or missing required features (`pipe-pane`).

```sh
tmux -V                                # check version
```

Recommended: >= 3.2. Minimum: 3.1. Below 3.1 is not supported. Upgrade tmux and retry.

## `kanban-md` not found

**Cause:** `kanban-md` is not installed or not on `PATH`.

```sh
batty install                          # attempts auto-install
# or manually:
cargo install kanban-md --locked
```

Ensure `~/.cargo/bin` is in your `PATH`.

## `batty resume` cannot find session

**Cause:** The tmux session no longer exists.

```sh
tmux list-sessions                     # see what's running
batty resume batty-my-phase            # try the full session name
```

If no session exists, start fresh with `batty work my-phase`.

## Board path is wrong

**Cause:** Batty resolves board paths from active sessions, worktrees, and fallback rules.

```sh
batty board my-phase --print-dir       # show resolved path
```

## Supervisor not responding

**Cause:** Supervisor program misconfigured, timeout too low, or supervision is paused.

1. Check the tmux status bar -- if it says `PAUSED`, press `C-b R` to resume
2. Verify config:

```sh
batty config --json | grep -A5 supervisor
```

Key settings: `supervisor.program`, `supervisor.args`, `supervisor.timeout_secs`, `supervisor.trace_io`.

## Worktree confusion

**Cause:** `--worktree` resumes the latest existing run by default.

```sh
batty work my-phase --worktree --new   # force a fresh run
```

For full cleanup: `batty remove` then `rm -rf .batty` (destructive -- verify first).

## Dangerous mode is active unexpectedly

**Cause:** `dangerous_mode.enabled = true` in config.

```toml
[dangerous_mode]
enabled = false
```

## Debugging Tier 2 decisions

Inspect context snapshots:

```
.batty/logs/<run>/tier2-context-<n>.md
```

Correlate with orchestrator events. Enable `supervisor.trace_io = true` for richer logs.

## Detector stuck in a loop

**Cause:** Repeated stale output with no forward progress.

Check detector settings in config:

- `detector.silence_timeout_secs`
- `detector.answer_cooldown_millis`
- `detector.unknown_request_fallback`
- `detector.idle_input_fallback`

If the loop persists, attach with `batty attach <phase>`, provide manual input in the executor pane, and continue.
