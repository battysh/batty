<!-- markdownlint-disable MD041 -->

<p align="center">
  <img src="assets/batty-icon.png" alt="Batty" width="200">
  <h1 align="center">Batty</h1>
  <p align="center"><strong>Hierarchical agent teams for software development.</strong></p>
  <p align="center">
    Define a team of AI agents in YAML. Batty runs them through structured SDK protocols or a PTY-based shim, routes work and messages between roles, manages engineer worktrees, and keeps the team moving while tmux remains the display and persistence layer.
  </p>
</p>

<p align="center">
  <a href="https://github.com/battysh/batty/actions"><img src="https://img.shields.io/github/actions/workflow/status/battysh/batty/ci.yml?style=for-the-badge&label=CI" alt="CI"></a>
  <a href="https://crates.io/crates/batty-cli"><img src="https://img.shields.io/crates/v/batty-cli?style=for-the-badge" alt="crates.io"></a>
  <a href="https://github.com/battysh/batty/releases"><img src="https://img.shields.io/github/v/release/battysh/batty?style=for-the-badge" alt="Release"></a>
  <a href="https://github.com/battysh/batty/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=for-the-badge" alt="MIT License"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick Start</a> &middot;
  <a href="https://battysh.github.io/batty/">Docs</a> &middot;
  <a href="https://github.com/battysh/batty">GitHub</a>
</p>

______________________________________________________________________

Batty is a control plane for AI coding teams. Instead of one agent doing everything badly, you define roles like architect, manager, and engineers; Batty launches each agent through typed SDK protocols (the default) or a PTY-owning shim fallback, isolates engineer work in git worktrees, routes messages, tracks the board, and uses tmux for visibility and session persistence.

<p align="center">
  <img src="assets/batty-supervision-flow.png" alt="How Batty works: Define → Supervise → Execute → Verify → Deliver" width="900">
  <br>
  <em>How Batty works: Define → Supervise → Execute → Verify → Deliver</em>
</p>

## Quick Start

```sh
cargo install batty-cli
cd my-project && batty init
batty start --attach
batty send architect "Build a REST API with JWT auth"
```

That gets you from zero to a live team session. For the full walkthrough, templates, and configuration details, see the [Getting Started guide](docs/getting-started.md).

## Quick Demo

<p align="center">
  <a href="https://www.youtube.com/watch?v=2wmBcUnq0vw">
    <img src="assets/demo-poster.jpg" alt="Watch the Batty demo" width="1000">
  </a>
</p>

<p align="center">
  <strong><a href="https://www.youtube.com/watch?v=2wmBcUnq0vw">Watch the full demo on YouTube</a></strong>
</p>

```text
You
  |
  | batty send architect "Build a chess engine"
  v
Architect (Claude Code)
  | plans the approach
  v
Manager (Claude Code)
  | creates tasks, assigns work
  v
Engineers (Codex / Claude / Aider)
  eng-1-1   eng-1-2   eng-1-3   eng-1-4
   |          |          |          |
   +---- isolated git worktrees ----+
```

Batty keeps each role visible in its own tmux pane. In SDK mode (the default since v0.7.x), each agent communicates over a typed JSON protocol -- Claude Code via stream-json NDJSON, Codex via JSONL, Kiro via ACP JSON-RPC 2.0 -- giving structured message delivery, completion detection, and auto-approval of tool use. When SDK mode is off, the PTY-owning shim with screen classification provides a universal fallback. The daemon auto-dispatches board tasks, runs standups, and merges engineer branches back when they pass tests.

For unattended teams, leave `auto_respawn_on_crash: true` enabled. Turning it
off is mainly useful when you want to debug crashes manually or supervise pane
restarts yourself.

## Install

### 1. Install kanban-md

