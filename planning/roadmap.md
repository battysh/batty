# Batty Roadmap

## Thesis

Developers need a way to run teams of AI agents that coordinate, communicate, and ship code autonomously. Batty implements this as a hierarchical agent command system on top of tmux.

## Principles

- tmux is the runtime. Not a stopgap — the permanent architecture.
- Compose, don't monolith. tmux + kanban-md + BYO agents.
- Ship fast. Validate with real projects before adding complexity.
- Markdown as backend. Files in, files out, git tracks everything.
- Hierarchy creates focus. Architect thinks, manager coordinates, engineers build.

---

## Historical: Phase-Based System (Complete)

The original Batty was a phase-based execution system (`batty work <phase>`) with supervisor/executor/director layers. Phases 1 through 4 shipped sequentially, covering: core agent runner, tmux supervisor, prompt detection, test gates, worktree isolation, runtime hardening, DAG scheduling, parallel execution, and merge queues.

This system was fully replaced by the team-based architecture in a ground-up rewrite. The phase-based code is gone; the lessons remain.

---

## Team Architecture: Foundation (Done)

Ground-up rewrite to hierarchical agent teams.

- **YAML-defined org chart** — architect, managers, engineers with configurable instances
- **CLI rewrite** — `init`, `start`, `stop`, `attach`, `status`, `send`, `inbox`, `assign`, `validate`, `config`, `merge`, `completions`
- **tmux layout builder** — automatic pane creation and zone-based grouping
- **Daemon** — background process for message routing, pane monitoring, status tracking
- **Watcher system** — per-agent status detection (idle, working, waiting) via pane output analysis
- **Message routing** — inbox-based delivery via tmux paste-buffer injection
- **Event logging** — all team events persisted to JSONL
- **Prompt templates** — role-specific system prompts bundled via `include_str!()`
- **Telegram bridge** — remote monitoring and message relay via Telegram bot
- **Dogfood** — Batty's own development runs on Batty teams

**Exit:** Team spawns, communicates, and operates on real projects. Telegram bridge enables remote oversight.

---

## Stabilization (Done)

Fixed build, hardened existing code, synced documentation.

- **Build fix** — resolved test compilation errors, clean build on main
- **Documentation sync** — CLAUDE.md, README, and planning docs updated to match team architecture
- **Test coverage** — 338 tests (310 batty + 28 docsgen), all passing. Coverage added for events, layout, standup, message modules
- **Template hardening** — prompt templates battle-tested across multi-agent sessions
- **Telegram bridge** — message splitting for 4096-char API limit, scoped `batty stop` to current project

**Exit:** Clean build, 338 tests pass, docs match reality.

---

## Autonomous Task Loop (Done)

End-to-end loop: board task → assign → engineer works → test → report → next task.

- **Board-driven dispatch** — daemon auto-assigns unclaimed tasks to idle engineers, 10s rate limit, hierarchy-scoped
- **Task completion detection** — idle-after-working triggers test gating for engineers with active tasks
- **Test gating** — `cargo test` in engineer worktree; pass → mark done + merge, fail → retry (max 2) → escalate
- **Progress reporting** — structured summaries flow engineer → manager → architect on completion and escalation
- **Failure handling** — 2 retries then escalation to manager, task blocked on board, architect notified

**Exit:** 357 tests passing. Given a populated board, the team works through tasks autonomously with test gating and progress reporting.

---

## Merge and Ship (Done)

Orchestrated code integration from multiple engineers working in parallel.

- **Worktree isolation** — each engineer works in a dedicated git worktree, refreshed before each assignment
- **Merge queue** — file-based lock serializes merges, rebase-before-merge ensures branches are current
- **Conflict resolution** — rebase conflicts detected and sent back to engineer via retry mechanism (max 2 retries, then escalate)
- **Branch cleanup** — `git reset --hard main` after successful merge leaves worktree clean and ready for next task

**Exit:** 362 tests passing. Engineers work in parallel, merges serialize safely, conflicts retry automatically, worktrees reset after each merge.

---

## Workflow Control Plane Rework (Done)

