# PRD: Batty Workflow Control Plane Rework

## Self-Clarification

1. **Problem/Goal:** Batty currently succeeds at creating and completing work, but too much of the system's coordination truth lives in chat messages rather than structured workflow state. This produces low utilization, high message volume, ambiguous freezes/reviews, and weak recovery when agents go idle or restart. The goal is to add a workflow-control feature layer where task state, dependencies, ownership, review, merge, and escalation are explicit, while messaging remains a support channel rather than the primary source of orchestration truth. This must be additive rather than a breaking replacement.
2. **Core Functionality:** This rework enables six core actions: define and track the full task lifecycle as structured state, dispatch and recover work automatically from that state, preserve Batty's role hierarchy in a topology-agnostic way so planning, dispatch, execution, review, and escalation responsibilities can be mapped onto any valid Batty team composition, expose the workflow engine itself as a visible orchestrator role in tmux rather than an invisible background-only process, require orchestrator actions to flow through explicit Batty CLI/API operations so they are reproducible and inspectable, and allow the same workflow model to operate with or without the built-in orchestrator enabled.
3. **Scope/Boundaries:** This should not replace tmux, kanban-md, git, or the current role model. It should not become a generic workflow platform unrelated to Batty. It should not remove human override. It should not require a database. It should not fully automate strategic decisions that still belong to architect or human. It should not force existing teams to adopt workflow-first orchestration immediately.
4. **Success Criteria:** We can verify success by showing that Batty keeps more members on runnable work, reduces coordination-only churn, recovers deterministically from idle/blocked/review states, and tracks branch/artifact/review/merge lifetime through structured task data. This must be measurable from board files and `events.jsonl`.
5. **Constraints:** Batty must remain terminal-native, file-based, git-friendly, and composable with kanban-md. Existing team topologies and prompt-driven roles must still work, including solo, pair, manager-led, multi-manager, and renamed-role templates. The design should be incrementally adoptable and testable with focused Rust unit tests plus live validation in real projects.

## Introduction

Batty currently operates as a hierarchical agent team inside tmux with a daemon that routes messages, observes panes, and nudges roles when the run becomes idle or inconsistent. This works, but the current model is still messaging-heavy: many important truths such as "this task is blocked on review," "this engineer produced a mergeable branch," or "this lane is frozen pending a dependency" are inferred from chat and pane state instead of stored as first-class workflow state.

This PRD defines an additive rework of Batty into a dynamic workflow control plane. The board remains markdown-backed and human-readable, but Batty gains the ability to manage a structured task lifecycle similar to a lightweight dynamic DAG/workflow engine: task creation, dependency tracking, execution, artifact production, review, merge disposition, escalation, and completion. Roles remain important, but messaging becomes supportive and transactional rather than the main place where work state lives.

The intended operating model is capability-based rather than role-name-based. Batty should resolve these workflow capabilities from team config, role type, and hierarchy:

- `planner`: defines or prioritizes frontier work.
- `dispatcher`: decomposes runnable work and routes it to executors.
- `executor`: performs bounded implementation, investigation, or validation work.
- `reviewer`: accepts, rejects, merges, archives, or escalates completed work.
- `orchestrator`: monitors workflow state, computes next actions, records decisions, and drives automatic interventions.
- `operator`: represents an external human endpoint when one exists.

In some topologies, one role may hold multiple capabilities. For example:

- in a solo topology, one role may plan, execute, and review;
- in a pair topology, the architect may plan/review while the engineer executes;
- in a manager topology, architect may plan, manager may dispatch/review, and engineers may execute;
- in a research topology, renamed roles such as `principal`, `sub-lead`, and `researcher` should still map cleanly onto the same workflow capabilities.

Messaging is used to notify, request action, or attach evidence, but the control plane remains the task/workflow state.

The orchestrator must not rely on hidden internal-only mutations for normal workflow actions. When it creates tasks, changes task state, dispatches work, records review dispositions, or triggers workflow transitions, it should do so through explicit Batty CLI/API operations that could also be invoked by another orchestrator process, a human operator, or an agent acting through the same command surface. Teams must also be able to disable the built-in orchestrator and still use the same workflow model manually or through agent-driven operation.

The rollout model is explicitly additive:

- `legacy mode`: current Batty behavior continues to work with message-driven coordination and optional nudges.
- `hybrid mode`: workflow-state features and orchestrator assistance are enabled selectively while current runtime behaviors remain available.
- `workflow-first mode`: workflow state becomes the primary source of orchestration truth and messaging is mostly assistive.

## Goals

