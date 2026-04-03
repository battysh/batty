# Architecture

Batty runs hierarchical agent teams through per-agent shims. This page covers the runtime model, daemon design, message routing, and module layout, with tmux acting as the operator-facing display layer.

## Team Hierarchy

```text
Human (optional)
  |
  v
Architect            -- plans architecture, sends directives
  |
  v
Manager(s)           -- breaks work into tasks, assigns to engineers
  |
  v
Engineer(s)          -- executes tasks in worktrees, reports progress
```

Each role maps to one runtime member and one visible pane. Engineers are multiplicative: 3 managers x 5 engineers = 15 engineer members. The hierarchy is defined in `team.yaml` and resolved at startup.

## Runtime Model

```text
batty start
  -> load team.yaml
  -> validate config
  -> resolve hierarchy (instances, naming, reports_to)
  -> create tmux session with layout zones
  -> initialize Maildir inboxes for all members
  -> check for resume marker (.batty/resume)
  -> spawn daemon as background process (with --resume if marker found)
  -> daemon:
       -> discover pane IDs via @batty_role tags
       -> spawn one batty shim per agent
       -> each shim owns a PTY, launches the agent CLI, and classifies screen state
       -> tmux panes tail shim PTY logs for display
       -> create git worktrees for engineers
       -> enter polling loop:
            poll inbox    -- deliver pending messages via shim command channel
            poll shims    -- receive structured state/completion events
            poll standups -- periodic standup reports
            poll automation -- state-driven recovery interventions
            poll board    -- task rotation when threshold reached
            emit events   -- structured JSONL event log
```

## Message Routing

Agents communicate through Maildir-based inboxes at `.batty/inboxes/<member>/`.

```text
batty send manager "Phase 1: build the board module"
  -> serialize to JSON
  -> atomic write to .batty/inboxes/manager/new/
  -> daemon polls inbox
  -> daemon reads message from new/
  -> daemon sends SendMessage to manager's shim
  -> shim writes into the agent PTY and waits for completion
  -> daemon moves message to cur/ (delivered)
```

Communication is gated by `talks_to` rules in team.yaml. Unauthorized messages are rejected at send time.

### Message Types

| Type     | Usage                          |
| -------- | ------------------------------ |
| `send`   | General message between roles  |
| `assign` | Task assignment to an engineer |

## Daemon

The daemon runs as a detached background process (`batty daemon --project-root <path>`), spawned by `batty start`. It:

- **Spawns shims** for each agent, with tmux panes attached to shim PTY logs
- **Maintains engineer worktrees** with `use_worktrees: true`, keeping one
  stable worktree path per engineer and switching assignments onto fresh task
  branches from `main`
- **Delivers messages** by polling Maildir inboxes and sending structured shim commands
- **Records assignment outcomes** so callers can see whether delivery launched
  successfully or failed
- **Monitors output** via shim state transitions and tmux/session watchers where needed
- **Runs reactive interventions** when system state implies stalled ownership,
  triage backlog, review backlog, dispatch gaps, or low utilization
- **Runs standups** on configurable intervals per role as a fallback safety net
- **Nudges agents** by re-injecting prompt sections on a timer as a fallback safety net
- **Rotates the board** when completed task count hits the threshold
- **Emits events** to `events.jsonl` for debugging and audit

The `automation` block in `team.yaml` controls these daemon behaviors:

```yaml
automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
```

These switches let a project keep periodic timers enabled while turning specific
reactive recovery paths on or off per team.

In shim mode, `auto_respawn_on_crash` defaults to `true` and should usually stay
enabled for unattended teams. Disable it only for debugging or when an operator
intends to supervise crash recovery manually.

When started with `--resume`, the daemon launches agents with session
continuation flags when the saved launch identity still matches and the saved
session is still available. If saved session state is stale or missing, that
member falls back to a fresh launch and rebuilds context from disk instead of
blocking startup. Startup preflight only respawns panes that are already dead;
healthy live panes are not restarted proactively.

PID is stored at `.batty/daemon.pid`. Logs go to `.batty/daemon.log`.

## Module Map

