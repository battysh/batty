# Orchestrator Guide

Batty's orchestrator is the daemon-driven control plane that keeps a team moving without turning the workflow into a black box. It watches board state, member state, inbox backlog, recent failures, and team load, then decides when to dispatch work, send recovery prompts, emit reports, or stay out of the way.

This page explains the shipped orchestrator surface: when it is active, what it automates, how it records actions, and which `team.yaml` keys control it.

## Overview

The orchestrator runs inside the main daemon loop. On each pass it can:

- auto-dispatch board work to idle engineers
- detect direct-report triage backlog and intervention conditions
- detect review backlog for idle managers
- emit periodic standups
- track rolling failure signatures from the event log
- generate a retrospective once a run is fully complete
- send idle nudges with countdown-based timing
- update pane-border status labels with state and timers
- append human-readable actions to the orchestrator log

The orchestrator is only considered enabled when both of these are true:

- `workflow_mode` is `hybrid` or `workflow_first`
- `orchestrator_pane` is `true`

If either condition is false, the legacy runtime still works, but the explicit orchestrator pane/log surface is off.

## Workflow Modes

Batty supports three rollout modes:

- `legacy`: preserves the older runtime behavior. Use this for existing teams that are not yet relying on workflow metadata as the control surface.
- `hybrid`: enables the orchestrator surface while keeping legacy behavior available. This is the safest migration mode for active teams.
- `workflow_first`: treats workflow state as the primary operating model. Use this when your board, review flow, and intervention expectations are already aligned with the orchestrator.

Practical guidance:

- choose `legacy` for backward compatibility
- choose `hybrid` for most real migrations
- choose `workflow_first` only when you want the board and workflow metadata to drive operations by default

## Auto-Dispatch

When board automation is enabled, Batty can move work forward without waiting for a human to manually push every assignment. The orchestrator looks for runnable tasks and idle capacity, then uses the existing engineer assignment path so the engineer still receives a normal inbox assignment with worktree and branch context.

At a high level, the path is:

1. identify open work in backlog or todo
1. resolve runnable tasks against workflow state and dependencies
1. pick an engineer with available capacity
1. claim the task / move it to `in-progress`
1. prepare the engineer worktree and branch
1. deliver the assignment and record the result

This is controlled primarily by:

- `board.auto_dispatch`
- `workflow_policy.wip_limit_per_engineer`
- `workflow_policy.capability_overrides`

## Intervention Matrix

The orchestrator's recovery prompts are state-driven, not timer spam. Each intervention family computes a signature from the current board/runtime state, tracks the member's current idle epoch, and suppresses repeat prompts unless either the state meaningfully changes or the cooldown expires.

| Intervention          | Trigger                                                                                 | State / dedupe behavior                                                                                                              | Main config knobs                                                                                                                                       |
| --------------------- | --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Triage backlog        | Manager is idle and has delivered direct-report result packets waiting in inbox         | Signature is based on the delivered backlog set; repeats are suppressed within the same idle epoch until backlog changes             | `automation.triage_interventions`, `automation.intervention_idle_grace_secs`, `automation_sender`                                                       |
| Review backlog        | Review owner is idle while board tasks remain in `review` for them                      | Signature includes review task refs; repeat prompts are blocked unless the review queue changes or cooldown clears                   | `automation.review_interventions`, `automation.intervention_idle_grace_secs`, `workflow_policy.review_timeout_secs`                                     |
| Owned-task recovery   | Member is idle while still owning active board work                                     | Signature follows owned active/review work; same-signature prompts are suppressed and may escalate when the condition persists       | `automation.owned_task_interventions`, `automation.intervention_idle_grace_secs`, `workflow_policy.escalation_threshold_secs`                           |
| Manager dispatch-gap  | Manager is idle while direct reports are idle and runnable work still exists            | Signature includes idle reports plus runnable backlog; repeats wait for a changed dispatch picture or cooldown reset                 | `automation.manager_dispatch_interventions`, `automation.intervention_idle_grace_secs`, `board.auto_dispatch`, `workflow_policy.wip_limit_per_engineer` |
| Architect utilization | Architect is idle while the team is underloaded or blocked on replenishment/lead action | Signature follows the underload snapshot; repeated prompts are deduped per idle epoch                                                | `automation.architect_utilization_interventions`, `automation.intervention_idle_grace_secs`                                                             |
| Board replenishment   | Idle engineers exist but runnable todo work is below threshold                          | Signature tracks idle capacity and runnable queue counts so the architect is only re-prompted when the replenishment picture changes | `board.rotation_threshold`, `automation.architect_utilization_interventions`, `automation.intervention_idle_grace_secs`                                 |