- Increase average team load by ensuring runnable work is surfaced and dispatched earlier.
- Reduce coordination overhead by moving key orchestration truth from chat into structured task metadata and daemon logic.
- Track the full task lifetime: creation, dependency resolution, execution, artifact generation, review, merge, rework, archive, and completion.
- Preserve and strengthen role specialization instead of collapsing all orchestration into a single role.
- Support all shipped Batty team topologies without hardcoding one specific org shape or role naming scheme.
- Make the workflow engine visible and inspectable as its own orchestrator runtime surface, with tmux visibility and persistent logs.
- Expose orchestrator actions through stable CLI/API operations so workflow control is reproducible, scriptable, and replaceable.
- Allow the workflow control model to function both with the built-in orchestrator enabled and with orchestrator behavior driven externally through CLI/API commands.
- Preserve current Batty behavior as a supported legacy mode while allowing gradual adoption of the new workflow layer.
- Make idle recovery, blocked recovery, review recovery, and merge recovery deterministic and observable.
- Keep the system fully file-based, terminal-native, and compatible with existing Batty + kanban-md workflows.

## Tasks

### T-001: Define topology-independent workflow responsibilities
**Description:** Specify the universal workflow capabilities and how Batty resolves them from role type, hierarchy, and optional config overrides.

**Acceptance Criteria:**
- [ ] A written capability model exists for planner, dispatcher, executor, reviewer, orchestrator, and operator responsibilities
- [ ] The model defines how responsibilities resolve in solo, pair, manager-led, multi-manager, and renamed-role topologies
- [ ] The model defines fallback behavior when a topology lacks a dedicated reviewer or dispatcher layer
- [ ] The model defines which responsibilities belong to the orchestrator and which belong to agent roles
- [ ] The model does not rely on hardcoded role names such as `scientist`, `architect`, `lead`, or `engineer`
- [ ] Quality checks pass

### T-002: Define the workflow state model
**Description:** Specify the canonical lifecycle states, ownership fields, dependency fields, and evidence fields for Batty-managed tasks.

**Acceptance Criteria:**
- [ ] A written state model exists for task lifecycle states, including `backlog`, `todo`, `in-progress`, `review`, `blocked`, `done`, and `archived`
- [ ] The model distinguishes execution ownership, review ownership, and dependency ownership
- [ ] The model defines required metadata for branch/worktree/artifact/test/review/merge evidence
- [ ] The model defines when a task is considered runnable versus blocked
- [ ] Quality checks pass

### T-003: Extend board metadata for workflow control
**Description:** Add Batty-specific task metadata fields to markdown task files without breaking kanban-md compatibility.

**Acceptance Criteria:**
- [ ] Task files can store workflow metadata such as `depends_on`, `review_owner`, `blocked_on`, `worktree_path`, `branch`, `commit`, `artifacts`, and `next_action`
- [ ] Existing Batty and kanban-md flows continue to parse task files successfully
- [ ] Default behavior for older task files without the new metadata is defined
- [ ] Quality checks pass

### T-004: Introduce an orchestrator runtime surface
**Description:** Make the workflow engine a first-class Batty runtime role with its own tmux pane, visible activity stream, and persistent logs.

**Acceptance Criteria:**
- [ ] Batty launches an orchestrator pane in tmux for workflow activity and decisions
- [ ] The orchestrator pane shows high-signal workflow actions such as dependency resolution, interventions, dispatch decisions, and recovery decisions
- [ ] Orchestrator activity is persisted to a dedicated log in addition to `events.jsonl`
- [ ] The orchestrator pane is inspectable without taking over other agent panes
- [ ] Quality checks pass

### T-005: Expose orchestrator actions through Batty CLI/API
**Description:** Define and implement the CLI/API surface that the orchestrator uses for workflow mutations so control-plane actions are not hidden internal side effects.

**Acceptance Criteria:**
- [ ] Batty provides explicit commands or internal API equivalents for workflow actions such as create task, update task state, assign work, record dependency state, record review disposition, and trigger merge/archive/rework transitions
- [ ] The orchestrator uses the same command/API surface for normal workflow mutations instead of bespoke hidden mutation paths
- [ ] The command/API surface is documented well enough for other orchestrators, agents, or humans to reproduce orchestrator actions
- [ ] The command/API surface remains usable when the built-in orchestrator is disabled
- [ ] Unit tests cover at least one end-to-end workflow mutation path through the public command/API surface
- [ ] Quality checks pass

