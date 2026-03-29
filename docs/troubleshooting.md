# Troubleshooting

Quick fixes for common Batty issues.

## `batty start` fails: no team config found

**Cause:** No `.batty/team_config/team.yaml` in the project.

```sh
batty init                             # scaffold team config
```

## `batty start` fails: session already exists

**Cause:** A previous session wasn't stopped.

```sh
batty stop                             # stop existing session
batty start                            # start fresh
```

## tmux version error on startup

**Cause:** tmux is too old or missing.

```sh
tmux -V                                # check version
```

Recommended: >= 3.2. Minimum: 3.1. Below 3.1 is not supported. Upgrade tmux and retry.

## `kanban-md` not found

**Cause:** `kanban-md` is not installed or not on `PATH`.

```sh
cargo install kanban-md --locked
```

Ensure `~/.cargo/bin` is in your `PATH`.

## No session to attach to

**Cause:** The team session isn't running.

```sh
tmux list-sessions                     # see what's running
batty start                            # start a new session
batty attach                           # then attach
```

## Messages not being delivered

**Cause:** The daemon may not be running, or the target member name is wrong.

1. Check daemon is alive:

```sh
cat .batty/daemon.pid                  # get PID
ps aux | grep batty                    # verify process is running
```

1. Check the inbox directly:

```sh
batty inbox <member>                   # list all messages
```

1. Check daemon logs:

```sh
tail -50 .batty/daemon.log
```

## Agent not responding in its pane

**Cause:** The agent may have exited or be waiting for input.

1. Attach and check: `batty attach`, then navigate to the agent's pane
1. Check daemon logs for spawn errors: `tail .batty/daemon.log`
1. Verify the agent binary is available: `which claude` or `which codex`

For unattended teams, keep `auto_respawn_on_crash: true` in `team.yaml` so
crashed shim agents are restarted automatically. Set it to `false` only while
debugging or when you want to restart panes manually.

## `batty send` rejected: not allowed to message

**Cause:** The `talks_to` rules in team.yaml don't allow this communication path.

Check team.yaml and adjust `talks_to` for the sender's role. The default hierarchy is:

- Architect \<-> Manager
- Manager \<-> Engineer

The human (CLI user) can always message any role.

## Validate fails: layout zones exceed 100%

**Cause:** Zone `width_pct` values in `layout.zones` sum to more than 100.

```sh
batty validate                         # shows the specific error
```

Adjust `width_pct` values to sum to 100 or less.

## Worktree merge conflicts

**Cause:** An engineer's worktree branch conflicts with main.

```sh
batty merge eng-1-1                    # attempt merge
```

If conflicts occur, resolve them manually in the worktree directory, then complete the merge.

## Telegram messages are not arriving

**Cause:** The bot token, allowed user ID, or Telegram chat bootstrap step is incomplete.

Check these in order:

1. Confirm the `user` role in `.batty/team_config/team.yaml` has `channel: telegram`
   and a `channel_config` block.
1. If you rely on `BATTY_TELEGRAM_BOT_TOKEN`, verify it is exported in the shell
   that launches `batty start`.
1. DM the bot first and send `/start`; Telegram will not let bots initiate a new chat.
1. Re-run `batty telegram` if you need to refresh the token or allowed user ID.
1. Restart Batty after changes:

```sh
batty stop
batty start
```

## Daemon dies unexpectedly

**Cause:** Check the daemon log for errors.

```sh
cat .batty/daemon.log
```

Common causes: tmux session was killed externally, agent binary not found, permission errors on inbox directories.

Restart with:

```sh
batty stop                             # clean up
batty start                            # fresh start
```

If Batty finds a stale saved session during startup, it falls back to a cold
respawn with context rebuild instead of requiring a manual pane restart.
Healthy live panes do not need a proactive restart; startup recovery only
touches panes that are already dead.

## Multiple orphaned batty sessions

**Cause:** Previous sessions from test runs or crashes.

```sh
batty stop                             # kills primary + all orphaned batty-* sessions
```