Move Batty from message-inferred coordination to explicit workflow state, visible orchestration, and deterministic recovery. Preserves current team model as supported legacy mode. PRD: [tasks/prd-batty-workflow-control-plane-rework.md](../tasks/prd-batty-workflow-control-plane-rework.md).

### Wave 1: Foundation Models & Board Extensions (T-001, T-002, T-003, T-007, T-016)

Establish the conceptual and data foundations before building runtime behavior.

- **Capability model** — define planner/dispatcher/executor/reviewer/orchestrator/operator responsibilities and how they resolve from role type + hierarchy across all topologies (`T-001`)
- **Workflow state model** — define task lifecycle states, ownership types, dependency semantics, and runnable/blocked criteria (`T-002`)
- **Board metadata extensions** — add workflow fields (`depends_on`, `review_owner`, `blocked_on`, `branch`, `commit`, `artifacts`, `next_action`) to task frontmatter without breaking kanban-md (`T-003`)
- **Rollout mode definitions** — define legacy/hybrid/workflow-first modes and backward-compatible adoption path (`T-007`)
- **Migration and backward compatibility** — older task files handled with safe defaults, existing configs run unchanged (`T-016`)

**Exit:** Written capability model, state model, extended board format, rollout modes, and migration behavior. All existing tests still pass.

### Wave 2: Runtime Engine (T-004, T-005, T-006, T-008, T-009, T-010, T-012)

Build the workflow engine and expose it through CLI/API.

- **Orchestrator runtime surface** — visible tmux pane for workflow decisions and activity (`T-004`)
- **CLI/API control surface** — explicit commands for task create, update state, assign, record review, trigger merge/archive/rework (`T-005`)
- **Orchestrated and non-orchestrated modes** — workflow works with or without built-in orchestrator (`T-006`)
- **Runnable-work resolver** — compute runnable/blocked/review tasks from board state without pane text (`T-008`)
- **Structured completion packets** — standardized engineer output with branch/commit/test/artifact evidence (`T-009`)
- **Review and merge state machine** — explicit review disposition driving task transitions (`T-010`)
- **Merge/artifact lifecycle tracking** — branch/artifact lifetime from execution through merge (`T-012`)

**Exit:** Orchestrator pane running, workflow mutations via CLI, review/merge state machine operational, completion packets parsed. Unit tests cover all new paths.

### Wave 3: Intelligence & Polish (T-011, T-013, T-014, T-015, T-017)

Wire up smart interventions, observability, and align prompts.

- **Dependency-aware nudges** — state-based interventions targeting the correct role (`T-011`)
- **Workflow observability metrics** — runnable count, blocked count, review age, idle-with-runnable signals (`T-013`)
- **Config-driven workflow policies** — WIP limits, escalation thresholds, intervention toggles, capability overrides (`T-014`)
- **Role prompt rewrite** — align prompts with capability model and control plane contracts (`T-015`)
- **Topology validation** — validate across solo, pair, manager-led, multi-manager, renamed-role topologies (`T-017`)

**Exit:** Batty drives execution, review, merge, recovery, and escalation from structured workflow state. Orchestrator visible in tmux. All mutations available through stable CLI/API. Prompts aligned. Metrics observable.

**Result:** 17 tasks shipped, 12 new modules, 593 tests (231 new). Validated across solo, pair, manager-led, multi-manager, and renamed-role topologies.

---

---

## Intelligence Layer (Done)

Make the team self-aware and self-improving. Build on the event stream, workflow metrics, and standup infrastructure to close the feedback loop.

### Wave 1: Periodic Standups (T-101, T-102, T-103)

Turn the existing standup module from a one-shot report into a daemon-driven periodic system with board-aware content.

- **Configurable standup interval** — `standup_interval_secs` in team.yaml; daemon triggers standup generation on the configured cadence (default: 300s). Zero disables. (`T-101`)
- **Board-aware standup content** — standups include assigned task IDs, blocked items, review queue age, and idle-with-runnable warnings alongside agent status (`T-102`)
- **Standup delivery to user** — standups for the user role are delivered via Telegram (if configured) or written to `.batty/standups/` as timestamped markdown files (`T-103`)

**Exit:** Standups fire automatically at the configured interval. Each standup shows agent status plus board context. User receives standups via Telegram or file.