Operationally, all of these share the same guard rails:

- they only fire for members the daemon currently sees as idle
- pending inbox work and higher-priority recovery conditions suppress lower-priority prompts
- `batty pause` disables nudge/standup automation and stops further intervention firing until resumed
- orchestrator actions are recorded to `.batty/orchestrator.log` and the ANSI pane log for operator review

## Triage System

Triage is how Batty prevents upward-report backlog from silently accumulating.

The daemon stores assignment delivery results and tracks delivered direct-report packets in manager inboxes. When a manager is idle, the orchestrator checks whether triage work is waiting and whether the idle period is old enough to justify intervention. If so, it sends a specific prompt telling the manager to process the backlog and summarize upward.

The orchestrator uses idle epochs and intervention signatures so it does not re-fire the same recovery prompt on every loop tick.

Relevant behavior:

- direct-report result packets are tracked as delivered inbox items
- backlog detection is gated on member idleness
- repeated prompts are suppressed unless the backlog signature changes
- interventions can be paused globally with `batty pause`

Main controls:

- `automation.triage_interventions`
- `automation.intervention_idle_grace_secs`
- `automation_sender`

## Review Backlog Interventions

Review backlog recovery is separate from direct-report triage. The orchestrator loads board tasks, finds work sitting in `review`, maps that work to the correct review owner, and prompts an idle manager when review work is waiting.

The intervention message includes useful execution context when available:

- task IDs and titles
- worktree paths
- branch names
- explicit next actions for review or escalation

This is controlled by:

- `automation.review_interventions`
- `workflow_policy.wip_limit_per_reviewer`
- `workflow_policy.review_timeout_secs`

## Owned-Task Interventions

Owned-task recovery handles the case where a member has gone idle even though the board still says they own active work. The daemon treats this as a workflow inconsistency or a blocked lane, then nudges the owner to normalize the board state, unblock the task, or escalate.

Behavior:

- only active ownership states qualify; `done` and archived work are ignored
- prompts are deduped by idle epoch plus the owned-task signature
- if the same stuck condition persists long enough, the daemon escalates instead of silently repeating the same nudge

This is controlled by:

- `automation.owned_task_interventions`
- `automation.intervention_idle_grace_secs`
- `workflow_policy.escalation_threshold_secs`

## Manager Dispatch-Gap Interventions

Dispatch-gap recovery is aimed at leads who have idle capacity below them but have not yet pushed new work into those lanes. The orchestrator inspects direct reports, runnable board work, and current ownership so it can distinguish "nothing to do" from "lead needs to dispatch now."

Behavior:

- only idle managers are considered
- reports with pending inbox work or active board ownership are not treated as free capacity
- the intervention message includes concrete next commands for inspecting todo/in-progress lanes and dispatching specific work

This is controlled by:

- `automation.manager_dispatch_interventions`
- `automation.intervention_idle_grace_secs`
- `board.auto_dispatch`
- `workflow_policy.wip_limit_per_engineer`

## Architect Utilization And Board Replenishment

Architect utilization recovery is the top-level backpressure mechanism. It fires when the team is underloaded and the architect is idle, prompting either dispatch recovery or board replenishment rather than letting the run stall.

