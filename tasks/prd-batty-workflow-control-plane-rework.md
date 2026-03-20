# PRD: Batty Workflow Control Plane Rework

## Self-Clarification

1. **Problem/Goal:** Batty currently succeeds at creating and completing work, but too much of the system's coordination truth lives in chat messages rather than structured workflow state. This produces low utilization, high message volume, ambiguous freezes/reviews, and weak recovery when agents go idle or restart. The goal is to rework Batty into a workflow-first control plane where task state, dependencies, ownership, review, merge, and escalation are explicit, while messaging remains a support channel rather than the primary source of orchestration truth.
2. **Core Functionality:** This rework enables three core actions: define and track the full task lifecycle as structured state, dispatch and recover work automatically from that state, and preserve Batty's hierarchical roles so scientist proposes directions, architect manages frontier/utilization, leads manage review/merge/decomposition, and engineers execute bounded tasks.
3. **Scope/Boundaries:** This should not replace tmux, kanban-md, git, or the current role model. It should not become a generic workflow platform unrelated to Batty. It should not remove human override. It should not require a database. It should not fully automate strategic decisions that still belong to architect or human.
4. **Success Criteria:** We can verify success by showing that Batty keeps more members on runnable work, reduces coordination-only churn, recovers deterministically from idle/blocked/review states, and tracks branch/artifact/review/merge lifetime through structured task data. This must be measurable from board files and `events.jsonl`.
5. **Constraints:** Batty must remain terminal-native, file-based, git-friendly, and composable with kanban-md. Existing team topologies and prompt-driven roles must still work. The design should be incrementally adoptable and testable with focused Rust unit tests plus live validation in a project like `~/mafia_solver`.

## Introduction

Batty currently operates as a hierarchical agent team inside tmux with a daemon that routes messages, observes panes, and nudges roles when the run becomes idle or inconsistent. This works, but the current model is still messaging-heavy: many important truths such as "this task is blocked on review," "this engineer produced a mergeable branch," or "this lane is frozen pending a dependency" are inferred from chat and pane state instead of stored as first-class workflow state.

This PRD defines a rework of Batty into a dynamic workflow control plane. The board remains markdown-backed and human-readable, but Batty becomes responsible for managing a structured task lifecycle similar to a lightweight dynamic DAG/workflow engine: task creation, dependency tracking, execution, artifact production, review, merge disposition, escalation, and completion. Roles remain important, but messaging becomes supportive and transactional rather than the main place where work state lives.

The intended operating model is:

- `scientist` proposes new directions, hypotheses, or frontier candidates.
- `architect` turns direction into executable frontier planning, dependency control, and utilization recovery.
- `lead` owns decomposition, review, merge/discard/rework decisions, and direct engineer supervision.
- `engineer` executes bounded work in branches/worktrees and produces structured completion packets.
- messaging is used to notify, request action, or attach evidence, but the control plane remains the task/workflow state.

## Goals

- Increase average team load by ensuring runnable work is surfaced and dispatched earlier.
- Reduce coordination overhead by moving key orchestration truth from chat into structured task metadata and daemon logic.
- Track the full task lifetime: creation, dependency resolution, execution, artifact generation, review, merge, rework, archive, and completion.
- Preserve and strengthen role specialization instead of collapsing all orchestration into a single role.
- Make idle recovery, blocked recovery, review recovery, and merge recovery deterministic and observable.
- Keep the system fully file-based, terminal-native, and compatible with existing Batty + kanban-md workflows.

## Tasks

### T-001: Define the workflow state model
**Description:** Specify the canonical lifecycle states, ownership fields, dependency fields, and evidence fields for Batty-managed tasks.

**Acceptance Criteria:**
- [ ] A written state model exists for task lifecycle states, including `backlog`, `todo`, `in-progress`, `review`, `blocked`, `done`, and `archived`
- [ ] The model distinguishes execution ownership, review ownership, and dependency ownership
- [ ] The model defines required metadata for branch/worktree/artifact/test/review/merge evidence
- [ ] The model defines when a task is considered runnable versus blocked
- [ ] Quality checks pass

### T-002: Extend board metadata for workflow control
**Description:** Add Batty-specific task metadata fields to markdown task files without breaking kanban-md compatibility.

**Acceptance Criteria:**
- [ ] Task files can store workflow metadata such as `depends_on`, `review_owner`, `blocked_on`, `worktree_path`, `branch`, `commit`, `artifacts`, and `next_action`
- [ ] Existing Batty and kanban-md flows continue to parse task files successfully
- [ ] Default behavior for older task files without the new metadata is defined
- [ ] Quality checks pass

### T-003: Introduce a runnable-work resolver
**Description:** Add daemon logic that determines which tasks are executable now, which are waiting on dependencies, and which role should act next.

