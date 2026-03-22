# Intervention System

The intervention system is an automated recovery mechanism in the Batty daemon that detects idle or stalled agents and nudges them back into productive work. It runs inside the daemon poll loop and covers six intervention types plus a baseline idle nudge.

All interventions share common behavior:

- **Pause marker** — interventions are suppressed when `.batty/pause` exists
- **Idle grace** — no intervention fires until the member has been idle for at least `intervention_idle_grace_secs` (default: 60s)
- **Pending inbox check** — interventions wait until the member's inbox has no unread messages (the agent may already be about to resume)
- **Cooldown** — after firing, the same intervention key is suppressed for `intervention_cooldown_secs` (default: 120s)
- **Signature dedup** — interventions compute a signature from the current state (task IDs, statuses, etc.) and suppress repeat fires while the signature is unchanged
- **Live delivery** — if the message is injected into a live pane, the member is marked working immediately

## 1. Idle Timeout Nudge

The simplest intervention: if a member has been idle past their configured timeout, remind them to resume work.

### Trigger Conditions

- `automation.timeout_nudges` is `true` (default: `true`)
- Member has been idle for at least `nudge_interval_secs` (per-role, default: 1800s / 30min) or `intervention_idle_grace_secs`, whichever is larger
- Member has no pending inbox messages
- Nudge has not already fired during this idle period

### Actions

- Delivers the member's configured nudge text plus a standard suffix: *"Idle nudge: you have been idle past your configured timeout. Move the current lane forward now or report the exact blocker."*
- Marks `fired_this_idle = true` to prevent repeat firing within the same idle period

### State Machine

```
Working → Idle (start timer, reset fired_this_idle)
Idle + timer elapsed + inbox empty → fire nudge → mark fired_this_idle
Idle → Working (clear timer, reset fired_this_idle, pause schedule)
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `timeout_nudges` | `automation` | `true` | Enable/disable idle nudges |
| `nudge_interval_secs` | per-role `roles[].nudge_interval_secs` | `1800` | Seconds before nudge fires |
| `intervention_idle_grace_secs` | `automation` | `60` | Minimum idle duration before any intervention |

### Cooldown / Dedup

- Fires once per idle period (`fired_this_idle` flag)
- Resets when the member transitions back to Working

## 2. Triage Intervention

Detects managers or architects who have unprocessed direct-report result packets in their inbox and nudges them to review.

### Trigger Conditions

- `automation.triage_interventions` is `true` (default: `true`)
- Member is idle and past the idle grace period
- Member has direct reports (is a manager or architect with reports)
- There are delivered but unacknowledged result packets from direct reports
- The member's idle epoch has advanced (they transitioned Working → Idle at least once)
- The triage intervention has not already fired for this idle epoch
- The `triage::<member_name>` cooldown key is not active

### Actions

- Sends a structured message listing the triage backlog count, scope of reports, and step-by-step resolution commands (`batty inbox`, `batty read`, `batty send`, `batty assign`)
- Records an orchestrator action: `recovery: triage intervention for <member> with N pending direct-report result(s)`
- Emits `TeamEvent::triage_intervention` (via orchestrator log)

### State Machine

```
Member idle + direct-report results pending + new idle epoch
  → fire triage intervention
  → record idle epoch as handled
  → start cooldown timer
Member resumes working → idle epoch increments on next idle transition
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `triage_interventions` | `automation` | `true` | Enable/disable triage interventions |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by `triage::<member_name>`
- Tracks idle epoch to avoid re-firing within the same idle period
- Cooldown timer prevents rapid re-firing across epoch transitions

## 3. Owned-Task Intervention

Detects members who are idle but still own active board tasks (in-progress, todo, or backlog — excluding review, done, and archived).

### Trigger Conditions

- `automation.owned_task_interventions` is `true` (default: `true`)
- Member is idle and past the idle grace period
- Member has no pending inbox messages
- Member owns tasks with status not in {`review`, `done`, `archived`}
- The intervention has not already fired for this exact set of tasks (signature check)
- The member's cooldown key is not active

### Actions

**Initial nudge (to the idle member):**
- Lists all owned active tasks with IDs, statuses, and titles
- Provides commands to retrieve task context (`kanban-md show`, `sed`)
- Suggests next actions: assign subtask, delegate, escalate blocker, or move to review/done
- Records orchestrator action: `recovery: owned-task intervention for <member> covering N active task(s)`

