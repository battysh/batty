# Getting Started

Get a Batty agent team running in your project in under 5 minutes.

## Prerequisites

- **Rust** toolchain (stable, >= 1.85)
- **tmux** >= 3.1 (recommended >= 3.2)
- **kanban-md** CLI: `cargo install kanban-md --locked`
- A coding agent: [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex), [Aider](https://aider.chat), or similar

## Install

```sh
# From crates.io
cargo install batty-cli

# Or from source
git clone https://github.com/battysh/batty.git
cd batty
cargo install --path .
```

If `batty` is not found after install, add Cargo bin to your PATH:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

## Initialize Your Team

```sh
cd my-project
batty init
```

This scaffolds `.batty/team_config/` with:

- **team.yaml** -- Team hierarchy, roles, agents, layout, communication rules
- **Prompt templates** -- `architect.md`, `manager.md`, `engineer.md` with role-specific instructions
- **Kanban board** -- A kanban-md board for task tracking

The default template (`simple`) creates: 1 architect + 1 manager + 3 engineers = 5 agents.

### Choose a Template

```sh
batty init --template solo       # 1 engineer, no hierarchy
batty init --template pair       # architect + 1 engineer
batty init --template simple     # architect + manager + 3 engineers (default)
batty init --template squad      # architect + manager + 5 engineers
batty init --template large      # architect + 3 managers + 15 engineers + Telegram
batty init --template research   # PI + 3 sub-leads + 6 researchers
batty init --template software   # tech lead + 2 eng managers + 8 developers
```

## Configure Your Team

Edit `.batty/team_config/team.yaml`:

```yaml
name: my-project

layout:
  zones:
    - name: architect
      width_pct: 33
    - name: managers
      width_pct: 33
    - name: engineers
      width_pct: 34
      split: { horizontal: 3 }

roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
    talks_to: [manager]

  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    prompt: manager.md
    talks_to: [architect, engineer]

  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    prompt: engineer.md
    talks_to: [manager]
    use_worktrees: true
```

Validate before launching:

```sh
batty validate
```

## Launch the Team

```sh
batty start --attach
```

A tmux session opens with each agent in its own pane. The background daemon:

- Spawns each agent with its prompt template
- Creates git worktrees for engineers (if `use_worktrees: true`)
- Monitors agent output
- Routes messages between roles
- Runs periodic standups
- Emits structured events to `events.jsonl`

## Interact with Your Team

Send the architect a project goal:

```sh
batty send architect "Build a REST API for user management with JWT auth"
```

The architect plans, sends directives to the manager, who creates tasks and assigns them to engineers.

### Essential Commands

```sh
batty start --attach           # start team and attach to tmux
batty attach                   # reattach to running session
batty stop                     # stop daemon and kill session

batty send architect "msg"     # send message to a role
batty assign eng-1-1 "task"    # assign task to an engineer
batty inbox architect          # list messages for a member
batty read architect abc123    # read a specific message
batty ack architect abc123     # mark message as delivered

batty status                   # show all members and states
batty status --json            # machine-readable status
batty board                    # open kanban board TUI
batty config                   # show resolved configuration

batty merge eng-1-1            # merge engineer's worktree into main
```

## Communication Model

Agents communicate through Maildir-based inboxes in `.batty/inboxes/`. The `talks_to` field in team.yaml controls who can message whom:

```
Human -> Architect -> Manager -> Engineers
                   <-         <-
```

- **Human** can message anyone (via `batty send` from outside tmux)
- **Architect** talks to managers (strategic directives)
- **Manager** talks to architect (status) and engineers (task assignments)
- **Engineers** talk to their manager (progress, questions)

Agents send messages with `batty send <role> "<message>"` and check their inbox with `batty inbox <name>`.

## Shell Completions

```sh
# zsh
batty completions zsh > "${HOME}/.zsh/completions/_batty"

# bash
batty completions bash > "${HOME}/.local/share/bash-completion/completions/batty"

# fish
batty completions fish > "${HOME}/.config/fish/completions/batty.fish"
```

## Next Steps

- [CLI Reference](reference/cli.md) -- Full command documentation
- [Configuration Reference](reference/config.md) -- All team.yaml options
- [Architecture](architecture.md) -- How the daemon and message routing work
- [Troubleshooting](troubleshooting.md) -- Common issues and fixes
