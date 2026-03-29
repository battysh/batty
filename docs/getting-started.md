# Getting Started

Use this guide to install Batty, create a team config, launch a shim-driven team session, send the first directive, and stop or resume the team.

## Prerequisites

- Rust 1.85+
- `tmux`
- `kanban-md`
- At least one agent CLI on your `PATH` (`claude`, `codex`, or similar)

```sh
cargo install kanban-md --locked
```

## Install

Install Batty from crates.io:

```sh
cargo install batty-cli
```

Or build from source:

```sh
git clone https://github.com/battysh/batty.git
cd batty
cargo install --path .
```

## Initialize

Run `batty init` from the repository you want Batty to manage.

```sh
cd my-project
batty init
```

Example output:

```text
Initialized team config (5 files):
  /path/to/my-project/.batty/team_config/team.yaml
  /path/to/my-project/.batty/team_config/architect.md
  /path/to/my-project/.batty/team_config/manager.md
  /path/to/my-project/.batty/team_config/engineer.md
  /path/to/my-project/.batty/team_config/board

Edit .batty/team_config/team.yaml to configure your team.
Then run: batty start
```

If you want a different scaffold, use `batty init --template solo|pair|simple|squad|large|research|software|batty`.

To set the default agent backend for all roles at init time:

```sh
batty init --agent codex
batty init --template squad --agent claude
```

Supported backends include `claude`, `codex`, and `kiro`. You can override individual roles later in `team.yaml`.

## Agent Runtime: Shim Mode

Batty supports a shim runtime for agent execution. When `use_shim: true` is
set in `team.yaml`, each agent runs as a `batty shim` subprocess instead of
being driven directly through a tmux pane. The shim owns the PTY, classifies
agent state automatically, and communicates with the daemon over a structured
socket protocol. Tmux panes become display-only surfaces that tail the shim's
PTY log.

Shim mode is the recommended runtime because it gives you reliable state
detection, structured message delivery, and cleaner shutdown behavior. Add
this to your `team.yaml`:

```yaml
use_shim: true
```

You can also chat with a single agent interactively using the shim protocol:

```sh
batty chat --agent-type claude
```

See [Configuration Reference](reference/config.md) for details on shim-related
config fields.

## Configure

Edit `.batty/team_config/team.yaml`. Start with `name`, `layout`, `roles`, `use_worktrees`, and the `automation` block.

Batty also has an optional `.batty/config.toml` for lower-level runtime defaults,
but team topology, layout, routing, automation, standups, and channel integration all live
in `team.yaml`.

```yaml
name: my-project
use_shim: true
auto_respawn_on_crash: true
automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
layout:
  zones:
    - name: architect
      width_pct: 30
    - name: engineers
      width_pct: 70
      split: { horizontal: 3 }
roles:
  - name: architect
    role_type: architect
    agent: claude
    prompt: architect.md
  - name: manager
    role_type: manager
    agent: claude
    prompt: manager.md
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    prompt: engineer.md
    use_worktrees: true
```

`auto_respawn_on_crash` defaults to `true` when omitted. Keep it that way for
unattended teams so crashed shim-backed agents are relaunched automatically.
Set it to `false` only when you are actively debugging crashes or you want to
supervise recovery manually.

Validate before you start:

```sh
batty validate
```

Example output:

```text
Config: /path/to/my-project/.batty/team_config/team.yaml
Team: my-project
Roles: 3
Total members: 5
Valid.
```

Add `--show-checks` to see individual pass/fail status for each validation rule:

```sh
batty validate --show-checks
```

The `automation` block controls which daemon behaviors are active. In most teams,
the reactive interventions should stay enabled:

- `triage_interventions` for delivered direct-report results that still need lead action
- `review_interventions` for completed work waiting on manager review
- `owned_task_interventions` for idle members who still own active work
- `manager_dispatch_interventions` for idle managers with idle reports and open lane work
- `architect_utilization_interventions` for idle architects while the team is underloaded

`timeout_nudges` and `standups` are still useful, but they are fallback safety nets rather
than the main control loop.

If `use_worktrees: true` is enabled for engineers, Batty keeps one stable
worktree per engineer at `.batty/worktrees/<engineer>`. New assignments reuse
that path but switch the engineer onto a fresh task branch from current `main`.
After merge, the engineer returns to `eng-main/<engineer>`.

