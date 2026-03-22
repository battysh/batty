# Getting Started

Use this guide to install Batty, create a team config, launch a tmux session, send the first directive, and stop or resume the team.

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

## Configure

Edit `.batty/team_config/team.yaml`. Start with `name`, `layout`, `roles`, `use_worktrees`, and the `automation` block.

Batty also has an optional `.batty/config.toml` for lower-level runtime defaults,
but team topology, layout, routing, automation, standups, and channel integration all live
in `team.yaml`.

```yaml
name: my-project
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

`batty start --attach` opens tmux instead of printing a summary. Expect something like:

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

Example output:

```text
Team session stopped.
```

The next `batty start` resumes agent sessions from the last stop:

```sh
batty start
```

Example output:

```text
Team session started: batty-my-project
Run `batty attach` to connect.
```

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

## Next Steps

- [Runtime Config Reference](reference/config.md)
- [CLI Reference](reference/cli.md)
