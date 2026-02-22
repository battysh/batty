# Getting Started

## Prerequisites

- Rust toolchain (`stable`)
- `tmux` 3.1+
- `kanban-md` CLI available on `PATH`

For new environments, run:

```sh
batty install
```

This command checks `tmux` and `kanban-md`, attempts automatic installation when possible, and installs Batty steering/skill assets in the project.

To remove installed Batty assets from a project:

```sh
batty remove
# optional targeting:
batty remove --target claude
batty remove --target codex --dir /path/to/project
```

## Basic Commands

Run directly from source:

```sh
cargo run -- config
cargo run -- work phase-2.7
```

Install globally (optional):

```sh
cargo install --path .
batty config
```

## Core Workflow

1. Start or resume execution for a phase:

```sh
batty work phase-2.7
# or
batty resume phase-2.7
```

2. Reattach to the tmux session when needed:

```sh
batty attach phase-2.7
```

3. Open the phase board in `kanban-md`:

```sh
batty board phase-2.7
# print resolved board path (for scripts/tooling):
batty board phase-2.7 --print-dir
```

4. List all boards at a glance:

```sh
batty board-list
```

## Common Runtime Modes

- `--attach`: opens the tmux session immediately
- `--worktree`: use isolated phase worktree runs
- `--worktree --new`: force a fresh run worktree
- `--dry-run`: show composed launch context and exit
- `--parallel N`: set parallel agent count (currently useful for future `work all` flow)
- `--agent AGENT`: override default executor agent
- `--policy POLICY`: override default policy tier

## Command and Flag Reference (Quick)

- `batty work <phase> [--parallel N] [--agent A] [--policy P] [--attach] [--worktree] [--new] [--dry-run]`
- `batty attach <phase>`
- `batty resume <phase|session>`
- `batty board <phase> [--print-dir]`
- `batty board-list`
- `batty config [--json]`
- `batty install [--target both|claude|codex] [--dir PATH]`
- `batty remove [--target both|claude|codex] [--dir PATH]`

## Dangerous Mode (Opt-In)

Batty supports an opt-in dangerous wrapper mode for supported agents:

```toml
[dangerous_mode]
enabled = true
```

When enabled, Batty adds the matching dangerous flag for `claude` or `codex` wrappers.
Keep this disabled unless you explicitly want reduced safety boundaries.

## Supervisor Hotkeys

During an active tmux-supervised run:

- Pause supervision: `Prefix + Shift+P` (`C-b P` with default tmux prefix)
- Resume supervision: `Prefix + Shift+R` (`C-b R` with default tmux prefix)