## Launch

Start the daemon and attach to tmux immediately:

```sh
batty start --attach
```

`batty start --attach` opens tmux instead of printing a summary. In shim mode,
those panes show each shim-backed agent session. Expect something like:

```text
┌ architect ─────────────┬ manager ───────────────┬ eng-1-1 ───────────────┐
│ role prompt loaded     │ role prompt loaded     │ codex/claude starting  │
│ waiting for directive  │ waiting for architect  │ waiting for assignment  │
├────────────────────────┼────────────────────────┼ eng-1-2 ───────────────┤
│                        │                        │ waiting for assignment  │
├────────────────────────┼────────────────────────┼ eng-1-3 ───────────────┤
│                        │                        │ waiting for assignment  │
└────────────────────────┴────────────────────────┴─────────────────────────┘
```

## Send A Directive

From another shell, send the architect the first goal:

```sh
batty send architect "Implement a small JSON API with auth and tests."
```

Example output:

```text
Message queued for architect.
```

## Monitor

Check the team without attaching:

```sh
batty status
```

Example output:

```text
Team: my-project
Session: batty-my-project (running)

MEMBER               ROLE         AGENT      REPORTS TO
--------------------------------------------------------------
architect            architect    claude     -
manager              manager      claude     architect
eng-1-1              engineer     codex      manager
eng-1-2              engineer     codex      manager
eng-1-3              engineer     codex      manager
```

Use these while the team runs:

```sh
batty attach
batty inbox architect
batty board
batty board summary
batty doctor
```

`batty board summary` is the quickest non-interactive snapshot of backlog vs. in-progress vs. review work, while `batty doctor` dumps the launch state, board health, worktree status, and daemon-derived checks when the team looks stuck.

If a member has queued messages, `batty inbox architect` looks like:

```text
STATUS   FROM         TYPE         ID       BODY
------------------------------------------------------------------------
pending  human        send         a1b2c3d4 Implement a small JSON API with auth...
```

## Stop And Resume

Stop the daemon and tmux session:

```sh
batty stop
```

On stop, Batty prints a session summary with task counts, cycle times, and agent
uptime before exiting.

Example output:

```text
Session summary: 12 tasks completed, avg cycle 8m, 3h uptime
Team session stopped.
```

The next `batty start` attempts to resume agent sessions from the last stop:

```sh
batty start
```

Example output:

```text
Team session started: batty-my-project
Run `batty attach` to connect.
```

If a saved session is stale or missing, Batty downgrades that member to a cold
respawn, rebuilds context from the worktree and task state, and continues
starting the team. It does not proactively restart healthy live panes during
startup; only panes that are already dead are respawned.

## Telegram

If you want a human endpoint over Telegram, use a template that includes a
`user` role, or add one manually, then run:

```sh
batty telegram
```

The setup wizard will:

1. Validate your bot token with the Telegram Bot API
1. Ask for your numeric Telegram user ID
1. Optionally send a test message
1. Update `.batty/team_config/team.yaml`

Typical resulting config:

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

If you do not want the token stored in `team.yaml`, set
`BATTY_TELEGRAM_BOT_TOKEN` in the environment and remove `bot_token` from the
file.

Restart the daemon after setup:

```sh
batty stop
batty start
```

## Auto-Merge Policy

Batty can auto-merge small, low-risk engineer branches without waiting for manual
review. Enable it in `team.yaml`:

```yaml
workflow_policy:
  auto_merge:
    enabled: true
    max_diff_lines: 200
    max_files_changed: 5
    max_modules_touched: 2
    sensitive_paths: [Cargo.toml, team.yaml, .env]
    confidence_threshold: 0.8
    require_tests_pass: true
```

When a task completes, the daemon scores the diff. If all thresholds are met and
no sensitive files are touched, the branch merges automatically. Otherwise it
routes to manual review.

## Review CLI

Record a review disposition from the command line:

```sh
batty review 42 approve
batty review 42 request-changes "Fix the error handling in parse_config"
batty review 42 reject "Wrong approach, see the architecture doc"
```

The `--reviewer` flag defaults to `human`. Use it when a manager or architect is
recording the review programmatically.