### T-006: Support orchestrated and non-orchestrated runtime modes
**Description:** Allow teams to run the same workflow model with the built-in orchestrator enabled or disabled, without losing workflow correctness.

**Acceptance Criteria:**
- [ ] Team config can enable or disable the built-in orchestrator role intentionally
- [ ] When orchestrator is disabled, workflow state can still be inspected and mutated through Batty CLI/API commands
- [ ] Workflow correctness does not depend on the orchestrator pane being present
- [ ] Documentation explains when to use orchestrated versus non-orchestrated mode
- [ ] Quality checks pass

### T-007: Define rollout modes and backward-compatible adoption
**Description:** Define legacy, hybrid, and workflow-first rollout modes so the new control plane can be adopted without breaking current Batty behavior.

**Acceptance Criteria:**
- [ ] Batty defines supported legacy, hybrid, and workflow-first operating modes
- [ ] Legacy mode preserves current runtime behavior and optional current nudge system
- [ ] Hybrid mode allows workflow features to be enabled incrementally without requiring full orchestrator adoption
- [ ] Workflow-first mode is explicitly opt-in rather than forced by default
- [ ] Documentation explains migration and recommended mode selection
- [ ] Quality checks pass

### T-008: Introduce a runnable-work resolver
**Description:** Add daemon logic that determines which tasks are executable now, which are waiting on dependencies, and which role should act next.

**Acceptance Criteria:**
- [ ] Batty can compute runnable tasks from board state without relying on pane text
- [ ] Batty can identify blocked tasks and name the exact unmet dependency or review gate
- [ ] Batty can identify review-owned tasks separately from execution-owned tasks
- [ ] Batty can resolve the correct acting capability or role for the next task transition
- [ ] Unit tests cover dependency resolution, blocked-state resolution, and runnable-state transitions
- [ ] Quality checks pass

### T-009: Add structured completion packets
**Description:** Standardize engineer completion output so Batty and leads can reason over review/merge readiness without relying on free-form prose alone.

**Acceptance Criteria:**
- [ ] Completion packets include task ID, branch, worktree path, commit SHA, changed paths, tests run, artifacts produced, and claimed outcome
- [ ] Batty can parse and store completion packet fields in workflow metadata
- [ ] Missing or malformed packet fields are surfaced as review blockers
- [ ] Unit tests cover packet parsing and failure cases
- [ ] Quality checks pass

### T-010: Implement review and merge state machine
**Description:** Make review disposition explicit so the resolved reviewer decides whether to merge, request rework, discard, archive, or escalate using structured task transitions.

**Acceptance Criteria:**
- [ ] Review state records review owner, review packet reference, and disposition status
- [ ] Merge-ready, rework-required, discarded, and escalated outcomes are distinguishable in task metadata
- [ ] Batty can move a task from `review` to `done`, `in-progress`, `archived`, or `blocked` based on disposition
- [ ] Unit tests cover review disposition transitions and invalid transitions
- [ ] Quality checks pass

### T-011: Implement dependency-aware nudges and escalations
**Description:** Replace generic idle recovery with state-based interventions that target the correct role based on workflow state.

**Acceptance Criteria:**
- [ ] Executors are nudged only for runnable work they still own
- [ ] Dispatchers/reviewers are nudged for review backlog, completion backlog, or unresolved dispatch gaps
- [ ] Planners are nudged for utilization recovery, blocked frontier decisions, or missing runnable work creation
- [ ] Nudges do not fire while pending inbox messages exist and respect configurable idle grace
- [ ] Unit tests cover state-targeted interventions and suppression conditions
- [ ] Quality checks pass

### T-012: Add merge/artifact lifecycle tracking
**Description:** Track the branch and artifact lifetime from engineer execution through review and merge so Batty knows what evidence exists and what is still missing.

**Acceptance Criteria:**
- [ ] Task metadata stores branch/worktree/commit/artifact references when available
- [ ] Batty can display branch/artifact status in `status` or other inspection commands
- [ ] Merge actions update workflow state and clear or preserve relevant metadata intentionally
- [ ] Unit tests cover branch/artifact metadata capture and state cleanup
- [ ] Quality checks pass

### T-013: Introduce workflow observability metrics
**Description:** Add daemon-derived metrics that reflect actual workflow health rather than only pane activity.

**Acceptance Criteria:**
- [ ] Batty records runnable task count, blocked task count, review age, assignment age, and idle-with-runnable-work signals
- [ ] Metrics are derivable from task and event state without external infrastructure
- [ ] `batty status` or a new inspection command exposes high-signal workflow metrics
- [ ] The orchestrator pane and logs expose recent workflow decisions in a human-auditable form
- [ ] Unit tests cover metric computation
- [ ] Quality checks pass

