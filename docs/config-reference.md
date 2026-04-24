# Team Config Reference

This page documents the human-edited `team.yaml` surface under
`.batty/team_config/`. For lower-level runtime defaults in `.batty/config.toml`,
see [reference/config.md](reference/config.md).

## Complete Example

```yaml
name: my-project
workspace_type: generic
agent: claude
workflow_mode: hybrid
use_shim: true
use_sdk_mode: true
auto_respawn_on_crash: true
orchestrator_pane: true
orchestrator_position: left
automation_sender: human
external_senders: [slack-bridge]
shim_health_check_interval_secs: 30
shim_health_timeout_secs: 90
shim_shutdown_timeout_secs: 10
shim_working_state_timeout_secs: 1800
pending_queue_max_age_secs: 600
event_log_max_bytes: 5242880
retro_min_duration_secs: 900

board:
  rotation_threshold: 20
  auto_dispatch: true
  worktree_stale_rebase_threshold: 5
  auto_replenish: true
  state_reconciliation_interval_secs: 30
  dispatch_stabilization_delay_secs: 30
  dispatch_dedup_window_secs: 60
  dispatch_manual_cooldown_secs: 30

standup:
  interval_secs: 300
  output_lines: 40

automation:
  timeout_nudges: true
  standups: true
  clean_room_mode: false
  failure_pattern_detection: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  replenishment_threshold: 1
  intervention_idle_grace_secs: 60
  intervention_cooldown_secs: 300
  utilization_recovery_interval_secs: 900
  commit_before_reset: true

workflow_policy:
  clean_room_mode: false
  handoff_directory: .batty/handoff
  wip_limit_per_engineer: 1
  wip_limit_per_reviewer: 2
  pipeline_starvation_threshold: 1
  escalation_threshold_secs: 1800
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  stale_in_progress_hours: 4
  aged_todo_hours: 24
  stale_review_hours: 4
  stall_threshold_secs: 120
  max_stall_restarts: 5
  health_check_interval_secs: 30
  planning_cycle_cooldown_secs: 300
  narration_threshold: 0.8
  narration_nudge_max: 2
  narration_detection_enabled: true
  narration_threshold_polls: 5
  context_pressure_threshold: 100
  context_pressure_threshold_bytes: 512000
  context_pressure_restart_delay_secs: 120
  graceful_shutdown_timeout_secs: 5
  auto_commit_on_restart: true
  uncommitted_warn_threshold: 20
  context_handoff_enabled: true
  handoff_screen_history: 20
  verification:
    max_iterations: 5
    auto_run_tests: true
    require_evidence: true
    test_command: cargo test
  claim_ttl:
    default_secs: 1800
    critical_secs: 900
    max_extensions: 2
    progress_check_interval_secs: 120
    warning_secs: 300
  allocation:
    strategy: scored
    tag_weight: 15
    file_overlap_weight: 10
    load_penalty: 8
    conflict_penalty: 12
    experience_bonus: 3
  auto_merge:
    enabled: true
    max_diff_lines: 200
    max_files_changed: 5
    max_modules_touched: 2
    sensitive_paths: [Cargo.toml, team.yaml, .env]
    confidence_threshold: 0.8
    require_tests_pass: true
    post_merge_verify: true

grafana:
  enabled: true
  port: 3000

roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      provider: openclaw
      target: "123456789"
      allowed_user_ids: [123456789]
    talks_to: [architect]

  - name: architect
    role_type: architect
    agent: claude
    prompt: batty_architect.md
    posture: orchestrator
    model_class: frontier
    talks_to: [human, manager]

  - name: manager
    role_type: manager
    agent: claude
    prompt: batty_manager.md
    posture: orchestrator
    model_class: frontier
    talks_to: [architect, engineer]

  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    prompt: batty_engineer.md
    posture: deep_worker
    model_class: standard
    nudge_interval_secs: 900
    use_worktrees: true
    talks_to: [manager]
```

## Top-Level Fields