## Review Timeout Escalation

Stale reviews are nudged and eventually escalated. Configure the thresholds in
`workflow_policy`:

```yaml
workflow_policy:
  review_nudge_threshold_secs: 1800   # nudge reviewer after 30 min
  review_timeout_secs: 7200           # escalate after 2 hours
```

After the nudge threshold, the reviewer gets a reminder. After the timeout, the
task is escalated so it does not block the pipeline.

Per-priority overrides let you shorten timeouts for critical work:

```yaml
workflow_policy:
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  review_timeout_overrides:
    critical:
      review_nudge_threshold_secs: 300
      review_timeout_secs: 600
    high:
      review_timeout_secs: 3600
```

When a task's priority matches a key in `review_timeout_overrides`, those values
take precedence over the top-level defaults. Omitted fields fall back to the
global values.

## External Senders

Non-team sources (email routers, Slack bridges, CI bots) can deliver messages to
any role by listing them in `external_senders`:

```yaml
external_senders:
  - email-router
  - slack-bridge
```

Messages from listed senders bypass the `talks_to` graph and land in the target
role's inbox.

## Non-Git-Repo Support

Batty runs in directories that are not git repositories. Git-dependent operations
(branch creation, worktree management, merge) degrade gracefully with clear
warnings instead of crashing. This is useful for planning-only teams or
documentation projects.

## Diagnostics

`batty doctor` dumps launch state, board health, worktree status, and daemon
checks. Add `--fix` to clean up orphan worktrees and branches left by previous
runs:

```sh
batty doctor          # read-only diagnostic dump
batty doctor --fix    # interactive cleanup of orphans
batty doctor --fix --yes  # skip confirmation prompt
```

## Scheduled Tasks

Tasks can be delayed or set to recur on a schedule using two frontmatter fields:

- `scheduled_for` — an RFC 3339 timestamp. The dispatcher will not assign the task until this time has passed.
- `cron_schedule` — a cron expression. When a task with `cron_schedule` reaches done, the daemon automatically recycles it back to todo for the next run.

Use the `batty task schedule` command to set these fields:

```sh
# Delay dispatch until a specific time
batty task schedule 42 --at '2026-03-25T09:00:00-04:00'

# Make a task recur every Monday at 9 AM
batty task schedule 42 --cron '0 9 * * MON'

# Set both a first-run time and a recurring schedule
batty task schedule 42 --at '2026-03-25T09:00:00-04:00' --cron '0 9 * * MON'

# Clear all scheduling fields
batty task schedule 42 --clear
```

The cron recycler runs as part of the daemon poll loop. When it finds a done task
with a `cron_schedule`, it moves the task back to todo, updates `cron_last_run`,
and emits a `task_recycled` event.

For the full guide including cron expression examples, missed-trigger behavior, and frontmatter field reference, see [Scheduled Tasks and Cron Recurrence](scheduled-tasks.md).

## Nudge CLI

The daemon runs several intervention types: replenish, triage, review, dispatch,
utilization, and owned-task. You can disable or re-enable any of these at runtime
without restarting the daemon:

```sh
# Disable the replenish intervention
batty nudge disable replenish

# Re-enable it
batty nudge enable replenish

# Show which interventions are currently enabled or disabled
batty nudge status
```

Available interventions: `replenish`, `triage`, `review`, `dispatch`,
`utilization`, `owned-task`.

Disabled interventions stay off until you re-enable them or restart the daemon
(all interventions reset to enabled on startup).

## Telemetry

Batty records agent and task metrics in a SQLite database at
`.batty/telemetry.db`. Query it with the `batty telemetry` subcommands:

```sh
batty telemetry summary     # session-level summaries
batty telemetry agents      # per-agent performance metrics
batty telemetry tasks       # per-task lifecycle metrics
batty telemetry reviews     # review pipeline: auto-merge rate, rework, latency
batty telemetry events      # recent events from the telemetry database
```

Telemetry is written automatically by the daemon alongside the existing
`events.jsonl` log. No configuration is needed.

## Retrospectives

After a run, generate a Markdown retrospective that analyzes task throughput,
review stall durations, rework rates, and failure patterns:

```sh
batty retro
```