| Module              | Responsibility                                                         |
| ------------------- | ---------------------------------------------------------------------- |
| `main.rs`           | CLI entrypoint, command dispatch                                       |
| `cli.rs`            | clap command/option definitions                                        |
| `team/mod.rs`       | Team lifecycle: init, start, stop, attach, status, send, assign, merge |
| `team/config.rs`    | TeamConfig, RoleDef, LayoutConfig parsed from YAML                     |
| `team/hierarchy.rs` | Instance resolution: naming, manager-engineer partitioning             |
| `team/layout.rs`    | tmux layout builder (zones to panes)                                   |
| `team/daemon.rs`    | Core daemon: shim spawning, polling loop, message delivery             |
| `team/inbox.rs`     | Maildir-based inbox: deliver, pending, mark_delivered, all_messages    |
| `team/message.rs`   | Message formatting and prompt composition for delivery                 |
| `team/comms.rs`     | Channel abstraction (tmux pane, Telegram, etc.)                        |
| `team/telegram.rs`  | Telegram bot integration and setup wizard                              |
| `team/standup.rs`   | Standup report generation from agent output                            |
| `team/watcher.rs`   | SessionWatcher: tmux output capture and state tracking                 |
| `shim/`             | PTY-owning shim runtime, protocol, classifier, chat frontend           |
| `team/board.rs`     | Kanban board rotation and task management                              |
| `team/events.rs`    | Structured event sink (JSONL)                                          |
| `team/templates/`   | Built-in team.yaml templates and prompt .md files                      |
| `tmux.rs`           | tmux command wrapper (session, window, pane, split, send-keys)         |
| `agent/mod.rs`      | AgentAdapter trait + registry                                          |
| `agent/codex.rs`    | Codex adapter                                                          |
| `worktree.rs`       | Git worktree create/cleanup                                            |
| `log/mod.rs`        | JSONL execution logs                                                   |
| `paths.rs`          | `.batty/` path resolution                                              |
| `config.rs`         | Legacy config loading                                                  |
| `prompt/`           | Prompt template utilities                                              |
| `task/`             | kanban-md task parsing                                                 |
| `events.rs`         | Event types                                                            |
| `bin/docsgen.rs`    | Documentation generator                                                |

## File Layout

```text
.batty/
  team_config/
    team.yaml              # team hierarchy config
    architect.md           # architect prompt template
    manager.md             # manager prompt template
    engineer.md            # engineer prompt template
    board/                 # kanban-md board directory
    events.jsonl           # structured event log
  shim_logs/
    <member>.log          # raw PTY output tailed by tmux panes in shim mode
  inboxes/
    architect/
      new/                 # pending messages
      cur/                 # delivered messages
      tmp/                 # atomic write staging
    manager/
    eng-1-1/
    ...
  daemon.pid               # daemon process ID
  daemon.log               # daemon stdout/stderr
  resume                   # resume marker (written by stop, consumed by start)
```

## tmux Compatibility

| tmux version | Status                          |
| ------------ | ------------------------------- |
| >= 3.2       | Full feature path (recommended) |
| 3.1.x        | Supported with fallbacks        |
| < 3.1        | Not supported (fails fast)      |

Even in shim mode, tmux remains required for pane layout, visibility, attach/detach, and session persistence.

## SDK Communication Modes

When `use_sdk_mode: true` is set in `team.yaml` (the default for Claude and Codex agents), the shim uses a typed JSON protocol to communicate with the agent process instead of PTY screen-scraping. The shim remains the process model (it owns the subprocess, manages lifecycle, and reports state to the daemon), but the I/O channel switches from raw PTY bytes to structured messages.

Three backend-specific protocols are supported:

| Backend     | Protocol                | Agent invocation                                                   |
| ----------- | ----------------------- | ------------------------------------------------------------------ |
| Claude Code | stream-json NDJSON      | `claude -p --input-format=stream-json --output-format=stream-json` |
| Codex CLI   | JSONL spawn-per-message | `codex exec --json`                                                |
| Kiro CLI    | ACP JSON-RPC 2.0        | `kiro-cli acp --trust-all-tools`                                   |

**Claude Code** maintains a long-running process. The shim writes JSON request objects to stdin and reads NDJSON response lines from stdout. State transitions (ready, thinking, tool-use, done) are derived from the structured event stream rather than terminal output classification.

**Codex CLI** uses a spawn-per-message model. Each message dispatches a new `codex exec --json` invocation whose stdout is a JSONL stream of structured events. The shim collects the response and maps completion back to the daemon's state model.

**Kiro CLI** speaks ACP (Agent Communication Protocol), a JSON-RPC 2.0 protocol on stdin/stdout. The shim sends RPC requests and receives typed responses, with tool invocations handled as nested RPC calls.

When `use_sdk_mode: false`, the shim falls back to the original PTY-based runtime: it owns a pseudo-terminal, feeds keystrokes, and classifies agent state from screen content.

### Key Files

| File                    | Responsibility                        |
| ----------------------- | ------------------------------------- |
| `shim/runtime_sdk.rs`   | Claude Code stream-json SDK runtime   |
| `shim/runtime_codex.rs` | Codex CLI JSONL SDK runtime           |
| `shim/runtime_kiro.rs`  | Kiro CLI ACP JSON-RPC 2.0 SDK runtime |

### Shim Configuration (team.yaml)

| Key                               | Type    | Default | Description                                                  |
| --------------------------------- | ------- | ------- | ------------------------------------------------------------ |
| `use_shim`                        | boolean | `true`  | Run each agent through a PTY-owning shim subprocess          |
| `use_sdk_mode`                    | boolean | `true`  | Use structured JSON protocols instead of PTY screen-scraping |
| `auto_respawn_on_crash`           | boolean | `true`  | Automatically respawn crashed agent shims                    |
| `shim_health_check_interval_secs` | integer | `30`    | How often to send health pings to agent shims                |
| `shim_health_timeout_secs`        | integer | `90`    | Max time without a pong before considering agent stalled     |
| `shim_shutdown_timeout_secs`      | integer | `10`    | Grace period for graceful shutdown before SIGKILL            |
| `shim_working_state_timeout_secs` | integer | `3600`  | Max time an agent can stay in Working state                  |