**Acceptance Criteria:**
- [ ] Batty can compute runnable tasks from board state without relying on pane text
- [ ] Batty can identify blocked tasks and name the exact unmet dependency or review gate
- [ ] Batty can identify review-owned tasks separately from engineer-owned execution tasks
- [ ] Unit tests cover dependency resolution, blocked-state resolution, and runnable-state transitions
- [ ] Quality checks pass

### T-004: Add structured completion packets
**Description:** Standardize engineer completion output so Batty and leads can reason over review/merge readiness without relying on free-form prose alone.

**Acceptance Criteria:**
- [ ] Completion packets include task ID, branch, worktree path, commit SHA, changed paths, tests run, artifacts produced, and claimed outcome
- [ ] Batty can parse and store completion packet fields in workflow metadata
- [ ] Missing or malformed packet fields are surfaced as review blockers
- [ ] Unit tests cover packet parsing and failure cases
- [ ] Quality checks pass

### T-005: Implement review and merge state machine
**Description:** Make review disposition explicit so leads decide whether to merge, request rework, discard, archive, or escalate using structured task transitions.

**Acceptance Criteria:**
- [ ] Review state records review owner, review packet reference, and disposition status
- [ ] Merge-ready, rework-required, discarded, and escalated outcomes are distinguishable in task metadata
- [ ] Batty can move a task from `review` to `done`, `in-progress`, `archived`, or `blocked` based on disposition
- [ ] Unit tests cover review disposition transitions and invalid transitions
- [ ] Quality checks pass

### T-006: Implement dependency-aware nudges and escalations
**Description:** Replace generic idle recovery with state-based interventions that target the correct role based on workflow state.

**Acceptance Criteria:**
- [ ] Engineers are nudged only for runnable work they still own
- [ ] Leads are nudged for review backlog, direct-report completion backlog, or unresolved dispatch gaps
- [ ] Architect is nudged for utilization recovery, blocked frontier decisions, or missing runnable work creation
- [ ] Nudges do not fire while pending inbox messages exist and respect configurable idle grace
- [ ] Unit tests cover state-targeted interventions and suppression conditions
- [ ] Quality checks pass

### T-007: Add merge/artifact lifecycle tracking
**Description:** Track the branch and artifact lifetime from engineer execution through review and merge so Batty knows what evidence exists and what is still missing.

**Acceptance Criteria:**
- [ ] Task metadata stores branch/worktree/commit/artifact references when available
- [ ] Batty can display branch/artifact status in `status` or other inspection commands
- [ ] Merge actions update workflow state and clear or preserve relevant metadata intentionally
- [ ] Unit tests cover branch/artifact metadata capture and state cleanup
- [ ] Quality checks pass

### T-008: Introduce workflow observability metrics
**Description:** Add daemon-derived metrics that reflect actual workflow health rather than only pane activity.

**Acceptance Criteria:**
- [ ] Batty records runnable task count, blocked task count, review age, assignment age, and idle-with-runnable-work signals
- [ ] Metrics are derivable from task and event state without external infrastructure
- [ ] `batty status` or a new inspection command exposes high-signal workflow metrics
- [ ] Unit tests cover metric computation
- [ ] Quality checks pass

### T-009: Add config-driven workflow policies
**Description:** Make workflow behaviors configurable so teams can enable or tune dependency recovery, review routing, WIP rules, and escalation policies.

**Acceptance Criteria:**
- [ ] Team config can enable or disable workflow interventions independently
- [ ] Team config can define WIP limits or similar concurrency guardrails per role
- [ ] Team config can control grace intervals and escalation thresholds
- [ ] Config validation rejects inconsistent workflow policy settings
- [ ] Unit tests cover default and explicit config parsing
- [ ] Quality checks pass

### T-010: Preserve role specialization with explicit contracts
**Description:** Rewrite role prompts and role-specific workflow responsibilities so scientist, architect, lead, and engineer operate against the new control plane consistently.

**Acceptance Criteria:**
- [ ] Scientist prompt describes frontier proposal and evidence generation responsibilities without tactical dispatch ownership
- [ ] Architect prompt describes dependency control, frontier planning, and utilization recovery duties
- [ ] Lead prompts describe decomposition, review, merge, discard, and rework duties
- [ ] Engineer prompts describe bounded execution and structured completion packet duties
- [ ] Prompt/templates remain compatible with current Batty runtime
- [ ] Quality checks pass

### T-011: Add migration and backward compatibility behavior
**Description:** Ensure existing Batty projects can adopt the new workflow model without breaking active runs or older boards.

**Acceptance Criteria:**
- [ ] Batty can read older task files that do not contain new workflow metadata
- [ ] A documented migration path exists for upgrading existing projects
- [ ] Batty defaults undefined fields safely and predictably
- [ ] Validation surfaces partial migration states clearly
- [ ] Quality checks pass