### Wave 2: Run Retrospectives (T-104, T-105, T-106)

Automated post-run analysis from the event log and board state.

- **Event log analyzer** — parse events.jsonl to compute per-task cycle time, failure/retry counts, escalation frequency, merge conflict rate, idle-time percentage (`T-104`)
- **Retrospective generator** — produce a structured markdown retrospective identifying top bottlenecks, longest-running tasks, most-failed tasks, and review queue stalls. Written to `.batty/retrospectives/` (`T-105`)
- **Retrospective trigger** — `batty retro` CLI command generates a retrospective on demand. Daemon auto-generates one when all board tasks reach done/archived (`T-106`)

**Exit:** `batty retro` produces useful post-run analysis. Auto-retro fires when a run completes. Retrospectives are human-readable markdown.

### Wave 3: Failure Pattern Detection (T-107, T-108)

Surface recurring problems before they compound.

- **Failure signature tracker** — daemon maintains a rolling window of failure events (test failures, escalations, merge conflicts) and detects repeated patterns (same engineer, same file, same error class) (`T-107`)
- **Pattern notification** — when a pattern exceeds a configurable threshold, surface a structured observation to the manager or architect via the existing message bus. Not automated remediation — automated noticing (`T-108`)

**Exit:** Repeated failures are detected and surfaced as messages. Configurable thresholds prevent noise.

### Wave 4: Team Templates (T-109, T-110)

Reusable team configurations for cross-project learning.

- **Template export** — `batty export-template <name>` saves the current team.yaml, prompt files, and workflow policies as a named template to `~/.batty/templates/` (`T-109`)
- **Template init** — `batty init --from <name>` bootstraps a project from a saved template instead of defaults (`T-110`)

**Exit:** Users can save and reuse successful team configurations across projects.

**Result:** 10 tasks shipped across 4 waves, 594 tests (up from 556). Periodic standups, run retrospectives, failure pattern detection, and team templates all operational.

---

## Operational Hardening (Done)

Targeted reliability fixes based on dogfooding observations. Daemon tracing, resume logging, diagnostics, prompt refresh, test deflaking, pipeline starvation detection.

**Result:** 8 tasks shipped across 2 waves. `batty doctor` diagnostics, daemon log rotation, structured resume logging, refreshed prompt templates, deflaked timing-sensitive tests, pipeline starvation detection. Build green: 668 tests passing.

---

## Hardened Runtime (Done)

Decomposed daemon.rs from 9249 lines into dedicated modules. Shipped typed command layers, delivery reliability, agent lifecycle management, and board integrity checks across 4 waves, 62 tasks (T-050 through T-111). Build green: 812 tests passing.

**Result:** daemon.rs modularized. Shell-outs wrapped in typed layers. Worktree health checks, CWD validation, failure pattern detection, event log rotation, `batty doctor --fix`, `batty export-run`, `batty board list/summary`, daemon preflight checks, graceful shutdown, agent health summary, cost estimation, task cycle time tracking, performance regression detection, and machine-readable status output all operational.

---

## Stability & Predictability (Done)

Make the system deterministic, well-tested, and resilient. Closed critical gaps exposed by dogfooding: zero integration tests → full intervention test suite, test coverage expanded across all major modules, prompt-sourced nudge system, domain error types, planning directives.