The report is written to `.batty/retro/` and printed to stdout. Use
`--events <path>` to point at a different events file.

## Team Templates

Export your current team config as a reusable template:

```sh
batty export-template my-team
```

This saves the config to `~/.batty/templates/my-team/`. Restore it in another
project:

```sh
cd new-project
batty init --from my-team
```

Built-in templates (`solo`, `pair`, `simple`, `squad`, `large`, `research`,
`software`, `batty`) are always available via `batty init --template <name>`.

## Stall Detection And Auto-Restart

When an agent stops producing output for longer than the configured threshold, the
daemon treats it as stalled. Stalled agents are automatically restarted with
exponential backoff:

```yaml
workflow_policy:
  stall_threshold_secs: 300    # seconds of silence before an agent is stalled (default: 300)
  max_stall_restarts: 2        # maximum restart attempts before escalation (default: 2)
```

Before restarting, the daemon writes a progress checkpoint file into the
engineer's worktree so the restarted agent can resume with prior task context.

The daemon also monitors agent backend health (e.g., whether the `claude` or
`codex` binary is responsive). Backend health status surfaces in `batty status`
output and periodic standups. Configure the health check interval:

```yaml
workflow_policy:
  health_check_interval_secs: 60   # how often the daemon checks backend health (default: 60)
```

## Task Estimation

Batty estimates remaining time for in-progress tasks using historical cycle
times from the telemetry database. It groups completed tasks by their tag set
and computes a median duration. When `batty status` runs, each active member
shows an ETA column derived from the best-matching tag set.

No additional configuration is needed — estimation works automatically once
the telemetry database has enough completed task data.

## Board Dependencies

Tasks can declare dependencies using a `depends_on` list in their frontmatter:

```yaml
---
id: 42
title: Implement feature X
depends_on: [40, 41]
---
```

Visualize the dependency graph:

```sh
batty board deps               # default tree format
batty board deps --format flat # flat list of edges
batty board deps --format dot  # Graphviz DOT output
```

The dispatcher respects dependencies — a task will not be auto-assigned until
all its `depends_on` tasks have reached `done`.

## Worktree Reconciliation

When branches are merged via cherry-pick instead of a fast-forward merge, the
original branch still looks unmerged to `git branch --merged`. Batty detects
this situation automatically using `git cherry` and resets the worktree to the
base branch so the next assignment starts clean.

This runs during the daemon poll loop. No configuration is needed — if all
commits on an engineer's branch have been cherry-picked onto main, the worktree
is reset without manual intervention.

## Pending Delivery Queue

Messages sent to agents that are still starting (never been ready) are buffered
in a pending delivery queue instead of being dropped to inbox. When the daemon
detects that the agent has transitioned to the Ready state, all queued messages
are delivered automatically. This prevents lost messages during agent startup.

## Board Archive

Move completed tasks out of the active board into an archive directory:

```sh
batty board archive                           # archive all done tasks
batty board archive --older-than 2026-03-01   # archive tasks completed before a date
```

Archived tasks are moved to `.batty/team_config/board/archive/` and no longer
appear in `batty board` output. This keeps the active board focused on current
work.

## Uncommitted Work Warning

The daemon monitors engineer worktrees for uncommitted changes. When an
engineer has more uncommitted diff lines than the configured threshold, the
manager receives a warning:

```yaml
workflow_policy:
  uncommitted_warn_threshold: 200   # diff lines before warning (default: 200)
```

This prevents engineers from losing work on worktree resets.

## Dispatch Backoff

After a manual task dispatch, the auto-dispatcher pauses for a configurable
cooldown to avoid re-dispatching work that is already being assigned:

```yaml
board:
  dispatch_manual_cooldown_secs: 30   # cooldown after manual dispatch (default: 30)
```

## Mixed-Backend Teams

Different roles can use different agent backends. Set a team-level default and
override it per role:

```yaml
agent: claude                    # team-level default
roles:
  - name: architect
    role_type: architect
    agent: claude                # uses team default
    prompt: architect.md
  - name: engineer
    role_type: engineer
    agent: codex                 # overrides team default
    instances: 3
    prompt: engineer.md
```

Resolution order: role-level `agent` > team-level `agent` > `"claude"` (hardcoded
fallback).

## False-Done Prevention