### T-014: Add config-driven workflow policies
**Description:** Make workflow behaviors configurable so teams can enable or tune dependency recovery, review routing, WIP rules, and escalation policies.

**Acceptance Criteria:**
- [ ] Team config can enable or disable workflow interventions independently
- [ ] Team config can define WIP limits or similar concurrency guardrails per role
- [ ] Team config can optionally override capability resolution when role type and hierarchy are insufficient
- [ ] Team config can control grace intervals and escalation thresholds
- [ ] Team config can enable, disable, or place the orchestrator pane/log behavior intentionally
- [ ] Team config can choose orchestrated versus non-orchestrated workflow mode intentionally
- [ ] Team config defaults preserve backward-compatible legacy behavior unless a workflow mode is explicitly enabled
- [ ] Config validation rejects inconsistent workflow policy settings
- [ ] Unit tests cover default and explicit config parsing
- [ ] Quality checks pass

### T-015: Preserve role specialization with explicit contracts
**Description:** Rewrite prompts and role-specific workflow responsibilities so any supported topology operates against the new control plane consistently.

**Acceptance Criteria:**
- [ ] Planner-capable roles describe frontier proposal, prioritization, and utilization recovery duties when applicable
- [ ] Dispatcher-capable roles describe decomposition and routing duties when applicable
- [ ] Reviewer-capable roles describe review, merge, discard, rework, and escalation duties when applicable
- [ ] Executor-capable roles describe bounded execution and structured completion packet duties when applicable
- [ ] Prompts make clear that the orchestrator owns workflow supervision and that agents should treat it as system control, not a peer for work handoff
- [ ] Prompts and docs make clear that orchestrator-triggered state changes are carried out through Batty commands/API, not special hidden powers
- [ ] Prompts and docs make clear how teams operate when the built-in orchestrator is disabled
- [ ] Prompts and docs make clear what remains unchanged in legacy mode
- [ ] Shipped templates with renamed roles remain compatible with the workflow model
- [ ] Prompt/templates remain compatible with current Batty runtime
- [ ] Quality checks pass

### T-016: Add migration and backward compatibility behavior
**Description:** Ensure existing Batty projects can adopt the new workflow model without breaking active runs or older boards.

**Acceptance Criteria:**
- [ ] Batty can read older task files that do not contain new workflow metadata
- [ ] A documented migration path exists for upgrading existing projects
- [ ] Batty defaults undefined fields safely and predictably
- [ ] Existing team configs continue to run without mandatory orchestrator/workflow changes
- [ ] Validation surfaces partial migration states clearly
- [ ] Quality checks pass

### T-017: Validate the new model across representative topologies
**Description:** Verify the workflow-first control plane on representative Batty topologies, including at least one simple hierarchy and one reduced topology.

**Acceptance Criteria:**
- [ ] The workflow model is validated against shipped templates such as solo, pair, simple/squad, and renamed-role research/software variants
- [ ] At least one live run is executed with the workflow-first control plane enabled
- [ ] At least one representative workflow mutation path is validated with the built-in orchestrator disabled
- [ ] At least one existing-style run is validated in legacy mode without behavior regressions
- [ ] Load, assignment, completion, and review metrics are captured before and after
- [ ] At least one substantive branch/review/merge path is completed through the new lifecycle
- [ ] A short postmortem documents what improved and what still failed
- [ ] Quality checks pass

## Functional Requirements

