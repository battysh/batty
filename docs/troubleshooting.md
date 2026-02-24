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
1. Verify config:

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

```text
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

## `batty work all` skips a phase

**Cause:** The phase is already marked complete or has no backlog tasks.

```sh
batty list                         # check status of all phases
batty board my-phase               # inspect the specific board
```

Phases are discovered by scanning `.batty/kanban/` and sorted by numeric suffix. A phase with all tasks in `done` status is considered complete and skipped.

## Completion fails: no milestone task found

**Cause:** The phase board has no task tagged `milestone`.

Batty's completion contract requires at least one `milestone` task, and that task must be in `done` before the phase can complete.

Fix by tagging a task:

```sh
kanban-md edit <ID> --add-tag milestone
```

Or create a dedicated milestone task in the board with `tags: [milestone]`.

## `batty merge` cannot find the worktree

**Cause:** The worktree directory doesn't exist or the run number is wrong.

```sh
ls .batty/worktrees/               # list available worktree runs
batty merge phase-4 001            # use the correct run number
```

The run argument accepts `run-001`, `001`, or `1` — all resolve to the same worktree.

## Parallel agents fail with dependency cycle

**Cause:** Tasks in the board have circular `depends` entries.

```sh
batty work my-phase --dry-run      # shows the DAG and detects cycles
```

Fix the task frontmatter to remove the cycle. The DAG scheduler reports which tasks form the cycle.

## Merge queue conflict during parallel run

**Cause:** Two parallel agents modified the same files. The merge queue serializes merges but cannot auto-resolve all conflicts.

Attach to the session and resolve conflicts manually, or re-run the conflicting task with `--new` to get a fresh worktree.
