# CLI Reference

This reference documents all Batty commands.

## `batty`

Hierarchical agent team system for software development

```text
Hierarchical agent team system for software development

Usage: batty [OPTIONS] <COMMAND>

Commands:
  init         Scaffold .batty/team_config/ with default team.yaml and prompt templates
  start        Start the team daemon and tmux session
  stop         Stop the team daemon and kill the tmux session
  attach       Attach to the running team tmux session
  status       Show all team members and their states
  send         Send a message to an agent role (human → agent injection)
  assign       Assign a task to an engineer (used by manager agent)
  validate     Validate team config without launching
  config       Show resolved team configuration
  board        Show the kanban board
  inbox        List inbox messages for a team member
  read         Read a specific message from a member's inbox
  ack          Acknowledge (mark delivered) a message in a member's inbox
  merge        Merge an engineer's worktree branch into main
  completions  Generate shell completions
  telegram     Set up Telegram bot for human communication
  help         Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help

  -V, --version
          Print version
```

## `batty assign`

Assign a task to an engineer (used by manager agent)

```text
Assign a task to an engineer (used by manager agent)

Usage: batty assign [OPTIONS] <ENGINEER> <TASK>

Arguments:
  <ENGINEER>
          Target engineer instance (e.g., "eng-1-1")

  <TASK>
          Task description

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty attach`

Attach to the running team tmux session

```text
Attach to the running team tmux session

Usage: batty attach [OPTIONS]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty board`

Show the kanban board

```text
Show the kanban board

Usage: batty board [OPTIONS]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty completions`

Generate shell completions

```text
Generate shell completions

Usage: batty completions [OPTIONS] <SHELL>

Arguments:
  <SHELL>
          Shell to generate completion script for
          
          [possible values: bash, zsh, fish]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty config`

Show resolved team configuration

```text
Show resolved team configuration

Usage: batty config [OPTIONS]

Options:
      --json
          Emit machine-readable JSON output

  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty init`

Scaffold .batty/team_config/ with default team.yaml and prompt templates

```text
Scaffold .batty/team_config/ with default team.yaml and prompt templates

Usage: batty init [OPTIONS]

Options:
      --template <TEMPLATE>
          Template to use for scaffolding

          Possible values:
          - solo:     Single agent, no hierarchy (1 pane)
          - pair:     Architect + 1 engineer pair (2 panes)
          - simple:   1 architect + 1 manager + 3 engineers (5 panes)
          - squad:    1 architect + 1 manager + 5 engineers with layout (7 panes)
          - large:    Human + architect + 3 managers + 15 engineers with Telegram (19 panes)
          - research: PI + 3 sub-leads + 6 researchers — research lab style (10 panes)
          - software: Human + tech lead + 2 eng managers + 8 developers — full product team (11 panes)
          - batty:    Batty self-development: human + architect + manager + 4 Rust engineers (6 panes)
          
          [default: simple]

  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help (see a summary with '-h')
```

## `batty inbox`

List inbox messages for a team member

```text
List inbox messages for a team member

Usage: batty inbox [OPTIONS] <MEMBER>

Arguments:
  <MEMBER>
          Member name (e.g., "architect", "manager-1", "eng-1-1")

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty read`

Read a specific message from a member's inbox

```text
Read a specific message from a member's inbox

Usage: batty read [OPTIONS] <MEMBER> <ID>

Arguments:
  <MEMBER>
          Member name

  <ID>
          Message ID (or prefix) from `batty inbox` output

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty ack`

Acknowledge (mark delivered) a message in a member's inbox

```text
Acknowledge (mark delivered) a message in a member's inbox

Usage: batty ack [OPTIONS] <MEMBER> <ID>

Arguments:
  <MEMBER>
          Member name

  <ID>
          Message ID (from `batty inbox` output)

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty merge`

Merge an engineer's worktree branch into main

```text
Merge an engineer's worktree branch into main

Usage: batty merge [OPTIONS] <ENGINEER>

Arguments:
  <ENGINEER>
          Engineer instance name (e.g., "eng-1-1")

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty send`

Send a message to an agent role (human → agent injection)

```text
Send a message to an agent role (human → agent injection)

Usage: batty send [OPTIONS] <ROLE> <MESSAGE>

Arguments:
  <ROLE>
          Target role name (e.g., "architect", "manager-1")

  <MESSAGE>
          Message to inject

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty start`

Start the team daemon and tmux session

```text
Start the team daemon and tmux session

Usage: batty start [OPTIONS]

Options:
      --attach
          Auto-attach to the tmux session after startup

  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty status`

Show all team members and their states

```text
Show all team members and their states

Usage: batty status [OPTIONS]

Options:
      --json
          Emit machine-readable JSON output

  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty stop`

Stop the team daemon and kill the tmux session

```text
Stop the team daemon and kill the tmux session

Usage: batty stop [OPTIONS]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

## `batty telegram`

Set up Telegram bot for human communication

```text
Set up Telegram bot for human communication

Usage: batty telegram [OPTIONS]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

Interactive wizard that configures Telegram integration for user roles.
Writes `bot_token` and `allowed_user_ids` into `channel_config` in team.yaml.
Can also be configured via `BATTY_TELEGRAM_BOT_TOKEN` env var.

## `batty validate`

Validate team config without launching

```text
Validate team config without launching

Usage: batty validate [OPTIONS]

Options:
  -v, --verbose...
          Verbosity level (-v, -vv, -vvv)

  -h, --help
          Print help
```