### T-012: Validate the new model on a live hierarchical run
**Description:** Use a real project run such as `~/mafia_solver` to verify that the new control plane increases throughput and reduces control chatter.

**Acceptance Criteria:**
- [ ] A live run is executed with the workflow-first control plane enabled
- [ ] Load, assignment, completion, and review metrics are captured before and after
- [ ] At least one substantive branch/review/merge path is completed through the new lifecycle
- [ ] A short postmortem documents what improved and what still failed
- [ ] Quality checks pass

## Functional Requirements

1. **FR-1:** The system must treat board/task metadata as the primary source of orchestration truth for task state, dependency state, review state, and merge state.
2. **FR-2:** The system must preserve markdown task files and remain compatible with kanban-md as the base task management layer.
3. **FR-3:** The system must model task lifecycle explicitly, including runnable, blocked, review-held, merge-ready, rework-required, and completed states.
4. **FR-4:** The system must distinguish execution ownership from review ownership so engineer and lead responsibilities do not collapse into one overloaded `claimed_by` field.
5. **FR-5:** The system must support explicit task dependencies and determine whether a task is runnable from those dependencies.
6. **FR-6:** The system must support structured completion packets for engineer-produced work, including branch/worktree/commit/test/artifact evidence where available.
7. **FR-7:** The system must allow leads to disposition reviewed work as merge, rework, archive/discard, or escalate, with those decisions reflected in task state.
8. **FR-8:** The system must allow architect to manage blocked frontier work by creating new tasks, assigning dependencies, or escalating policy questions.
9. **FR-9:** The system must keep scientist as a role that can generate new direction or hypothesis tasks without owning day-to-day dispatch.
10. **FR-10:** The daemon must compute which role is responsible for the next action on a task based on current workflow state.
11. **FR-11:** The daemon must only send nudges when the task/workflow state indicates a real intervention target and when inbox/preconditions permit.
12. **FR-12:** The system must expose workflow observability signals that reflect runnable work, blocked work, review backlog, and idle-with-runnable-work situations.
13. **FR-13:** The system must support configuration of workflow policies such as idle grace, escalation thresholds, intervention toggles, and concurrency/WIP rules.
14. **FR-14:** The system must preserve human override through tmux, CLI commands, and direct message/send operations.
15. **FR-15:** The system must remain file-based and git-auditable, with no required external database or service.

## Non-Goals

- Batty will not become a general-purpose distributed workflow engine for arbitrary enterprise jobs.
- This rework will not replace tmux, git, kanban-md, or the current CLI-first operating model.
- This rework will not remove natural-language messaging entirely; it will reduce its orchestration burden.
- This rework will not fully automate architect or human policy decisions where genuine ambiguity or high-risk judgment remains.
- This rework will not require a web dashboard or cloud control plane.
- This rework will not attempt to solve all prompt-quality issues through orchestration alone.

## Technical Considerations

- The preferred model is dynamic-workflow-like rather than a static DAG. New tasks may be created during execution, but each task must still have explicit dependencies, owners, and next-action semantics.
- Batty should continue to compose with existing tools rather than replacing them. kanban-md owns task file conventions; Batty owns orchestration semantics layered on top.
- New task metadata must remain readable and diff-friendly in markdown frontmatter.
- The daemon should derive as much as possible from board state plus event log state rather than parsing pane text for control truth.
- `events.jsonl` should remain the audit trail for state transitions, interventions, assignments, review dispositions, and merge decisions.
- Existing commands such as `batty status`, `batty inbox`, `batty merge`, and `batty assign` should evolve rather than be replaced outright.
- The design should tolerate restarts: a restarted daemon must recover workflow state from files and resume deterministic supervision.

## Success Metrics

- Increase average active load on real project runs from current ~20-30% toward a materially higher steady state.
- Reduce message-to-completion ratio on comparable runs.
- Reduce cases where a task remains `in-progress` while the true next action belongs to review or dependency resolution.
- Reduce time from engineer completion packet to lead review disposition.
- Reduce time a member remains idle while runnable work exists for that role or lane.
- Increase the percentage of interventions that produce a state transition rather than another explanatory message.
- Preserve or improve total completed-task throughput while lowering coordination overhead.

## Open Questions

- Should task dependencies be stored as explicit IDs only, or also grouped into named gate concepts such as "blind-source gate"?
- Should Batty introduce a first-class "merge queue" concept, or is review disposition plus branch metadata sufficient?
- How strict should WIP limits be by default for leads and engineers?
- Should review and merge evidence be stored only in task metadata, or also as structured packet files in a separate directory?
- Should scientist-generated directions become a special task class, or can they use the same task model with different ownership?
- How much automatic state normalization should Batty perform versus only suggesting the transition to architect or lead?
