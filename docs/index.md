# Batty

**Supervised agent execution for software teams.**

Batty reads your kanban board, launches a coding agent in tmux, supervises its work, auto-answers routine prompts, escalates real questions, gates on tests, and merges the result. You design the phases. Batty executes them.

## How It Works

Three roles in one supervised tmux session:

- **Executor** -- Your coding agent (Claude Code, Codex, Aider) works through the board
- **Supervisor** -- Watches the executor, answers questions it can't handle alone
- **Director** -- Reviews completed phases and decides: merge, rework, or escalate

Two-tier prompt handling keeps things moving:

1. **Tier 1** -- Regex match on known prompts -> instant auto-answer (~80% of prompts)
2. **Tier 2** -- Supervisor agent with full project context -> intelligent answer for the rest

Everything is files. Config is TOML. Tasks are Markdown. Logs are JSONL. All git-versioned.

## Get Started

1. [Getting Started](getting-started.md) -- Install, configure, run your first phase
2. [CLI Reference](reference/cli.md) -- Every command and flag
3. [Configuration](reference/config.md) -- All config.toml options

## Go Deeper

- [Architecture](architecture.md) -- Module map, data flow, design decisions
- [Troubleshooting](troubleshooting.md) -- Common issues and fixes
