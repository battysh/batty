# Configuration Reference

This reference covers both `.batty/config.toml` (runtime defaults) and
`.batty/team_config/team.yaml` (team topology, automation, and workflow policy).

## Location

Optional runtime defaults are read from `.batty/config.toml` when the file is present. Team topology and day-to-day runtime settings live in `.batty/team_config/team.yaml`.

## Fields

| Key                                 | Type                               | Default                       | Description                                                             |
| ----------------------------------- | ---------------------------------- | ----------------------------- | ----------------------------------------------------------------------- |
| `defaults.agent`                    | string                             | `claude`                      | Default agent name for runtime paths that consult `.batty/config.toml`. |
| `defaults.policy`                   | enum (`observe`, `suggest`, `act`) | `observe`                     | Default policy tier for prompt handling.                                |
| `defaults.dod`                      | string or null                     | `(none)`                      | Definition of done command run after task completion.                   |
| `defaults.max_retries`              | integer                            | `3`                           | Maximum retries for failed DoD commands.                                |
| `supervisor.enabled`                | boolean                            | `true`                        | Enable Tier 2 supervisor escalation.                                    |
| `supervisor.program`                | string                             | `claude`                      | Program used for supervisor calls.                                      |
| `supervisor.args`                   | array[string]                      | `[-p, --output-format, text]` | Arguments passed to the supervisor program.                             |
| `supervisor.timeout_secs`           | integer                            | `60`                          | Supervisor command timeout in seconds.                                  |
| `supervisor.trace_io`               | boolean                            | `true`                        | Log supervisor prompts and responses for debugging.                     |
| `detector.silence_timeout_secs`     | integer                            | `3`                           | Silence threshold before unknown-request fallback triggers.             |
| `detector.answer_cooldown_millis`   | integer                            | `1000`                        | Minimum delay between automatic answers.                                |
| `detector.unknown_request_fallback` | boolean                            | `true`                        | Escalate unresolved output to supervisor when no known prompt matches.  |
| `detector.idle_input_fallback`      | boolean                            | `true`                        | Allow idle-output input prompts to trigger response handling.           |
| `dangerous_mode.enabled`            | boolean                            | `false`                       | Enable dangerous-mode flags for supported agent wrappers.               |
| `policy.auto_answer`                | table[string -> string]            | `{}`                          | Prompt-to-answer overrides for runtime paths that use this config.      |

## Default Template

```toml
[defaults]
agent = "claude"
policy = "observe"
max_retries = 3

[supervisor]
enabled = true
program = "claude"
args = ["-p", "--output-format", "text"]
timeout_secs = 60
trace_io = true

[detector]
silence_timeout_secs = 3
answer_cooldown_millis = 1000
unknown_request_fallback = true
idle_input_fallback = true

[dangerous_mode]
enabled = false

[policy.auto_answer]
"Continue? [y/n]" = "y"
```

---

## Team Config (`team.yaml`)

Team topology, automation, and workflow policy live in `.batty/team_config/team.yaml`.

### Top-Level Fields

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string | (required) | Team name, used as the tmux session name prefix. |
| `workflow_mode` | enum (`legacy`, `hybrid`, `workflow_first`) | `legacy` | Controls whether the legacy task loop, workflow state model, or both are active. |
| `orchestrator_pane` | boolean | `true` | Show the orchestrator log pane in the tmux layout. |
| `orchestrator_position` | enum (`bottom`, `left`) | `bottom` | Position of the orchestrator pane. |
| `external_senders` | array[string] | `[]` | Non-team senders (e.g. `email-router`, `slack-bridge`) allowed to message any role. |
| `event_log_max_bytes` | integer | `10485760` (10 MB) | Maximum size of `events.jsonl` before rotation. |
| `retro_min_duration_secs` | integer | `60` | Minimum run duration before a retrospective is generated. |

### `board`

| Key | Type | Default | Description |
|---|---|---|---|
| `board.rotation_threshold` | integer | `20` | Number of done tasks before the board rotates. |
| `board.auto_dispatch` | boolean | `true` | Enable auto-dispatch of board tasks to idle engineers. |
| `board.dispatch_stabilization_delay_secs` | integer | `30` | Cooldown between auto-dispatch attempts. |

### `standup`

| Key | Type | Default | Description |
|---|---|---|---|
| `standup.interval_secs` | integer | `300` (5 min) | Interval between periodic standups. |
| `standup.output_lines` | integer | `30` | Number of tail output lines included per agent in standups. |

### `automation`

