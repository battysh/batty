# Batty

**Hierarchical agent teams for software development.**

Define a team of AI agents in YAML -- architect, managers, engineers -- and Batty runs them in a coordinated tmux session. The daemon spawns agents, routes messages between roles, monitors output, manages worktrees, and keeps the kanban board moving.

## How It Works

A YAML config defines your team hierarchy:

- **Architect** -- Plans architecture, sends directives to managers
- **Manager** -- Breaks work into tasks, assigns to engineers, reports progress up
- **Engineers** -- Execute tasks in isolated worktrees, report back to their manager

Agents communicate through Maildir-based inboxes using `batty send` and `batty inbox`. A background daemon monitors all panes, delivers messages, runs state-driven interventions, keeps periodic standups/timeouts as fallback safety nets, and emits structured events.

Everything is files. Config is YAML. Messages are JSON. Events are JSONL. All git-versioned.

## Get Started

1. [Getting Started](getting-started.md) -- Install, configure, launch your first team
1. [CLI Reference](cli-reference.md) -- Operator-facing command guide
1. [Team Config Reference](config-reference.md) -- `team.yaml` fields, defaults, and examples
1. [Runtime Config](reference/config.md) -- Optional `.batty/config.toml` defaults
1. [Generated CLI Surface](reference/cli.md) -- Exhaustive clap-generated command tree
1. [Module Reference](reference/modules.md) -- Source map for contributors and maintainers

## Go Deeper

- [Scheduled Tasks & Cron](scheduled-tasks.md) -- Delayed dispatch, recurring tasks, cron recycler
- [Orchestrator Guide](orchestrator.md) -- Runtime automation, interventions, and config
- [Architecture](architecture.md) -- Module map, data flow, daemon design
- [Workflow Migration](workflow-migration.md) -- Safe defaults and rollout guidance for older teams and boards
- [Troubleshooting](troubleshooting.md) -- Common issues and fixes