- **Wave 1: Orchestrator & Intervention Integration Tests** — complete. Integration test harness, triage/owned-task/review/dispatch/utilization intervention tests, orchestrator log verification, multi-intervention choreography, edge cases. (T-501 through T-507, #144)
- **Wave 2: Test Coverage Expansion** — complete. status.rs, doctor.rs, merge.rs, interventions.rs, nudge.rs, event log, config validation all covered. (T-508 through T-514)
- **Wave 3: Refactoring & Error Handling** — partial. Domain error types shipped (T-515, #141). Remaining unwrap cleanup (1,282 across key modules), shell-out context, poll loop isolation, panic safety deferred to Production Readiness.
- **Wave 4: Prompt-Sourced Nudge Messages** — complete. Nudge sections in all role prompts, combined nudge+intervention messages, planning directives for configurable review/escalation/utilization guidance. (#142, #147)
- **Wave 5: Documentation & Maintenance** — partial. Daemon module map done (#137). Remaining intervention docs, dead code audit, compiler warnings, README refresh deferred.

**Result:** 20 tasks shipped. Integration test suite, test coverage expansion, nudge system, planning directives, domain error types all operational. Remaining error handling and docs rolled into next phase.

---

## Production Readiness (Done)

Enable confident hands-off multi-hour runs. The system is functionally complete but has operational gaps that limit unattended operation: 1,282 unwrap/expect calls in production paths, manual review as the #1 throughput bottleneck, single-backend agent coupling, and Telegram delivery failures. This phase closes those gaps.

### Autonomous Pipeline (merged)

Shipped during Stability transition. These features make the pipeline self-managing.

- **Dispatch queue** — board-aware task assignment with WIP enforcement, stabilization delay, worktree readiness checks, persisted queue state. (#145, merged d39bf33)
- **Board replenishment intervention** — nudge architect when todo queue drops below threshold. (#146, merged b995229)
- **Planning directives** — configurable review/escalation/utilization guidance via markdown hooks. (#147, merged c4d8936)
- **Codex session management** — per-instance session tracking, resume by ID, log paths. (#148, merged 4bc9b15)
- **Pipeline starvation fix** — board-aware idle counting, manager-working suppression, 5-min cooldown. (47f0d15, 32dda7a)
- **Dispatch guard** — block assignment to engineers with pending board items. (#151, merged)
- **Ticket-scoped branches** — `eng-1-N-<task_id>` naming to prevent branch collision. (#152, merged)
- **Telegram delivery reliability** — retry logic, graceful degradation, dedup. (#149, merged)
- **Kiro agent backend** — integrate kiro-cli as alternative agent backend. (#150, merged)
- **Orchestrator log beautification** — ANSI colors, visual hierarchy in tmux pane. (#154, merged)
- **Shell-out error context** — `.context()` on external command invocations. (merged)
- **Telegram dedup** — dedup outbound Telegram delivery. (#159, merged)
- **Starvation state preservation** — preserve starvation state until backlog clears. (merged)

### Deferred to Future Phases

The following planned waves were descoped from v0.3.0. They remain valuable but were deprioritized in favor of shipping the autonomous pipeline and dogfooding fixes.

- **Error Resilience** — unwrap cleanup (1,282 → <200 target), daemon poll loop isolation. Shell-out error context partially shipped.
- **Review Automation** — auto-merge policy, review timeout/escalation, structured review feedback. Still the #1 throughput killer; high priority for a future phase.
- **Agent Backend Abstraction** — backend trait, mixed-backend teams, backend health. Kiro backend shipped as a first step.
- **Documentation & Hygiene** — intervention docs, README refresh, dead code audit, compiler warning cleanup.

**Result:** v0.3.0 shipped. 16+ tasks merged, 594 tests, autonomous pipeline operational. Dispatch queue, WIP guard, ticket-scoped branches, Telegram reliability, pipeline starvation detection, and kiro backend all merged. Remaining waves rolled forward.

---

## Dogfooding Fixes (Done)

Bugs and improvements surfaced from running Batty on two real projects simultaneously (batty-dev with 4 engineers, batty-marketing with 4 engineers in a non-git workspace). Every item below is a real failure observed in production runs, not a theoretical concern.

### Wave 1: Daemon State Hygiene

The daemon tracks task assignments internally, but this tracking drifts from board truth. Engineers get stuck, completions are rejected, and stale entries linger.

- **Active-task reconciliation** — daemon-state `active_tasks` retains entries for tasks already marked done on the board (observed: eng-1-3 tracked on done task #155 with retry_count=2). The daemon must reconcile active_tasks against board state each poll cycle and clear stale entries.
- **Completion rejection recovery** — "completion rejected because branch has no commits" fires repeatedly for the same engineer/task with no auto-recovery (observed: eng-1-3/task-155, fired twice at 23:44 and 00:00). After rejection, the daemon should clear the stale assignment and return the engineer to idle rather than leaving them in limbo.
- **Pane cwd correction on resume** — after `batty stop` + `batty start` (resume), engineer panes have their cwd at the project root instead of their worktree directory. The correction logic runs but fails for 3 of 4 engineers (observed: eng-1-1, eng-1-2, eng-1-3 all failed). Fix the cwd correction to reliably reset panes to their worktree paths.

**Exit:** No stale active_tasks entries when corresponding board tasks are done. Completion rejection triggers cleanup, not infinite retry. Resume restores pane cwds reliably.

### Wave 2: Non-Code Project Support

Batty was designed for code projects with git repos, but batty-marketing proves it's useful for non-code workspaces too. Several assumptions break in that context.

- **Skip worktree setup when disabled** — the daemon launcher attempts worktree setup even when `use_worktrees: false` is set in team.yaml, generating 42+ WARN entries per session (observed in batty-marketing). Gate all worktree operations on the config flag so non-git projects run cleanly.
- **Non-git-repo graceful handling** — when the project directory is not a git repository, git-dependent operations (branch detection, merge, worktree refresh) should be skipped entirely with a single INFO log, not attempted and failed with repeated WARNs.
- **External message sources** — the email router daemon sends messages from `email-router` which is blocked because it's not a recognized role in `talks_to` config (observed: routing blocked from email-router to maya-lead and jordan-pm). Add a configurable `external_senders` list in team.yaml that permits message delivery from non-role sources.

**Exit:** Non-git projects run without worktree warnings. External message sources can be explicitly allowed in config.

### Wave 3: Operational Cleanup

Test debris and trivial artifacts accumulate across runs.

- **Test session cleanup** — 39 orphaned tmux sessions from test runs remain after tests complete (observed: `batty-test-daemon-lifecycle-*`, `batty-test-restart-*`, etc.). Test harness must reliably clean up sessions in teardown, including on test failure/panic. Add a `batty doctor --fix` check that detects and offers to clean up orphaned test sessions.
- **Trivial retrospective suppression** — retrospective generated on a 4-second run with 0 completed tasks (observed: `1774145470.md`). Skip auto-retrospective generation for runs shorter than a configurable minimum duration (default: 60 seconds).
- **Leftover branch cleanup on task completion** — eng-1-2 retains branch `eng-1-2/task-158-notes` after task #158 is done instead of being reset to base branch. The post-merge worktree reset must reliably return all engineer worktrees to their base branch.

**Exit:** No orphaned test sessions after test runs. No trivial retrospectives. Engineer worktrees clean after task completion.

**Result:** 9 tasks shipped across 3 waves. Active-task reconciliation, completion rejection recovery, pane cwd fix, worktree skip, non-git handling, external senders, test session cleanup, retro suppression, and post-merge branch cleanup all operational.

---

## Review Automation (Done)

Auto-merge policy engine, daemon integration with per-task overrides, stale review escalation with structured feedback, and review observability metrics.

**Result:** 4 tasks shipped. Auto-merge policy engine with confidence scoring, wired into daemon completion path. Review timeout escalation with configurable thresholds. Structured review feedback stored in task frontmatter. Review metrics in status, standups, and retrospectives.

---

## Error Resilience (Partial — Continuing)

Reduce crash surface from 1,282 unwrap/expect calls to under 200 in production paths. Make every failure diagnosable.

### Shipped

- **Daemon poll loop isolation** — subsystem calls wrapped with criticality tiers (critical/recoverable/catch_unwind), consecutive failure tracking with escalation warnings. (#183, merged)
- **Remaining module unwrap cleanup** — mod.rs, events.rs, watcher.rs, inbox.rs, merge.rs cleaned with sentinel tests. (#184, merged)

### Deferred to Next Session

- **daemon.rs and task_loop.rs unwrap cleanup** — the largest cleanup (~580 calls). eng-1-4 stalled on this; carrying forward. (#182, partial/closed)

---

## Documentation & Hygiene (Partial — Continuing)

Close documentation gaps and clean up accumulated technical debt. Lightweight tasks suitable for parallel execution alongside heavier phases.

### Wave 1: Documentation

- **Intervention system documentation** — document all 5 intervention types (triage, owned-task, review, dispatch, utilization), triggers, state machines, cooldown/dedup, and configuration.
- **README and getting-started refresh** — update user-facing docs to reflect all features shipped since v0.3.0: dogfooding fixes, review automation, error resilience, external senders, non-git support, auto-merge policy.

### Wave 2: Hygiene

- **Dead code audit and compiler warning cleanup** — zero warnings target. Remove unused dependencies, dead code paths, stale imports. Clean `cargo clippy --all-targets`.

**Exit:** System fully documented. Zero compiler warnings. User-facing docs current.

---

## Tech Stack

| Layer | Choice |
|---|---|
| Core | Rust (clap + tokio) |
| Runtime | tmux |
| Config | YAML (team) + TOML (project) |
| Tasks | Markdown kanban board |
| Logs | JSON lines |
| Comms | Telegram (optional) |

---

## Review Automation (Next)

The review queue is the #1 throughput killer across all dogfooding sessions. Engineers complete tasks and sit idle while the manager manually reviews, tests, and merges. This phase automates the safe cases and adds time-bounded escalation for the rest.

### Wave 1: Auto-Merge for Low-Risk Tasks

Eliminate manual review for tasks that pass automated quality gates. The manager still handles complex or risky changes, but routine tasks (small diffs, all tests pass, no conflict) merge automatically.

- **Auto-merge policy engine** — configurable policy in team.yaml defining when a completed task can be merged without manual review. Policy evaluates: test pass/fail, diff size (lines changed), file count, conflict status, task priority, and optional tag-based rules. When all criteria are met, the daemon merges directly. Manager retains a per-task override to force manual review.
- **Merge confidence scoring** — before auto-merging, compute a confidence score based on diff characteristics (test coverage of changed files, number of modules touched, presence of migration/config changes). Low-confidence changes are routed to manual review even if they meet the size threshold. This prevents auto-merging a 10-line change that touches 5 critical modules.

**Exit:** Tasks that pass tests and meet policy thresholds merge without human intervention. Manager reviews only what needs human judgment.

### Wave 2: Review Queue Drain

Prevent the review queue from silently rotting. Add time-based escalation and structured feedback.

- **Review timeout and escalation** — tasks in review status beyond a configurable threshold trigger escalation. First: nudge the reviewer (manager). If still stale after a second threshold: escalate to architect. Configurable per-priority (high-priority tasks escalate faster). Prevents the pattern where 3 tasks sit in review while engineers idle.
- **Structured review feedback** — when manual review is required, the reviewer records disposition (approve/request-changes/reject) with specific, actionable feedback via `batty review <task_id> <disposition> "<feedback>"`. Rework cycles receive the exact feedback, not a generic "fix it" nudge. Feedback is stored in task frontmatter for traceability.

**Exit:** Review queue drains within configured time bounds. Review feedback is structured, stored, and actionable.

### Wave 3: Review Observability

Make the review pipeline visible and measurable.

- **Review queue metrics** — track review queue depth, average review latency, auto-merge rate, manual review rate, and rework rate. Surface in `batty status` and periodic standups. This gives the architect data to tune auto-merge thresholds.
- **Review history in retrospectives** — include review queue stall duration, auto-merge vs manual-merge ratio, and rework cycles per task in run retrospectives. Identifies whether auto-merge thresholds are too aggressive or too conservative.

**Exit:** Review pipeline performance is observable and feeds back into policy tuning.

---

## Architectural Concerns

- **interventions.rs ~2,400 lines** — the board replenishment intervention added ~900 lines. This module needs decomposition in a future phase (extract each intervention type into its own submodule).
- **dispatch.rs ~700 lines** — grew significantly with the dispatch queue. Monitor for further growth.

## Risks

1. **Agent reliability** — coding agents produce inconsistent output. Mitigated by test gating and manager review.
2. **Message delivery** — tmux paste injection can fail if pane is in wrong state. Mitigated by daemon retry and status checking.
3. **Context limits** — long-running agents hit context windows. Mitigated by focused task scoping and fresh agent sessions per task.
4. **Coordination overhead** — multi-agent communication adds latency. Mitigated by keeping the hierarchy shallow and messages concise.
