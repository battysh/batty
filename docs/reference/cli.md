# CLI Reference

Global flags: `-v|--verbose` increases log verbosity; `-h|--help` and `-V|--version` are available on the root command.

## Setup

### `batty init`
- Usage: `batty init [--template solo|pair|simple|squad|large|research|software|batty]`
- Create `.batty/team_config/` with starter prompts and `team.yaml`.
- Key option: `--template` chooses the team shape; default is `simple`.
- Example: `batty init --template software`

### `batty validate`
- Usage: `batty validate`
- Validate team configuration and prerequisites without starting a session.
- Example: `batty validate`

### `batty config`
- Usage: `batty config [--json]`
- Print the resolved team configuration.
- Key option: `--json` emits machine-readable output.
- Example: `batty config --json`

## Session

### `batty start`
- Usage: `batty start [--attach]`
- Start the team daemon and tmux session.
- Key option: `--attach` jumps into the tmux session after startup.
- Example: `batty start --attach`

### `batty stop`
- Usage: `batty stop`
- Stop the daemon and kill the active team tmux session.
- Example: `batty stop`

### `batty attach`
- Usage: `batty attach`
- Attach to the running team tmux session.
- Example: `batty attach`

### `batty status`
- Usage: `batty status [--json]`
- Show member state, session status, and runtime information.
- Key option: `--json` emits machine-readable output.
- Example: `batty status --json`

## Communication

### `batty send`
- Usage: `batty send <ROLE> <MESSAGE>`
- Inject a message into a role, for example `architect` or `manager-1`.
- Example: `batty send architect "Review today's blockers"`

### `batty assign`
- Usage: `batty assign <ENGINEER> <TASK>`
- Queue a task for an engineer instance such as `eng-1-1`.
- Example: `batty assign eng-1-1 "Fix flaky merge test"`

### `batty inbox`
- Usage: `batty inbox <MEMBER>`
- List pending and delivered messages for one member.
- Example: `batty inbox manager-1`

### `batty read`
- Usage: `batty read <MEMBER> <ID>`
- Read a specific inbox message by full ID or prefix.
- Example: `batty read architect 20260311-101530`

### `batty ack`
- Usage: `batty ack <MEMBER> <ID>`
- Mark an inbox message as delivered.
- Example: `batty ack architect 20260311-101530`

## Workflow

### `batty board`
- Usage: `batty board`
- Open or print the current kanban board context.
- Example: `batty board`

### `batty merge`
- Usage: `batty merge <ENGINEER>`
- Rebase and merge an engineer worktree branch back into `main`.
- Example: `batty merge eng-1-1`

## Utility

### `batty completions`
- Usage: `batty completions <bash|zsh|fish>`
- Print shell completion scripts.
- Example: `batty completions zsh`

### `batty telegram`
- Usage: `batty telegram`
- Run the interactive Telegram bot setup flow for human-channel roles.
- Example: `batty telegram`

## Common Flows

```bash
# Start a fresh team
batty init --template simple
batty validate
batty start --attach

# Send work and review progress
batty assign eng-1-1 "Implement docs sync"
batty inbox eng-1-1
batty status

# Finish a completed branch
batty merge eng-1-1
```