| Key                                               | Purpose                                                    |
| ------------------------------------------------- | ---------------------------------------------------------- |
| `name`                                            | Team/session name                                          |
| `agent`                                           | Team-wide default backend when a role does not override it |
| `workflow_mode`                                   | `legacy`, `hybrid`, or `workflow_first`                    |
| `use_shim`                                        | Run members through the managed shim runtime               |
| `use_sdk_mode`                                    | Prefer structured protocols over PTY parsing               |
| `auto_respawn_on_crash`                           | Restart crashed agents automatically                       |
| `orchestrator_pane` / `orchestrator_position`     | Show the orchestration surface in tmux                     |
| `external_senders`                                | Allow non-team sources to message roles                    |
| `shim_*` and `pending_queue_max_age_secs`         | Runtime health and delivery tuning                         |
| `event_log_max_bytes` / `retro_min_duration_secs` | Log and retrospective limits                               |

## `board`

`board` controls the daemon's view of runnable work.

## `workspace_type`

`workspace_type` defaults to `generic`. Set `workspace_type: brazil` only for
Brazil-style multi-repo workspaces where the project root is the workspace
`src/` directory. In Brazil mode, engineer repos are created under sibling
workspace roots like `.batty-brazil/<engineer>/src/<repo>` and Batty runs the
Brazil registration hook when the local `brazil` CLI is available.

- `auto_dispatch`: assign `todo` work to idle engineers automatically
- `auto_replenish`: create planning pressure when backlog runs dry
- `worktree_stale_rebase_threshold`: how many stale-base checks occur before rebase/reset
- `state_reconciliation_interval_secs`: resync daemon state with board ownership
- `dispatch_*`: dedup, cooldown, and stabilization timings

## `automation`

These toggles turn runtime recovery systems on or off.

- `timeout_nudges` and `standups` are timer-based safety nets
- `triage_interventions`, `review_interventions`, and `owned_task_interventions`
  handle common workflow stalls
- `manager_dispatch_interventions` and `architect_utilization_interventions`
  keep the hierarchy moving when reports go idle
- `failure_pattern_detection` feeds long-running reliability analysis
- `commit_before_reset` protects engineer changes before daemon-driven resets

## `workflow_policy`

This block controls execution quality and merge safety.

- `verification`: completion retry limits and test command policy
- `claim_ttl`: stale-ownership reclaim timings
- `allocation`: scored assignment weights
- `main_smoke`: periodic `main` smoke test and dispatch-gate policy
- `auto_merge`: unattended merge thresholds and post-merge verification
- `context_*` and `handoff_*`: context-pressure restart and handoff behavior
- `review_*` and `stale_*`: escalation thresholds for aging work
- `narration_*`: guard rails against agents narrating instead of changing code

`workflow_policy.main_smoke` fields:

- `enabled`: turn periodic `main` smoke checks on or off. Default: `true`
- `interval_secs`: smoke-check cadence. Default: `600`
- `command`: shell command run at the project root. Default: `cargo check`
- `pause_dispatch_on_failure`: stop auto-dispatch while `main` is broken. Default: `true`
- `auto_revert`: optionally revert `HEAD` after a failing smoke run. Default: `false`

## `roles`

Each role entry defines topology and behavior for one role class.

Common fields:

- `name`, `role_type`, `agent`, `instances`
- `prompt`, `talks_to`, `use_worktrees`
- `posture` and `model_class` for template- and model-selection hints
- `channel` and `channel_config` for user-facing endpoints such as Telegram
- `nudge_interval_secs`, `receives_standup`, `standup_interval_secs`
- `provider_overlay` and `instance_overrides` for per-member specialization
- `auth_mode` / `auth_env` when a backend needs explicit auth posture

## Recommended Defaults For Unattended Teams

- Keep `use_shim: true`, `use_sdk_mode: true`, and `auto_respawn_on_crash: true`.
- Leave `board.auto_dispatch: true` and `workflow_policy.auto_merge.enabled: true`.
- Enable `use_worktrees: true` for engineers.
- Use `workflow_mode: hybrid` unless you are intentionally forcing workflow-first behavior.