| Key | Type | Default | Description |
|---|---|---|---|
| `automation.timeout_nudges` | boolean | `true` | Nudge idle agents after silence threshold. |
| `automation.standups` | boolean | `true` | Enable periodic standup generation. |
| `automation.failure_pattern_detection` | boolean | `true` | Detect rolling failure patterns and notify. |
| `automation.triage_interventions` | boolean | `true` | Intervene when delivered results need lead action. |
| `automation.review_interventions` | boolean | `true` | Intervene when completed work awaits manager review. |
| `automation.owned_task_interventions` | boolean | `true` | Recover idle members who still own active work. |
| `automation.manager_dispatch_interventions` | boolean | `true` | Prompt idle managers with idle reports and open work. |
| `automation.architect_utilization_interventions` | boolean | `true` | Prompt idle architects when the team is underloaded. |
| `automation.replenishment_threshold` | integer or null | `null` | Backlog item count that triggers replenishment. |
| `automation.intervention_idle_grace_secs` | integer | `60` | Grace period before an agent is considered idle. |
| `automation.intervention_cooldown_secs` | integer | `120` | Minimum seconds between interventions on the same agent. |

### `workflow_policy`

| Key | Type | Default | Description |
|---|---|---|---|
| `workflow_policy.wip_limit_per_engineer` | integer or null | `null` | Max concurrent in-progress tasks per engineer. |
| `workflow_policy.wip_limit_per_reviewer` | integer or null | `null` | Max concurrent review items per reviewer. |
| `workflow_policy.pipeline_starvation_threshold` | integer or null | `1` | Minimum todo items before starvation alerts fire. |
| `workflow_policy.escalation_threshold_secs` | integer | `3600` (1 h) | Time before a blocked task is escalated. |
| `workflow_policy.review_nudge_threshold_secs` | integer | `1800` (30 min) | Time before a stale review gets a nudge. |
| `workflow_policy.review_timeout_secs` | integer | `7200` (2 h) | Time before a stale review is escalated. |
| `workflow_policy.auto_archive_done_after_secs` | integer or null | `null` | Auto-archive done tasks after this duration. |
| `workflow_policy.capability_overrides` | map[string -> array[string]] | `{}` | Override capability grants per role. |

### `workflow_policy.auto_merge`

| Key | Type | Default | Description |
|---|---|---|---|
| `auto_merge.enabled` | boolean | `false` | Enable the auto-merge policy engine. |
| `auto_merge.max_diff_lines` | integer | `200` | Max total diff lines (added + removed) for auto-merge. |
| `auto_merge.max_files_changed` | integer | `5` | Max changed files for auto-merge. |
| `auto_merge.max_modules_touched` | integer | `2` | Max top-level `src/` modules touched for auto-merge. |
| `auto_merge.sensitive_paths` | array[string] | `[Cargo.toml, team.yaml, .env]` | Paths that always require manual review. |
| `auto_merge.confidence_threshold` | float | `0.8` | Minimum confidence score to proceed with auto-merge. |
| `auto_merge.require_tests_pass` | boolean | `true` | Require all tests to pass before auto-merge. |

### `cost`

| Key | Type | Default | Description |
|---|---|---|---|
| `cost.models` | map[string -> ModelPricing] | `{}` | Per-model token pricing for `batty cost` estimates. |

Each `ModelPricing` entry supports: `input_usd_per_mtok`, `cached_input_usd_per_mtok`, `cache_read_input_usd_per_mtok`, `output_usd_per_mtok`, `reasoning_output_usd_per_mtok`.

### `roles[]`

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string | (required) | Role name, used as the instance prefix. |
| `role_type` | enum (`user`, `architect`, `manager`, `engineer`) | (required) | Determines hierarchy position and capabilities. |
| `agent` | string or null | `null` | Agent CLI to launch (e.g. `claude`, `codex`, `aider`). |
| `instances` | integer | `1` | Number of instances to create for this role. |
| `prompt` | string or null | `null` | Prompt template filename in `team_config/`. |
| `talks_to` | array[string] | `[]` | Roles this member can message directly. |
| `channel` | string or null | `null` | Communication channel (`telegram` or null for tmux). |
| `channel_config` | object or null | `null` | Channel-specific config (see Telegram section). |
| `nudge_interval_secs` | integer or null | `null` | Override default nudge interval for this role. |
| `receives_standup` | boolean or null | `null` | Override whether this role receives standups. |
| `standup_interval_secs` | integer or null | `null` | Override standup interval for this role. |
| `use_worktrees` | boolean | `false` | Create isolated git worktrees for this role's instances. |

### Example `team.yaml`

```yaml
name: my-project
external_senders: [email-router]
retro_min_duration_secs: 120
automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
workflow_policy:
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  auto_merge:
    enabled: true
    max_diff_lines: 200
    max_files_changed: 5
    confidence_threshold: 0.8
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