kanban-md is a separate Go tool. Grab the latest binary from
[GitHub releases](https://github.com/antopolskiy/kanban-md/releases):

```sh
# macOS (Apple Silicon)
curl -sL https://github.com/antopolskiy/kanban-md/releases/latest/download/kanban-md_0.33.0_darwin_arm64.tar.gz | tar xz
mv kanban-md /usr/local/bin/

# macOS (Intel)
curl -sL https://github.com/antopolskiy/kanban-md/releases/latest/download/kanban-md_0.33.0_darwin_amd64.tar.gz | tar xz
mv kanban-md /usr/local/bin/

# Linux (x86_64)
curl -sL https://github.com/antopolskiy/kanban-md/releases/latest/download/kanban-md_0.33.0_linux_amd64.tar.gz | tar xz
mv kanban-md ~/.local/bin/
```

Or with Go: `go install github.com/antopolskiy/kanban-md@latest`

### 2. Install Batty

From crates.io:

```sh
cargo install batty-cli
```

From source:

```sh
git clone https://github.com/battysh/batty.git
cd batty
cargo install --path .
```

## How It Works

```text
team.yaml
   |
   v
batty start
   |
   +--> SDK mode (default): typed JSON protocol per agent backend
   |      Claude Code  -> stream-json NDJSON
   |      Codex CLI    -> JSONL (exec --json)
   |      Kiro CLI     -> ACP JSON-RPC 2.0
   +--> PTY fallback: shim process with screen classifier (use_sdk_mode: false)
   +--> tmux panes for operator visibility
   +--> engineer worktrees created when enabled
   +--> daemon loop watches agent state, inboxes, board, retries, standups
   |
   v
batty send / assign / board / status / merge
```

Batty does not embed a model. It orchestrates external agent CLIs, keeps state in files, uses SDK protocols (or PTY shims) as the execution boundary, and uses tmux plus git worktrees as the operator-facing runtime surface.

On restart, Batty resumes saved agent sessions when the launch identity still
matches and the saved session is still available. If the saved session is stale
or missing, Batty falls back to a cold respawn and rebuilds task context
automatically. Healthy live panes are left alone; startup preflight only
respawns panes that are already dead.

## Built-in Templates

`batty init --template <name>` scaffolds a ready-to-run team:

| Template   | Agents | Description                                       |
| ---------- | -----: | ------------------------------------------------- |
| `solo`     |      1 | Single engineer, no hierarchy                     |
| `pair`     |      2 | Architect + 1 engineer                            |
| `simple`   |      6 | Human + architect + manager + 3 engineers         |
| `squad`    |      7 | Architect + manager + 5 engineers                 |
| `large`    |     19 | Human + architect + 3 managers + 15 engineers     |
| `research` |     10 | PI + 3 sub-leads + 6 researchers                  |
| `software` |     11 | Human + tech lead + 2 eng managers + 8 developers |
| `batty`    |      6 | Batty's own self-development team                 |

## Highlights

- Hierarchical agent teams instead of one overloaded coding agent
- SDK mode (default): structured JSON protocols for Claude Code (stream-json NDJSON), Codex CLI (JSONL), and Kiro CLI (ACP JSON-RPC 2.0) with completion detection and auto-approval
- PTY shim fallback with screen classification and state detection when SDK mode is off
- tmux-backed visibility with persistent panes and session resume
- Agent-agnostic role assignment: Claude Code, Codex, Aider, Kiro, or similar — set the default with `batty init --agent <backend>`
- Maildir inbox routing with explicit `talks_to` communication rules
- Stable per-engineer worktrees with fresh task branches on each assignment
- Kanban-driven task loop with auto-dispatch, retry tracking, and test gating
- Scheduled tasks: `scheduled_for` delays dispatch until a future time, `cron_schedule` enables recurring tasks that auto-recycle from done back to todo ([guide](docs/scheduled-tasks.md))
- [Intervention system](docs/interventions.md): seven automated recovery mechanisms (triage, review, owned-task, dispatch-gap, utilization, board replenishment, idle nudge) with cooldowns, dedup, and escalation
- Per-intervention runtime toggles via `batty nudge` to disable or re-enable specific daemon behaviors without restarting
- Orchestrator automation for triage, review, owned-task recovery, dispatch-gap recovery, utilization recovery, standups, nudges, and retrospectives
- Auto-merge policy engine with confidence scoring and configurable thresholds for safe unattended merges
- Review timeout escalation: stale reviews are nudged and auto-escalated after configurable thresholds, with per-priority overrides
- Failure pattern detection: rolling window analysis detects recurring failures and notifies when thresholds are exceeded
- SQLite telemetry database: `batty telemetry` queries agent performance, task lifecycle, review pipeline metrics, and event history
- Consolidated metrics dashboard: `batty metrics` shows tasks, cycle time, rates, and agent performance in one view
- Run retrospectives: `batty retro` generates Markdown reports analyzing task throughput, review stall durations, rework rates, and failure patterns
- Team template export/import: `batty export-template` saves your team config, `batty init --from` restores it
- Bundled Grafana dashboard template with 21 panels and 6 alerts for monitoring agent sessions, pipeline health, and task lifecycle
- Daemon restart recovery: dead agent panes are automatically respawned with task context and backoff
- Crash auto-respawn defaults to on for unattended teams; disable it only for debugging or manual supervision
- External senders: allow non-team sources (email routers, Slack bridges) to message any role
- Graceful non-git-repo handling: git-dependent operations degrade cleanly when the project is not a repository
- Session summary on `batty stop`: prints task counts, cycle times, and agent uptime before exiting
- Daemon auto-archive: completed tasks are automatically archived when the board exceeds a threshold
- Board health dashboard: `batty board health` shows per-status counts, stale tasks, and dependency issues
- Team load and cost estimation: `batty load` shows team utilization, `batty cost` estimates session spending
- Inbox purge with age filtering: `batty inbox purge --older-than 7d` cleans up delivered messages
- `batty validate --show-checks`: individual pass/fail status for each config validation rule
- `batty doctor --fix`: detect and clean up orphan worktrees and branches left by previous runs
- `batty board archive`: move completed tasks to an archive directory to keep the active board fast
- Error resilience: sentinel tests guard production error paths in daemon and task loop modules
- Modular codebase: large modules (daemon, config, delivery, watcher, doctor, merge) are decomposed into focused submodules
- Worktree reconciliation: auto-detect cherry-picked branches and reset stale worktrees so engineers always start clean
- Pending delivery queue: messages sent to agents that are still starting are buffered and delivered automatically when the agent becomes ready
- YAML config, Markdown boards, JSON/JSONL + SQLite logs: everything stays file-based

## CLI Quick Reference

| Command                                                  | Purpose                                                                  |
| -------------------------------------------------------- | ------------------------------------------------------------------------ |
| `batty init [--template NAME] [--agent BACKEND]`         | Scaffold `.batty/team_config/`                                           |
| `batty start [--attach]`                                 | Launch the daemon and tmux session                                       |
| `batty stop` / `batty attach`                            | Stop or reattach to the team session                                     |
| `batty send <role> <message>`                            | Send a message to a role                                                 |
| `batty assign <engineer> <task>`                         | Queue work for an engineer and report delivery result                    |
| `batty inbox <member>` / `read` / `ack`                  | Inspect and manage inbox messages                                        |
| `batty board` / `board list` / `board summary`           | Open the kanban board or inspect it without a TTY                        |
| `batty board health`                                     | Show board health dashboard (status counts, stale tasks, dep issues)     |
| `batty board archive [--older-than DATE]`                | Move done tasks to archive directory                                     |
| `batty status [--json]`                                  | Show current team state                                                  |
| `batty merge <engineer>`                                 | Merge an engineer worktree branch                                        |
| `batty review <id> <disposition> [feedback]`             | Record a review disposition (approve, request-changes, reject)           |
| `batty task review <id> --disposition <d>`               | Record a review disposition (workflow-level variant)                     |
| `batty task schedule <id> [--at T] [--cron E] [--clear]` | Set or clear scheduled dispatch time and cron recurrence                 |
| `batty nudge disable/enable/status`                      | Toggle specific daemon interventions at runtime                          |
| `batty telemetry summary/agents/tasks/reviews/events`    | Query SQLite telemetry for agent, task, and review metrics               |
| `batty retro`                                            | Generate a run retrospective analyzing throughput and failure patterns   |
| `batty load`                                             | Estimate team load and show recent load history                          |
| `batty cost`                                             | Estimate current run cost from agent session files                       |
| `batty metrics`                                          | Show consolidated telemetry dashboard (tasks, cycle time, rates, agents) |
| `batty doctor [--fix]`                                   | Dump diagnostic state; `--fix` cleans up orphan worktrees/branches       |
| `batty pause` / `resume` / `queue`                       | Control automation and inspect queued dispatch work                      |
| `batty inbox purge [--older-than DUR]`                   | Purge delivered inbox messages, optionally by age                        |
| `batty validate [--show-checks]`                         | Validate config; `--show-checks` shows per-rule pass/fail                |
| `batty config` / `export-run`                            | Show resolved config and export runtime state                            |
| `batty telegram`                                         | Configure Telegram for human communication                               |
| `batty completions <shell>`                              | Generate shell completions                                               |

## Requirements

- Rust toolchain, stable `>= 1.85`
- `tmux >= 3.1` (recommended `>= 3.2`)
- `kanban-md` CLI: see [Install](#install) for setup
- At least one coding agent CLI such as Claude Code, Codex, or Aider

## Engineer Worktrees

When `use_worktrees: true` is enabled for engineers, Batty keeps one stable
worktree directory per engineer under `.batty/worktrees/<engineer>`.

Each new `batty assign` does not create a new worktree. Instead it:

- reuses that engineer's existing worktree path
- resets the engineer slot onto current `main`
- creates a fresh task branch such as `eng-1-2/task-123` or
  `eng-1-2/task-say-hello-1633ae2d`
- launches the engineer in that branch

After merge, Batty resets the engineer back to the base branch
`eng-main/<engineer>` so the next assignment starts clean.

## Telegram Integration

Batty can expose a human endpoint over Telegram through a `user` role. This is
useful when you want the team to keep running in tmux while you send direction
or receive updates from your phone.

The fastest path is:

```sh
batty init --template simple
batty telegram
batty stop && batty start
```

`batty telegram` guides you through:

- creating or reusing a bot token from `@BotFather`
- discovering your numeric Telegram user ID
- sending a verification message
- updating `.batty/team_config/team.yaml` with the Telegram channel config

After setup, the `user` role in `team.yaml` will look like this:

```yaml
- name: human
  role_type: user
  channel: telegram
  talks_to: [architect]
  channel_config:
    provider: telegram
    target: "123456789"
    bot_token: "<telegram-bot-token>"
    allowed_user_ids: [123456789]
```

Notes:

- You must DM the bot first in Telegram before it can send you messages.
- `bot_token` can also come from `BATTY_TELEGRAM_BOT_TOKEN` instead of being
  stored in `team.yaml`.
- The built-in `simple`, `large`, `software`, and `batty` templates already
  include a Telegram-ready `user` role.

## Built with Batty

<p align="center">
  <img src="assets/batty-team-session.jpeg" alt="Batty team session in tmux" width="1000">
</p>

This session shows Batty coordinating a live team in `~/mafia_solver`: the `architect` sets direction, `black-lead` and `red-lead` turn that into lane-specific work, and the `black-eng-*` / `red-eng-*` panes are individual engineer agents running in separate worktrees inside one shared `tmux` layout.

- [chess_test](https://github.com/Zedmor/chess_test): a chess engine built by a Batty team (architect + manager + engineers)

## Grafana Monitoring

Batty includes a bundled Grafana dashboard template with 21 panels across 6 rows and 6 pre-configured alerts. The dashboard covers session overview, pipeline health, agent performance, delivery and communication, task lifecycle, and recent activity.

The dashboard JSON is available in the source tree at `src/team/grafana/dashboard.json`. Copy it and import into your Grafana instance to monitor live team runs.

Pre-configured alerts:

| Alert                  | Detects                            |
| ---------------------- | ---------------------------------- |
| Agent Stall            | Agent silent past threshold        |
| Delivery Failure Spike | Message delivery failures climbing |
| Pipeline Starvation    | Not enough work in the pipeline    |
| High Failure Rate      | Tasks failing above threshold      |
| Context Exhaustion     | Agent context window nearly full   |
| Session Idle           | Entire team idle too long          |

## Docs and Links

- [Getting Started](docs/getting-started.md)
- [Demo](https://www.youtube.com/watch?v=2wmBcUnq0vw)
- [CLI Reference](docs/reference/cli.md)
- [Runtime Config Reference](docs/reference/config.md)
- [Module Reference](docs/reference/modules.md)
- [Scheduled Tasks & Cron](docs/scheduled-tasks.md)
- [Intervention System](docs/interventions.md)
- [Orchestrator Guide](docs/orchestrator.md)
- [Architecture](docs/architecture.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Full docs site](https://battysh.github.io/batty/)
- [Use Cases](USECASES.md)
- [Contributing](CONTRIBUTING.md)
- [Good First Issues](https://github.com/battysh/batty/labels/good%20first%20issue)
- [GitHub](https://github.com/battysh/batty)

## License

MIT
