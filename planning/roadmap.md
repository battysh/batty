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

## Production Readiness (Current)

Enable confident hands-off multi-hour runs. The system is functionally complete but has operational gaps that limit unattended operation: 1,282 unwrap/expect calls in production paths, manual review as the #1 throughput bottleneck, single-backend agent coupling, and Telegram delivery failures. This phase closes those gaps.

### Autonomous Pipeline (merged)

Shipped during Stability transition. These features make the pipeline self-managing.

- **Dispatch queue** — board-aware task assignment with WIP enforcement, stabilization delay, worktree readiness checks, persisted queue state. (#145, merged d39bf33)
- **Board replenishment intervention** — nudge architect when todo queue drops below threshold. (#146, merged b995229)
- **Planning directives** — configurable review/escalation/utilization guidance via markdown hooks. (#147, merged c4d8936)
- **Codex session management** — per-instance session tracking, resume by ID, log paths. (#148, merged 4bc9b15)
- **Pipeline starvation fix** — board-aware idle counting, manager-working suppression, 5-min cooldown. (47f0d15, 32dda7a)
- **Dispatch guard** — block assignment to engineers with pending board items. (#151, in progress)
- **Ticket-scoped branches** — `eng-1-N-<task_id>` naming to prevent branch collision. (#152, in progress)

### In Progress

- **Telegram delivery reliability** — fix 3,494 failed sends: retry logic, graceful degradation. (#149, in progress)
- **Kiro agent backend** — integrate kiro-cli as alternative agent backend. (#150, in progress)
- **Orchestrator log beautification** — ANSI colors, visual hierarchy in tmux pane. (#154, queued)

### Wave 1: Error Resilience

Reduce crash surface from 1,282 unwrap/expect calls to under 200 in production paths. Make every failure diagnosable.

- **Unwrap cleanup in daemon.rs and task_loop.rs** — replace unwrap/expect with `?` or explicit error handling in the two highest-traffic modules (515 + 68 calls). Preserve test-only unwraps.
- **Unwrap cleanup in remaining modules** — same treatment for mod.rs (254), events.rs (138), watcher.rs (102), inbox.rs (80), merge.rs (125). Target: total production unwraps under 200.
- **Shell-out error context** — every external command invocation gets `.context()` with: which command, what args, what the daemon was trying to do. Stderr captured in error chain.
- **Daemon poll loop isolation** — each subsystem call in the poll loop wrapped in error recovery with logging. One subsystem failure must not crash the daemon.

**Exit:** Production unwraps under 200. Shell-out failures diagnosable from logs alone. Daemon survives individual subsystem failures.

### Wave 2: Review Automation

The review queue is the #1 throughput killer. Manual review backs up the pipeline and idles engineers. Automate what can be automated while preserving quality gates.

- **Auto-merge policy** — configurable policy in team.yaml: when all tests pass and the diff is under a size threshold, merge without manual review. Policy can be scoped per task priority or tag. Manager retains override.
- **Review timeout and escalation** — tasks sitting in review beyond a configurable threshold auto-escalate. First to the manager (nudge), then to architect if still stale. Prevents silent review queue rot.
- **Structured review feedback** — when manual review is required, the reviewer records disposition (approve/request-changes/reject) with specific feedback via `batty review`. Rework cycles get the exact feedback, not a generic "fix it" nudge.

**Exit:** Low-risk tasks merge automatically on test pass. Review queue drains within configured time bounds. Review feedback is structured and actionable.

### Wave 3: Agent Backend Abstraction

Building on #150 (kiro integration), make agent backends a first-class pluggable concept. Different projects and different roles may benefit from different agents.

- **Backend trait** — define the interface that all agent backends must implement: launch, health check, inject message, detect status, session management. Current Claude Code and Codex backends extracted to this interface.
- **Mixed-backend teams** — team.yaml supports per-role `agent` field choosing the backend. A team can run architect on Claude Code, manager on Claude Code, and engineers on Codex or Kiro.
- **Backend health and fallback** — health checks per backend type. If a backend fails to launch or becomes unhealthy, surface it through `batty doctor` and `batty status`.

**Exit:** Agent backends are pluggable. Teams can mix backends. Backend health is observable.

### Wave 4: Documentation & Hygiene

Close remaining documentation gaps and clean up accumulated technical debt.

- **Intervention system documentation** — document the 5 intervention types, triggers, state machines, cooldown/dedup, and configuration.
- **README and getting-started refresh** — update all user-facing docs to reflect Stability and Production Readiness features.
- **Dead code audit and compiler warning cleanup** — zero warnings target. Remove unused dependencies and dead code paths.

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

## Architectural Concerns

- **interventions.rs ~2,400 lines** — the board replenishment intervention added ~900 lines. This module needs decomposition in a future phase (extract each intervention type into its own submodule).
- **dispatch.rs ~700 lines** — grew significantly with the dispatch queue. Monitor for further growth.

## Risks

1. **Agent reliability** — coding agents produce inconsistent output. Mitigated by test gating and manager review.
2. **Message delivery** — tmux paste injection can fail if pane is in wrong state. Mitigated by daemon retry and status checking.
3. **Context limits** — long-running agents hit context windows. Mitigated by focused task scoping and fresh agent sessions per task.
4. **Coordination overhead** — multi-agent communication adds latency. Mitigated by keeping the hierarchy shallow and messages concise.
