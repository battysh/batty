# CLI Reference

This page is the operator-facing guide to Batty's command surface. For the
exhaustive clap-generated output, see [reference/cli.md](reference/cli.md).

## Core Session Commands

| Command | Purpose |
| --- | --- |
| `batty init` | Scaffold `.batty/team_config/` with a team template and prompts |
| `batty start` | Launch the daemon and tmux session |
| `batty attach` | Attach to the running tmux session |
| `batty status` | Show current member state and hierarchy |
| `batty stop` | Stop the daemon and tmux session |
| `batty validate --show-checks` | Validate `team.yaml` with per-check output |

## Team Communication

| Command | Purpose |
| --- | --- |
| `batty send <role> <message>` | Deliver a directive or response to a role |
| `batty inbox <member>` | List inbox messages for a member |
| `batty read <member> <id>` | Read one inbox message |
| `batty ack <member> <id>` | Mark a delivered inbox message as acknowledged |
| `batty chat --agent-type <backend>` | Talk to a single shim-backed agent interactively |

## Workflow Commands

| Command | Purpose |
| --- | --- |
| `batty board list --status todo` | Inspect runnable backlog |
| `batty board summary` | Quick status counts by workflow state |
| `batty board health` | Detect stale tasks, blocked work, and dependency issues |
| `batty board archive --older-than 7d` | Move old done tasks out of the active board |
| `batty queue` | Inspect pending dispatch work |
| `batty review <id> <disposition>` | Record approve/request-changes/reject decisions |
| `batty task schedule <id> --at ... --cron ...` | Delay or recur a task |
| `batty merge <engineer>` | Merge an engineer branch manually |

## Observability

| Command | Purpose |
| --- | --- |
| `batty metrics` | Consolidated throughput dashboard |
| `batty telemetry summary` | Session-level telemetry summary |
| `batty telemetry agents` | Per-agent runtime metrics |
| `batty telemetry tasks` | Task lifecycle metrics |
| `batty retro` | Generate a retrospective report |
| `batty load` | Team utilization and recent load |
| `batty cost` | Cost estimate from session artifacts |
| `batty grafana setup|status|open` | Manage the built-in Grafana dashboard |

## Runtime Controls

| Command | Purpose |
| --- | --- |
| `batty pause` / `batty resume` | Pause or resume automation timers |
| `batty nudge status` | Show enabled intervention classes |
| `batty nudge disable <name>` | Turn off one intervention without restart |
| `batty scale` | Change live team topology |
| `batty doctor --fix` | Inspect and clean orphaned runtime state |

## Configuration And Export

| Command | Purpose |
| --- | --- |
| `batty config` | Show resolved configuration |
| `batty export-template` | Export current team config as a reusable template |
| `batty export-run` | Snapshot runtime state for debugging |
| `batty completions <shell>` | Generate shell completions |
| `batty telegram` | Configure Telegram human communication |

## Typical Day-One Flow

```sh
cargo install batty-cli
batty init --template squad --agent codex
batty validate --show-checks
batty start
batty attach
```

In a second shell:

```sh
batty send architect "Build the first milestone and keep tests green."
batty status
batty board health
```