1. **FR-1:** The system must treat board/task metadata as the primary source of orchestration truth for task state, dependency state, review state, and merge state.
2. **FR-2:** The system must preserve markdown task files and remain compatible with kanban-md as the base task management layer.
3. **FR-3:** The system must model task lifecycle explicitly, including runnable, blocked, review-held, merge-ready, rework-required, and completed states.
4. **FR-4:** The system must distinguish execution ownership from review ownership so different workflow capabilities do not collapse into one overloaded `claimed_by` field.
5. **FR-5:** The system must support explicit task dependencies and determine whether a task is runnable from those dependencies.
6. **FR-6:** The system must support structured completion packets for executor-produced work, including branch/worktree/commit/test/artifact evidence where available.
7. **FR-7:** The system must allow the resolved reviewer to disposition reviewed work as merge, rework, archive/discard, or escalate, with those decisions reflected in task state.
8. **FR-8:** The system must allow the resolved planner or dispatcher to manage blocked frontier work by creating new tasks, assigning dependencies, or escalating policy questions.
9. **FR-9:** The system must support optional specialized planning roles, but must not require a dedicated `scientist` or similarly named role.
10. **FR-10:** The system must expose the workflow engine as a first-class orchestrator runtime surface with its own tmux pane and persistent logs.
11. **FR-11:** The orchestrator must perform normal workflow mutations through explicit Batty CLI/API operations rather than hidden internal-only mutation paths.
12. **FR-12:** The same workflow CLI/API surface must be usable by other orchestrators, agents, or humans to reproduce orchestrator actions.
13. **FR-13:** The workflow model must remain usable when the built-in orchestrator is disabled.
14. **FR-14:** In non-orchestrated mode, Batty must still allow inspection and mutation of workflow state through the same CLI/API surface.
15. **FR-15:** The system must support explicit operating modes for legacy, hybrid, and workflow-first usage.
16. **FR-16:** Legacy mode must preserve current Batty behavior unless users explicitly opt into newer workflow features.
17. **FR-17:** The orchestrator must compute which role or capability is responsible for the next action on a task based on current workflow state.
18. **FR-18:** The orchestrator must only send nudges when the task/workflow state indicates a real intervention target and when inbox/preconditions permit.
19. **FR-19:** The system must expose workflow observability signals that reflect runnable work, blocked work, review backlog, idle-with-runnable-work situations, and recent orchestrator decisions.
20. **FR-20:** The system must support configuration of workflow policies such as idle grace, escalation thresholds, intervention toggles, concurrency/WIP rules, capability overrides, orchestrator runtime visibility, and operating mode.
21. **FR-21:** The system must preserve human override through tmux, CLI commands, and direct message/send operations.
22. **FR-22:** The system must remain file-based and git-auditable, with no required external database or service.
23. **FR-23:** The system must work across all valid Batty topologies, including solo, pair, manager-led, multi-manager, and renamed-role configurations.
24. **FR-24:** The system must not hardcode workflow behavior to specific role names such as `scientist`, `architect`, `lead`, or `engineer`.

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
- Responsibility resolution should be based on role type, hierarchy, and optional config overrides rather than raw role names.
- The orchestrator should be visible in tmux as an operational surface, not only as an invisible daemon, while still retaining durable logs for postmortems and debugging.
- Workflow mutations should be represented as reusable command/API operations so the orchestrator is replaceable and replayable rather than privileged by implementation details.
- The same control-plane semantics should remain valid whether the built-in orchestrator is active or absent.
- Backward compatibility should be a first-class design constraint, not a later migration task.

## Success Metrics

- Increase average active load on real project runs from current ~20-30% toward a materially higher steady state.
- Reduce message-to-completion ratio on comparable runs.
- Reduce cases where a task remains `in-progress` while the true next action belongs to review or dependency resolution.
- Reduce time from executor completion packet to reviewer disposition.
- Reduce time a member remains idle while runnable work exists for that role or lane.
- Increase the percentage of interventions that produce a state transition rather than another explanatory message.
- Preserve or improve total completed-task throughput while lowering coordination overhead.
- Make orchestrator decisions inspectable in real time and reconstructable from logs after the fact.
- Allow at least one workflow mutation path to be reproduced from CLI alone without the built-in orchestrator process.
- Allow teams to operate the workflow model manually or agent-driven when they choose not to run the built-in orchestrator.
- Preserve current behavior for existing Batty teams that do not opt into the new workflow layer.

## Open Questions

- Should task dependencies be stored as explicit IDs only, or also grouped into named gate concepts such as "blind-source gate"?
- Should Batty introduce a first-class "merge queue" concept, or is review disposition plus branch metadata sufficient?
- How strict should WIP limits be by default for dispatch/review roles versus executor roles?
- Should review and merge evidence be stored only in task metadata, or also as structured packet files in a separate directory?
- Should workflow capabilities be purely inferred from role type and hierarchy, or should Batty add explicit per-role capability overrides in config?
- Should planner-generated directions become a special task class, or can they use the same task model with different ownership?
- How much automatic state normalization should Batty perform versus only suggesting the transition to the resolved upstream workflow owner?
- Should the orchestrator always occupy a dedicated pane in every topology, or should that be configurable for minimal layouts such as solo/pair?
- Should the workflow API be expressed purely as CLI commands, or should Batty also expose a shared internal command library that the CLI and orchestrator both call?
- Which workflow behaviors, if any, should be unavailable in non-orchestrated mode versus merely manual?
- Which features should be enabled by default in hybrid mode versus requiring explicit opt-in per team?
