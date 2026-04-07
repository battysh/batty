# Getting Started

Use this guide to install Batty, create a team config, launch the daemon, and
get to the unattended v0.10.0 happy path quickly.

## Prerequisites

- Rust 1.85+
- `tmux >= 3.1` (3.2+ recommended)
- `kanban-md` on your `PATH`
- At least one supported agent CLI such as `claude`, `codex`, or `kiro`

Install `kanban-md` if needed:

```sh
cargo install kanban-md --locked
```

## 1. Install Batty

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

## 2. Initialize A Team

Run `batty init` inside the repository you want Batty to manage:

```sh
cd my-project
batty init
```

Useful variants:

```sh
batty init --template squad
batty init --template batty --agent codex
batty init --template cleanroom
```

The generated `.batty/team_config/` directory contains `team.yaml`, prompt
templates, and the board directory.

## 3. Configure `team.yaml`

Batty v0.10.0 is designed around a shim-backed, SDK-first runtime. Start with a
team that looks like this:

```yaml
name: my-project
workflow_mode: hybrid
use_shim: true
use_sdk_mode: true
auto_respawn_on_crash: true

board:
  auto_dispatch: true
  auto_replenish: true

automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true

workflow_policy:
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  context_handoff_enabled: true
  verification:
    auto_run_tests: true
    require_evidence: true
    test_command: cargo test
  auto_merge:
    enabled: true
    require_tests_pass: true

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

Recommended defaults:

- Keep `use_shim: true` and `use_sdk_mode: true`.
- Leave `auto_respawn_on_crash: true` on for unattended teams.
- Keep `board.auto_dispatch: true` so idle engineers do not sit waiting.
- Keep `workflow_policy.auto_merge.enabled: true` so safe diffs merge on green.

For a fuller example, see [config-reference.md](config-reference.md).

## 4. Validate Before Starting

```sh
batty validate
batty validate --show-checks
```

Use `--show-checks` when you want the failing validation rule, not just the
top-level error.

## 5. Start And Attach

Start the daemon:

```sh
batty start
```

Attach to the tmux session:

```sh
batty attach
```

In a second shell, keep a non-interactive view open:

```sh
batty status
```

## 6. Send The First Directive

```sh
batty send architect "Build a small JSON API with auth, tests, and CI."
```

The architect plans, the manager creates and routes tasks, and engineers pick
up runnable work through the daemon-managed board loop.

## 7. Understand The Happy Path

In v0.10.0, the intended steady state is:

1. Architect defines the objective.
2. Manager turns it into board tasks.
3. Idle engineers receive work via auto-dispatch.
4. Engineers execute inside isolated worktrees.
5. Verification runs on completion.
6. Small, safe diffs auto-merge; larger ones route to review.

Watch that flow with:

```sh
batty board summary
batty board health
batty queue
batty metrics
```

## 8. Monitor A Running Team

These are the everyday operational commands:

```sh
batty status
batty inbox architect
batty board summary
batty doctor
batty telemetry summary
batty grafana status
```

- `batty status` shows liveness and hierarchy.
- `batty board summary` shows workflow pressure.
- `batty doctor` is the first stop when the team looks stuck.
- `batty telemetry` and `batty metrics` show throughput and review health.

## 9. Optional: Telegram Control Plane

If your config includes a `user` role, run:

```sh
batty telegram
```

The setup flow validates the bot configuration and writes the resulting channel
settings into `team.yaml` unless you prefer to keep the token in
`BATTY_TELEGRAM_BOT_TOKEN`.

## 10. Stop And Resume

Stop the daemon and tmux session:

```sh
batty stop
```

Restart later with:

```sh
batty start
```

Batty attempts session resume when the saved launch identity still matches. If a
saved session is stale, Batty falls back to a cold respawn and rebuilds context
from on-disk state instead of blocking startup.

## Troubleshooting

- If an agent stalls, check `batty status`, `batty doctor`, and `.batty/daemon.log`.
- If builds contend, make sure engineers are using shared-target worktrees rather
  than ad hoc local `target/` directories.
- If messages do not arrive, inspect `batty inbox <member>` and verify the daemon
  is still running.
- If auth is broken, prefer current CLI OAuth flows and then restart Batty.

For operational fixes, see [troubleshooting.md](troubleshooting.md).

## Next Steps

- Read [cli-reference.md](cli-reference.md) for the main operator commands.
- Read [config-reference.md](config-reference.md) for the full `team.yaml` surface.
- Use `batty metrics`, `batty telemetry`, and `batty grafana status` on longer runs.
