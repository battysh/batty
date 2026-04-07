# Batty <-> OpenClaw Contract

## Goal

Define a stable, versioned contract between Batty and OpenClaw for supervision.

Batty stays authoritative for:

- team topology
- board state
- messaging delivery
- policy enforcement
- operator-visible approval rules

OpenClaw only sees normalized DTOs and emits explicit commands. It does not
depend on Batty's prompt wording, free-form summaries, or internal event names.

## Anti-corruption boundary

The contract boundary sits between:

- Batty internals: `status::TeamStatusJsonReport`, `events::TeamEvent`, board state,
  workflow metrics, and operator policy
- OpenClaw-facing DTOs: `TeamStatus`, `TeamEvent`, `TeamCommand`,
  `EscalationSurface`, `ApprovalSurface`, and capability negotiation

Rules:

1. Batty prompt or message wording does not cross the boundary.
2. Internal event names are mapped to explicit enum variants.
3. Batty remains the system of record for lifecycle and merge/review authority.
4. OpenClaw command intents are explicit and typed, never inferred from prose.

## DTOs

### `TeamStatus`

Represents the current supervision-safe view of a team.

Fields:

- `teamName`, `sessionName`
- `lifecycle`: `running | stopped | degraded | recovering`
- `running`, `paused`
- `members[]`: normalized `MemberStatus`
- `pipeline`: normalized `PipelineMetrics`
- `escalationSurface`
- `approvalSurface`
- `capabilities[]`

### `MemberStatus`

Represents one Batty member in a stable form.

Fields:

- `name`, `role`, `roleType`
- `state`: `starting | idle | working | done | crashed | unknown`
- `health`: `healthy | warning | unhealthy`
- `activeTaskIds[]`, `reviewTaskIds[]`
- `pendingInboxCount`, `triageBacklogCount`
- `signal`
- `restartCount`, `contextExhaustionCount`, `deliveryFailureCount`
- `taskElapsedSecs`
- `backendHealth`

### `PipelineMetrics`

Represents workflow-safe queue and throughput counters.

Fields:

- `activeTaskCount`, `reviewQueueCount`
- `runnableCount`, `blockedCount`
- `inReviewCount`, `inProgressCount`
- `staleInProgressCount`, `staleReviewCount`
- `triageBacklogCount`, `unhealthyMemberCount`
- `autoMergeRate`, `reworkRate`, `avgReviewLatencySecs`

### `TeamEvent`

Represents a single explicit event in the OpenClaw stream.

Topics:

- `completion`
- `review`
- `stall`
- `merge`
- `escalation`
- `delivery_failure`
- `lifecycle`

Explicit event kinds include:

- `task_completed`
- `task_escalated`
- `task_merged_automatic`
- `task_merged_manual`
- `review_nudged`
- `review_escalated`
- `review_stalled`
- `agent_stalled`
- `delivery_failed`
- `session_started`
- `session_reloading`
- `session_reloaded`
- `session_stopped`
- `agent_started`
- `agent_restarted`
- `agent_crashed`
- `agent_stopped`
- `agent_respawned`
- `agent_context_exhausted`
- `agent_health_changed`

### `TeamCommand`

Represents a typed command Batty can validate before acting on it.

Supported actions:

- `start`
- `stop`
- `restart`
- `send`
- `nudge`
- `review`
- `merge`

Each command carries:

- `projectId`
- `actor`
- `approval`
- an explicit typed action payload

## Escalation and approval surfaces

`EscalationSurface` states what OpenClaw may surface to humans or Batty:

- `session_unavailable`
- `member_unhealthy`
- `review_queue_blocked`
- `task_blocked`
- `human_approval_required`

Authority is `recommend_only` in this phase. OpenClaw can recommend or request
approval, but Batty still enforces policy.

`ApprovalSurface` defines which commands are human-only or human-approved:

- `stop`: required approval, human decision `stop_session`
- `restart`: suggested approval, human decision `restart_session`
- `review`: required approval, human decision `review_disposition`
- `merge`: required approval, human decision `merge_disposition`

`start`, `send`, and `nudge` remain machine-readable and low-authority.

## Versioning plan

Schema rules:

1. Current schema version is `1`.
2. Minimum supported schema version is `1`.
3. Additive fields are allowed within a schema version.
4. Breaking field or meaning changes require a new schema version.
5. New enum variants require capability review before rollout.

Capability negotiation:

- OpenClaw sends `requestedSchemaVersion`
- OpenClaw sends `minCompatibleSchemaVersion`
- OpenClaw optionally requests a subset of capabilities
- Batty returns `compatible`, `negotiatedSchemaVersion`, and granted capabilities

This lets Batty ship the contract incrementally while remaining explicit about
which DTOs and authority surfaces are active.

## Compatibility guidance

- Batty may continue emitting the legacy `projectEvent` envelope for existing
  consumers during migration.
- New consumers should prefer the explicit `TeamEvent` contract with enum-based
  `eventKind`.
- Approval policies are part of the contract surface and must be updated with
  the same discipline as commands or event kinds.
