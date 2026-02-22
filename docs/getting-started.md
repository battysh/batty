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
```

## Common Runtime Modes

- `--attach`: opens the tmux session immediately
- `--worktree`: use isolated phase worktree runs
- `--worktree --new`: force a fresh run worktree
- `--dry-run`: show composed launch context and exit