Board replenishment is the architect-specific branch of that logic: when idle engineers exist but the runnable todo queue is too small, the orchestrator asks the architect to add or normalize executable tasks.

Behavior:

- utilization and replenishment signatures are deduped so the architect is not re-prompted every loop tick
- replenishment prompts include current board counts and idle-engineer context
- upward reporting guidance is included when the architect has a parent role

This is controlled by:

- `automation.architect_utilization_interventions`
- `automation.intervention_idle_grace_secs`
- `board.rotation_threshold`

## Standup System

Standups are periodic, scoped summaries generated by the daemon. Each recipient sees only their direct reports, not the whole team. Batty enriches standups with board-aware context so the report includes active task IDs, runnable-task pressure, and recent output excerpts rather than only a raw idle/working flag.

Delivery behavior:

- managers and architects receive standups by default
- other roles can opt in with `receives_standup: true`
- a role-level `standup_interval_secs` overrides the global interval
- `standup.interval_secs: 0` disables standups globally
- human users receive standups through their configured channel when applicable

Main controls:

- `standup.interval_secs`
- `standup.output_lines`
- `automation.standups`
- `roles[].receives_standup`
- `roles[].standup_interval_secs`

## Failure Pattern Detection

Batty keeps a rolling in-memory window of recent failure-relevant events. The window is populated from structured daemon events such as:

- `task_escalated`
- events with an `error` field
- event names containing `fail`
- event names containing `conflict`

The current built-in patterns are:

- `RepeatedTestFailure`: repeated error activity for the same role
- `EscalationCluster`: multiple escalations in the recent window
- `MergeConflictRecurrence`: repeated conflict events in the recent window

When the detected frequency crosses the notification threshold, the orchestrator sends recovery notifications to managers. More severe recurrences are also escalated to architects. Each emitted notification is also written to the structured event log as `pattern_detected`.

Controls:

- `automation.failure_pattern_detection`

Operational note:

- the rolling window is currently fixed in code rather than configurable from `team.yaml`

## Retrospective Reports

Retrospectives are generated from `.batty/team_config/events.jsonl`. Batty splits the event stream into runs using `daemon_started` boundaries, analyzes the most recent run, and computes:

- run duration
- task assignment and completion pairs
- retry counts from repeated assignment
- escalations
- routed message count
- average idle percentage from `load_snapshot`

There are two ways to use it:

- manually with `batty retro`
- automatically when every non-archived task on the board is `done`

Generated reports are written to `.batty/retrospectives/<run_end>.md`, and the daemon emits a `retro_generated` event when the automatic path fires.

## Nudge System

Nudges are lightweight recovery prompts driven by sustained idleness. A nudge countdown starts when a member becomes idle, pauses when they resume working, and only fires once per idle period. This prevents repeated nagging while still giving the daemon a way to recover stalled lanes.

Nudges are dependency-aware because the daemon checks board state, inbox state, and higher-priority intervention conditions before deciding a member is ready for idle automation.

Main controls:

- `automation.timeout_nudges`
- `roles[].nudge_interval_secs`
- `automation.intervention_idle_grace_secs`

## Orchestrator Pane And Log

When the orchestrator surface is enabled, layout construction can reserve a dedicated pane for it. The pane position is controlled by:

- `orchestrator_position: bottom`
- `orchestrator_position: left`

In addition to the pane itself, the daemon records high-level orchestrator actions to a text log under `.batty/`. This log is intended as an operator-visible trail of what the control plane actually decided to do, such as review interventions, dispatch recovery prompts, and utilization recovery actions.

The pane/log surface is useful when you want to audit automation decisions instead of guessing from side effects in individual role panes.

## Status Labels

Batty updates tmux pane-border labels continuously so each pane shows the member state and relevant countdowns. This makes the automation state visible without opening logs or scanning events.

Examples of what the border label can reflect:

- idle vs working state
- nudge countdowns
- standup countdowns
- other timer-driven automation state

This is driven by daemon state transitions and refreshed during the polling loop.