**Escalation (to the member's manager):**
- If the member remains stuck on the same tasks for `escalation_threshold_secs` (default: 3600s / 1 hour), escalates to their `reports_to` parent
- Sends a structured escalation message to the parent with task details and stuck duration
- Emits `TeamEvent::task_escalated` for each escalated task
- Marks `escalation_sent = true` to prevent repeat escalation

### State Machine

```
Member idle + owns active tasks + new signature
  → fire owned-task nudge to member
  → record signature + detection timestamp
  → start cooldown timer

Same signature persists for escalation_threshold_secs
  → escalate to reports_to parent
  → mark escalation_sent

Task set changes (new signature)
  → reset and fire new nudge
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `owned_task_interventions` | `automation` | `true` | Enable/disable owned-task interventions |
| `escalation_threshold_secs` | `workflow_policy` | `3600` | Seconds before escalating to parent |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by member name
- Signature = sorted `task_id:status` pairs — changes when tasks are added, removed, or change status
- Escalation is a one-shot per signature (resets when signature changes)

## 4. Review Intervention

Detects members who have tasks waiting in the review queue and nudges them to process reviews.

### Trigger Conditions

- `automation.review_interventions` is `true` (default: `true`)
- Member is idle and past the idle grace period
- Member has no pending inbox messages
- Member owns review tasks (determined by `review_backlog_owner_for_task` — prefers the `reports_to` manager of the task's claimed engineer; falls back to `claimed_by` if engineer not found in members)
- The idle epoch has advanced at least once
- The intervention has not already fired for this exact review task set (signature check)
- The `review::<member_name>` cooldown key is not active

### Actions

- Lists review tasks with IDs, claimed engineers, branch names, and worktree paths
- Provides commands to inspect each task (`kanban-md show`, `sed`, worktree path, `git diff`)
- Suggests review actions: approve and move to done, request changes, or merge
- Records orchestrator action: `recovery: review intervention for <member> covering N queued review task(s)`

### State Machine

```
Member idle + review tasks pending + new signature
  → fire review intervention
  → record signature
  → start cooldown timer

Signature changes (tasks added/removed/status changed)
  → reset and fire new intervention
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `review_interventions` | `automation` | `true` | Enable/disable review interventions |
| `review_nudge_threshold_secs` | `workflow_policy` | `1800` | Time before review nudge (used by daemon review loop) |
| `review_timeout_secs` | `workflow_policy` | `7200` | Time before review escalation (used by daemon review loop) |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by `review::<member_name>`
- Signature = sorted `task_id:status:claimed_by` triples
- Suppressed while signature unchanged

## 5. Manager Dispatch-Gap Intervention

Detects managers whose direct-report engineers are all idle, with either active unworked tasks or unassigned open tasks available.

### Trigger Conditions

- `automation.manager_dispatch_interventions` is `true` (default: `true`)
- Member is a Manager role
- Member is idle and past the idle grace period
- Member has no pending inbox messages
- Member has no pending triage backlog (triage takes priority)
- Member has no pending review tasks (reviews take priority)
- All direct-report engineers are idle
- At least one of:
  - Idle engineers with active tasks (claimed but not worked)
  - Unassigned open tasks (backlog/todo with no `claimed_by`)
- The intervention has not already fired for this exact dispatch state (signature check)
- The `dispatch::<member_name>` cooldown key is not active

### Actions

- Lists idle engineers with their active task IDs
- Lists idle engineers with no assignments
- Lists available unassigned open tasks
- Suggests dispatch actions: assign tasks, reassign, or replenish the board
- Records orchestrator action with counts of idle active/unassigned reports and open tasks

### State Machine

```
Manager idle + all engineers idle + dispatch gap detected + triage/review clear
  → fire dispatch-gap intervention
  → record signature
  → start cooldown timer

Dispatch state changes (engineer starts working, task assigned)
  → reset and fire new intervention
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `manager_dispatch_interventions` | `automation` | `true` | Enable/disable dispatch-gap interventions |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by `dispatch::<member_name>`
- Signature = sorted combination of idle-active report names + task IDs, idle-unassigned report names, and unassigned task IDs
- Suppressed while dispatch state unchanged

## 6. Architect Utilization Intervention

Detects pipeline-wide starvation where less than half of engineers are working, and nudges the architect to intervene.

### Trigger Conditions

- `automation.architect_utilization_interventions` is `true` (default: `true`)
- At least one engineer exists in the team
- A utilization gap exists: idle engineers with active tasks, OR idle unassigned engineers with open tasks available
- Fewer than half of all engineers are currently working (`working < ceil(total / 2)`)
- Architect is idle and past the idle grace period
- Architect has no pending inbox messages
- The intervention has not already fired for this exact utilization state (signature check)
- The `utilization::<architect_name>` cooldown key is not active

### Actions

- Reports team-wide utilization: working engineers, idle engineers with active tasks (listing task IDs), idle engineers without assignments
- Lists available unassigned open tasks
- Suggests actions: dispatch work, unblock stalled engineers, or replenish the board
- Records orchestrator action with full utilization breakdown

### State Machine

```
Less than half engineers working + utilization gap + architect idle
  → fire utilization intervention to architect
  → record signature
  → start cooldown timer

Utilization state changes (engineer starts working, tasks assigned)
  → reset and fire new intervention
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `architect_utilization_interventions` | `automation` | `true` | Enable/disable utilization interventions |
| `pipeline_starvation_threshold` | `workflow_policy` | `1` | Minimum pipeline depth (used elsewhere for starvation detection) |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by `utilization::<architect_name>`
- Signature = sorted combination of working engineers, idle-active engineers + task IDs, idle-unassigned engineers, and unassigned task IDs
- Suppressed while utilization state unchanged

## 7. Board Replenishment Intervention

Detects when the board is running low on actionable work and nudges the architect to create more tasks.

### Trigger Conditions

- Pause marker not present
- At least one architect and one engineer exist
- Idle unassigned engineers exist (engineers who are idle and have no active claimed tasks)
- The number of unblocked, unclaimed todo tasks is below `replenishment_threshold` (defaults to the total number of engineers)
- Unblocked = no `blocked`, no `blocked_on`, and all `depends_on` tasks are done or missing
- Architect is idle and past the idle grace period
- Architect has no pending inbox messages
- The intervention has not already fired for this exact board state (signature check)
- The `replenishment::<architect_name>` cooldown key is not active

### Actions

- Reports board health: todo/in-progress/done counts, threshold, idle engineers, and available unblocked tasks
- If `.batty/team_config/replenishment_context.md` exists, includes its content as planning context
- Suggests: create new tasks, unblock existing tasks, or break down large tasks
- Records orchestrator action with full board breakdown

### State Machine

```
Unblocked todo tasks < threshold + idle unassigned engineers + architect idle
  → fire replenishment intervention to architect
  → record signature
  → start cooldown timer

Board state changes (tasks added, completed, unblocked)
  → reset and fire new intervention
```

### Configuration

| Field | Location | Default | Description |
|-------|----------|---------|-------------|
| `replenishment_threshold` | `automation` | number of engineers | Minimum unblocked todo tasks before intervention fires |
| `intervention_idle_grace_secs` | `automation` | `60` | Grace period before firing |
| `intervention_cooldown_secs` | `automation` | `120` | Cooldown between fires |

### Cooldown / Dedup

- Keyed by `replenishment::<architect_name>`
- Signature = sorted idle engineers + unblocked task IDs + todo/in-progress/done counts
- Suppressed while board state unchanged

## Common Configuration Reference

All intervention settings live in `team.yaml` under the `automation` and `workflow_policy` sections.

### `automation` section

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timeout_nudges` | bool | `true` | Enable idle timeout nudges |
| `triage_interventions` | bool | `true` | Enable triage backlog interventions |
| `review_interventions` | bool | `true` | Enable review queue interventions |
| `owned_task_interventions` | bool | `true` | Enable owned-task interventions |
| `manager_dispatch_interventions` | bool | `true` | Enable dispatch-gap interventions |
| `architect_utilization_interventions` | bool | `true` | Enable utilization interventions |
| `replenishment_threshold` | int or null | number of engineers | Unblocked todo threshold for replenishment |
| `intervention_idle_grace_secs` | int | `60` | Seconds a member must be idle before any intervention fires |
| `intervention_cooldown_secs` | int | `120` | Seconds between repeated fires of the same intervention key |

### `workflow_policy` section

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `escalation_threshold_secs` | int | `3600` | Seconds before owned-task escalation to parent |
| `review_nudge_threshold_secs` | int | `1800` | Seconds before review nudge |
| `review_timeout_secs` | int | `7200` | Seconds before review escalation |
| `pipeline_starvation_threshold` | int or null | `1` | Minimum pipeline depth for starvation detection |

### Per-role settings

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `nudge_interval_secs` | int | `1800` | Seconds before idle nudge fires for this role |

## Example Scenarios

### Scenario 1: Engineer finishes work but doesn't move the task

1. Engineer completes coding and goes idle
2. After 60s idle grace, daemon detects engineer is idle with an in-progress task
3. Owned-task intervention fires: "You are idle but still own task #42 (in-progress). Move it forward or report the blocker."
4. If engineer remains stuck for 3600s on the same task, daemon escalates to their manager

### Scenario 2: Manager ignores review queue

1. Engineer moves task #15 to review status
2. Manager is idle with no other work
3. After 60s grace, review intervention fires: "You have 1 queued review task: #15 by eng-1"
4. Provides commands to inspect the worktree and review the changes

### Scenario 3: Pipeline starvation

1. Team has 4 engineers, only 1 is working
2. 2 idle engineers have active tasks they're not progressing
3. 1 idle engineer has no assignments but open tasks exist on the board
4. Daemon detects < 50% utilization and fires architect utilization intervention
5. Architect receives a breakdown of who is idle, what tasks are stuck, and what's available

### Scenario 4: Board running dry

1. Only 1 unblocked todo task remains, but 3 engineers are idle and unassigned
2. Replenishment threshold (defaults to 3, the number of engineers) is not met
3. Daemon fires board replenishment intervention to the architect
4. If `replenishment_context.md` exists, its content is included as planning guidance

## Source Code

The intervention system is implemented in `src/team/daemon/interventions.rs`. Supporting types:

- `NudgeSchedule` — tracks per-member idle nudge state
- `OwnedTaskInterventionState` — tracks signature, detection time, and escalation status
- `TeamEvent` variants — `task_escalated`, `review_nudge_sent`, `review_escalated` (in `src/team/events.rs`)
- `AutomationConfig` — all toggle and timing fields (in `src/team/config.rs`)
- `WorkflowPolicy` — escalation and review thresholds (in `src/team/config.rs`)
