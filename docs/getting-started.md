# Getting Started

Get Batty running in your project in under 5 minutes.

## Prerequisites

- **Rust** toolchain (stable)
- **tmux** >= 3.1 (recommended >= 3.2)
- A coding agent: [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex), [Aider](https://aider.chat), or similar

`kanban-md` is also required but `batty install` handles it automatically.

## Install

```sh
# From the Batty repository
cargo install --path .

# Or run directly without installing
cargo run -- <command>
```

If `batty` is not found after install, add Cargo bin to your PATH:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

## Set Up Your Project

```sh
batty install
```

This does three things:

1. **Checks tools** -- Verifies `tmux` and `kanban-md` are available, attempts to install them if not
2. **Installs steering files** -- Adds workflow rules that your agent loads automatically (`.claude/rules/` for Claude Code, `.agents/rules/` for Codex)
3. **Installs skills** -- Adds kanban-md skills so your agent knows how to work with task boards

Batty never touches your existing `CLAUDE.md` or `AGENTS.md`.

To remove everything Batty installed:

```sh
batty remove
```

## Run Your First Phase

```sh
batty work my-phase
```

A tmux session opens with three areas:

- **Executor pane** -- Your coding agent working through the board
- **Orchestrator pane** -- Batty's log showing every auto-answer, escalation, and status change
- **Status bar** -- Live progress: phase name, current task, tasks done, supervision state

The agent picks tasks from the board, implements them, runs tests, and commits. Batty handles the prompts.

## Essential Commands

```sh
batty work my-phase        # start supervised execution
batty attach my-phase      # reattach to a running session
batty resume my-phase      # resume supervision after crash
batty board my-phase       # open the kanban board TUI
batty board-list           # list all boards with status
batty config               # show resolved configuration
```

## Runtime Modes

| Flag | Effect |
|------|--------|
| `--attach` | Open the tmux session immediately instead of backgrounding |
| `--agent codex` | Override the default executor agent |
| `--policy suggest` | Override the default policy tier |
| `--worktree` | Run in an isolated git worktree |
| `--worktree --new` | Force a fresh worktree (don't resume existing) |
| `--dry-run` | Show the composed launch context and exit |

## Configuration

Batty reads `.batty/config.toml`:

```toml
[defaults]
agent = "claude"       # or "codex", "aider"
policy = "act"         # observe | suggest | act | fully-auto

[supervisor]
enabled = true
program = "claude"
args = ["-p", "--output-format", "text"]
timeout_secs = 60

[detector]
silence_timeout_secs = 3
answer_cooldown_millis = 1000

[policy.auto_answer]
"Continue? [y/n]" = "y"
```

Full reference: [Configuration Reference](reference/config.md)

## Supervisor Hotkeys

During an active tmux session:

| Hotkey | Action |
|--------|--------|
| `C-b P` | Pause supervision (Tier 1 and Tier 2 stop, you type manually) |
| `C-b R` | Resume supervision |

While paused, human input still works -- you just take over from Batty.

## Dangerous Mode (Opt-In)

For environments where you want reduced agent safety prompts:

```toml
[dangerous_mode]
enabled = true
```

When enabled, Batty adds the appropriate dangerous-mode flag for each agent (`--dangerously-skip-permissions` for Claude, `--dangerously-bypass-approvals-and-sandbox` for Codex). Keep this disabled unless you explicitly accept the risk.

## Next Steps

- [CLI Reference](reference/cli.md) -- Full command and flag documentation
- [Architecture](architecture.md) -- How the three-layer supervision works
- [Troubleshooting](troubleshooting.md) -- Common issues and fixes