## Configuration Reference

The orchestrator-related `team.yaml` keys are spread across root config, board config, workflow policy, automation toggles, and role-level overrides.

### Root Keys

| Key                     | Meaning                                                                        |
| ----------------------- | ------------------------------------------------------------------------------ |
| `workflow_mode`         | Chooses `legacy`, `hybrid`, or `workflow_first`.                               |
| `orchestrator_pane`     | Enables the dedicated orchestrator surface when the workflow mode supports it. |
| `orchestrator_position` | Places the orchestrator pane at `bottom` or `left`.                            |
| `automation_sender`     | Optional visible sender name for daemon-generated intervention messages.       |

### Board Keys

| Key                        | Meaning                                      |
| -------------------------- | -------------------------------------------- |
| `board.rotation_threshold` | Threshold used by board-rotation automation. |
| `board.auto_dispatch`      | Enables automated dispatch of runnable work. |

### Standup Keys

| Key                     | Meaning                                                    |
| ----------------------- | ---------------------------------------------------------- |
| `standup.interval_secs` | Global standup interval in seconds. `0` disables standups. |
| `standup.output_lines`  | Number of recent output lines included per report.         |

### Automation Keys

| Key                                              | Meaning                                                                  |
| ------------------------------------------------ | ------------------------------------------------------------------------ |
| `automation.timeout_nudges`                      | Enables idle nudges.                                                     |
| `automation.standups`                            | Enables periodic standup generation.                                     |
| `automation.failure_pattern_detection`           | Enables rolling failure-pattern detection.                               |
| `automation.triage_interventions`                | Enables direct-report triage recovery prompts.                           |
| `automation.review_interventions`                | Enables review backlog interventions.                                    |
| `automation.owned_task_interventions`            | Enables recovery prompts for idle members holding active work.           |
| `automation.manager_dispatch_interventions`      | Enables prompts when managers are idle while lanes are under-dispatched. |
| `automation.architect_utilization_interventions` | Enables architect utilization recovery prompts.                          |
| `automation.intervention_idle_grace_secs`        | Minimum idle time before state-driven interventions are allowed to fire. |

### Workflow Policy Keys

| Key                                            | Meaning                                                       |
| ---------------------------------------------- | ------------------------------------------------------------- |
| `workflow_policy.wip_limit_per_engineer`       | Optional engineer concurrency guard for dispatch.             |
| `workflow_policy.wip_limit_per_reviewer`       | Optional reviewer concurrency guard.                          |
| `workflow_policy.escalation_threshold_secs`    | Timeout threshold for escalation-sensitive workflow handling. |
| `workflow_policy.review_timeout_secs`          | Timeout threshold for overdue review work.                    |
| `workflow_policy.auto_archive_done_after_secs` | Optional archival timeout for completed work.                 |
| `workflow_policy.capability_overrides`         | Maps work types to roles/capabilities for dispatch decisions. |

### Role-Level Keys

| Key                             | Meaning                                                   |
| ------------------------------- | --------------------------------------------------------- |
| `roles[].receives_standup`      | Opts a role in or out of standups.                        |
| `roles[].standup_interval_secs` | Overrides the global standup cadence for that role.       |
| `roles[].nudge_interval_secs`   | Sets the idle timeout before a nudge is eligible to fire. |
| `roles[].use_worktrees`         | Enables engineer-style worktree handling for that role.   |

## Recommended Starting Point

For most teams, the safest orchestrator rollout looks like this:

```yaml
workflow_mode: hybrid
orchestrator_pane: true
orchestrator_position: bottom

board:
  auto_dispatch: true

standup:
  interval_secs: 300
  output_lines: 30

automation:
  standups: true
  timeout_nudges: true
  triage_interventions: true
  review_interventions: true
  failure_pattern_detection: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 180
```

Start in `hybrid`, watch the orchestrator log, and only move to `workflow_first` once the board, review flow, and intervention prompts match how the team already works.