When an engineer reports completion, the daemon verifies that the worktree
branch actually has commits beyond `main`. If no new commits are found, the
completion is rejected and the task stays in progress. This prevents empty
branches from being merged.

## Grafana Monitoring

Batty includes a bundled Grafana dashboard with 21 panels and 6 pre-configured
alerts. The `batty grafana` commands handle installation, service management,
and browser access.

### Setup

Install Grafana, the SQLite datasource plugin, and start the service:

```sh
batty grafana setup
```

This runs three steps:

1. `brew install grafana`
1. `grafana-cli plugins install frser-sqlite-datasource`
1. `brew services start grafana`

Once Grafana is running, start your team as usual:

```sh
batty start
```

The daemon writes telemetry to `.batty/telemetry.db` (SQLite), which the
dashboard queries via the SQLite datasource plugin.

### Verify

Check that the Grafana server is reachable:

```sh
batty grafana status
```

Example output:

```text
Grafana is running at http://localhost:3000
{"commit":"...","database":"ok","version":"..."}
```

### Open The Dashboard

Open the Grafana dashboard in your default browser:

```sh
batty grafana open
```

This opens `http://localhost:3000`. Import the bundled dashboard JSON
(`src/team/grafana/dashboard.json`) via **Dashboards > Import** and point the
datasource at your `.batty/telemetry.db` file.

### What The Dashboard Shows

The dashboard has 6 rows:

- **Session Overview** — active agents, session uptime, team utilization
- **Pipeline Health** — task flow rates, queue depths, starvation indicators
- **Agent Performance** — per-agent task counts, cycle times, failure rates
- **Delivery & Communication** — message delivery success rates, latency
- **Task Lifecycle** — time-in-status breakdowns, rework rates
- **Recent Activity** — live event stream

### Alerts

Six alerts fire automatically when thresholds are crossed:

1. **Agent Stall** — an agent has stopped producing output
1. **Delivery Failure Spike** — message delivery error rate is elevated
1. **Pipeline Starvation** — no tasks are flowing through the pipeline
1. **High Failure Rate** — task failure rate exceeds normal levels
1. **Context Exhaustion** — an agent's context window is nearly full
1. **Session Idle** — the session has been idle for too long

Alert thresholds are configurable inside the dashboard JSON.

### Custom Port

By default, Grafana runs on port 3000. Override it in `team.yaml`:

```yaml
grafana:
  port: 9090
```

All `batty grafana` commands respect this setting.

## Metrics Dashboard

`batty metrics` shows a consolidated view of task throughput, cycle times, rates,
and per-agent performance from the telemetry database:

```sh
batty metrics
```

This is a quick alternative to `batty telemetry` when you want a single-screen
overview instead of querying individual metric categories.

## Team Load And Cost

Estimate team utilization and session spending:

```sh
batty load    # show team load and recent load history
batty cost    # estimate current run cost from agent session files
```

`batty load` shows how many engineers are active vs. idle and the recent load
trend. `batty cost` reads agent session files to estimate API spending for the
current run.

## Board Health

`batty board health` shows a dashboard of board status counts, stale tasks,
and dependency issues:

```sh
batty board health
```

Use this when the board feels stuck or when you suspect dependency cycles are
blocking dispatch.

## Inbox Purge

Clean up delivered messages from inbox directories:

```sh
batty inbox purge eng-1-1              # purge all delivered for one role
batty inbox purge --all-roles          # purge across all inboxes
batty inbox purge eng-1-1 --older-than 7d  # only messages older than 7 days
```

This keeps inbox directories from growing unbounded during long runs.

## Failure Pattern Detection

The daemon monitors task outcomes over a rolling window and detects recurring
failure patterns. When failure counts exceed the configured threshold, a
notification is emitted so the team can investigate systemic issues rather than
retrying indefinitely.

No additional configuration is needed — pattern detection runs automatically as
part of the daemon poll loop.

## Daemon Auto-Archive

The daemon automatically archives completed tasks when the active board exceeds
a size threshold. This keeps `batty board` responsive during long runs without
requiring manual `batty board archive` invocations.

## Next Steps

- [Runtime Config Reference](reference/config.md)
- [CLI Reference](reference/cli.md)
- [Intervention System](interventions.md)
