# Architecture

Batty runs hierarchical agent teams in tmux. This page covers the runtime model, daemon design, message routing, and module layout.

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

Each role maps to one or more tmux panes. Engineers are multiplicative: 3 managers x 5 engineers = 15 engineer panes. The hierarchy is defined in `team.yaml` and resolved at startup.

## Runtime Model

```text
batty start
  -> load team.yaml
  -> validate config
  -> resolve hierarchy (instances, naming, reports_to)
  -> create tmux session with layout zones
  -> initialize Maildir inboxes for all members
  -> spawn daemon as background process
  -> daemon:
       -> discover pane IDs via @batty_role tags
       -> spawn agents in panes with prompt templates
       -> create git worktrees for engineers
       -> enter polling loop:
            poll inbox    -- deliver pending messages via tmux send-keys
            poll watchers -- capture agent output, detect state changes
            poll standups -- periodic standup reports
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
  -> daemon injects into manager's tmux pane via send-keys
  -> daemon moves message to cur/ (delivered)
```

Communication is gated by `talks_to` rules in team.yaml. Unauthorized messages are rejected at send time.

### Message Types

| Type | Usage |
| --- | --- |
| `send` | General message between roles |
| `assign` | Task assignment to an engineer |

## Daemon

The daemon runs as a detached background process (`batty daemon --project-root <path>`), spawned by `batty start`. It:

- **Spawns agents** in their assigned tmux panes with composed prompts
- **Creates worktrees** for engineers with `use_worktrees: true`
- **Delivers messages** by polling Maildir inboxes and injecting into panes
- **Monitors output** via `SessionWatcher` (tmux `capture-pane`)
- **Runs standups** on configurable intervals per role
- **Nudges agents** by re-injecting prompt sections on a timer
- **Rotates the board** when completed task count hits the threshold
- **Emits events** to `events.jsonl` for debugging and audit

PID is stored at `.batty/daemon.pid`. Logs go to `.batty/daemon.log`.

## Module Map

| Module | Responsibility |
| --- | --- |
| `main.rs` | CLI entrypoint, command dispatch |
| `cli.rs` | clap command/option definitions |
| `team/mod.rs` | Team lifecycle: init, start, stop, attach, status, send, assign, merge |
| `team/config.rs` | TeamConfig, RoleDef, LayoutConfig parsed from YAML |
| `team/hierarchy.rs` | Instance resolution: naming, manager-engineer partitioning |
| `team/layout.rs` | tmux layout builder (zones to panes) |
| `team/daemon.rs` | Core daemon: agent spawning, polling loop, message delivery |
| `team/inbox.rs` | Maildir-based inbox: deliver, pending, mark_delivered, all_messages |
| `team/message.rs` | Message formatting and prompt composition for injection |
| `team/comms.rs` | Channel abstraction (tmux pane, Telegram, etc.) |
| `team/standup.rs` | Standup report generation from agent output |
| `team/watcher.rs` | SessionWatcher: tmux output capture and state tracking |
| `team/board.rs` | Kanban board rotation and task management |
| `team/events.rs` | Structured event sink (JSONL) |
| `team/templates/` | Built-in team.yaml templates and prompt .md files |
| `tmux.rs` | tmux command wrapper (session, window, pane, split, send-keys) |
| `agent/mod.rs` | AgentAdapter trait + registry |
| `agent/codex.rs` | Codex adapter |
| `worktree.rs` | Git worktree create/cleanup |
| `log/mod.rs` | JSONL execution logs |
| `paths.rs` | `.batty/` path resolution |
| `config.rs` | Legacy config loading |
| `prompt/` | Prompt template utilities |
| `task/` | kanban-md task parsing |
| `events.rs` | Event types |
| `bin/docsgen.rs` | Documentation generator |

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
```

## tmux Compatibility

| tmux version | Status |
| --- | --- |
| >= 3.2 | Full feature path (recommended) |
| 3.1.x | Supported with fallbacks |
| < 3.1 | Not supported (fails fast) |
