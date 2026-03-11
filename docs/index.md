# Batty

**Hierarchical agent teams for software development.**

Define a team of AI agents in YAML -- architect, managers, engineers -- and Batty runs them in a coordinated tmux session. The daemon spawns agents, routes messages between roles, monitors output, manages worktrees, and keeps the kanban board moving.

## How It Works

A YAML config defines your team hierarchy:

- **Architect** -- Plans architecture, sends directives to managers
- **Manager** -- Breaks work into tasks, assigns to engineers, reports progress up
- **Engineers** -- Execute tasks in isolated worktrees, report back to their manager

Agents communicate through Maildir-based inboxes using `batty send` and `batty inbox`. A background daemon monitors all panes, delivers messages, runs periodic standups, and emits structured events.

Everything is files. Config is YAML. Messages are JSON. Events are JSONL. All git-versioned.

## Get Started

1. [Getting Started](getting-started.md) -- Install, configure, launch your first team
1. [CLI Reference](reference/cli.md) -- Every command and flag
1. [Runtime Config](reference/config.md) -- Optional `.batty/config.toml` defaults
1. [Module Reference](reference/modules.md) -- Source map for contributors and maintainers

## Go Deeper

- [Architecture](architecture.md) -- Module map, data flow, daemon design
- [Troubleshooting](troubleshooting.md) -- Common issues and fixes
