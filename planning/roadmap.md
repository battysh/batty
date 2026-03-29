# Batty Roadmap

## Thesis

Developers need a way to describe what they want built and have a team of AI agents autonomously plan, execute, and deliver it. Batty implements this as a goal-directed agent command system: the user describes intent, the architect operationalizes it, and a dynamically-scaled team executes through structured workflow.

## Principles

- Goal-directed by default. The user describes what they want; the architect figures out how.
- The shim is the agent runtime. Each agent runs inside a dedicated shim process that owns its PTY, detects its state, and communicates via structured messages.
- The console TUI is the primary interface. Chat with the architect, view the board, peek at agents — all without tmux knowledge.
- Dynamic topology. The architect scales engineers up/down as the work evolves. No static configuration required.
- Compose, don't monolith. shim + kanban-md + ratatui + BYO agents.
- Ship fast. Validate with real projects before adding complexity.
- Markdown as backend. Files in, files out, git tracks everything.

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

## Error Resilience (Done)

Reduce crash surface in production paths. Make every failure diagnosable.

- **Daemon poll loop isolation** — subsystem calls wrapped with criticality tiers (critical/recoverable/catch_unwind), consecutive failure tracking with escalation warnings. (#183, merged)
- **Module unwrap cleanup** — mod.rs, events.rs, watcher.rs, inbox.rs, merge.rs cleaned with sentinel tests. (#184, merged)
- **daemon.rs and task_loop.rs audit** — confirmed 0 panicking unwraps in production code (all ~559 were in test blocks). Sentinel tests added to guard both files. (#308, #311, merged)

---

## Documentation & Hygiene (Done)

- **Intervention system documentation** — all 7 intervention types documented at docs/interventions.md (456 lines). (#310)
- **README and getting-started refresh** — updated for all features shipped since v0.3.0. (#310, #327 in progress)
- **Dead code audit and compiler warning cleanup** — zero clippy/compiler warnings. 28 stale annotations removed. (#309, merged)

---

## Agent Shim (In Progress — v0.7.0)

Replace tmux-based agent management with a process-per-agent shim architecture. The shim wraps each AI coding CLI behind a message-oriented interface, eliminating tmux capture-pane polling, paste-buffer injection fragility, and the tight coupling between supervision and terminal management.

**Motivation:** Tmux served as the original agent runtime because it provided output capture, input injection, and session persistence for free. But as batty scaled to multi-agent teams, tmux became the primary source of operational fragility: 5-second poll latency masked state changes, paste-buffer injection failed silently, and screen-scraping required agent-specific classifiers entangled with tmux internals. The shim provides sub-second state detection, reliable direct-PTY message delivery, and clean encapsulation of all agent IO behind a typed channel protocol.

**POC:** Validated in `poc/agent-shim/`. Protocol types, all 4 classifiers, PTY management, vt100 virtual screen, state machine, message injection, and response extraction are implemented and tested.

**Spec:** `planning/shim-spec.md`

### Wave 1: Shim Core (T-336, T-337)

Move POC into production and prove it works end-to-end.

- **Integrate shim into main crate** — protocol types, classifiers, shim runtime as modules under `src/`. `batty shim` subcommand as the entry point the daemon will fork/exec. Wire dependencies: portable-pty, vt100, libc. (`T-336`)
- **Chat frontend and integration tests** — `batty chat` command for interactive shim usage. Integration tests with real PTY (bash): send commands, verify completions, test state transitions, message queuing. (`T-337`)

**Exit:** `batty shim` runs as a standalone process. `batty chat` provides interactive access. Integration tests validate core mechanics with real PTY interaction.

### Wave 2: Daemon Adoption (T-338, T-339)

Wire the daemon to manage agents through shims instead of tmux.

- **Spawn agents as shim subprocesses** — daemon creates socketpair, fork/execs `batty shim` per agent, manages via AgentHandle (channel + process). Message delivery through shim channel. State detection via shim events. Readiness gate via Ready event. Coexists with legacy tmux mode via config flag. (`T-338`)
- **Tmux as display layer** — shims write raw PTY output to log files. Tmux panes run `tail -f` for visual monitoring. `batty attach` shows live output. Status bar driven by shim events. (`T-339`)

**Exit:** The daemon operates agents through shims. `batty attach` shows live output. `batty status` reports agent states from shim events. Legacy mode available via `use_shim: false`.

### Wave 3: Lifecycle & Fidelity (T-340, T-341)

Handle the full operational lifecycle and enhance state detection.

- **Agent lifecycle via shim events** — crash recovery (Died events → respawn), context exhaustion handling (ContextExhausted → restart with task summary), graceful shutdown (Shutdown command propagation), health monitoring (Ping/Pong), stale detection (Warning events), session resume. (`T-340`)
- **JSONL session tracking** — port Claude/Codex JSONL trackers into shim classifiers. Merge priority: screen > tracker for Claude, tracker > screen for Codex completion. Graceful degradation when session files unavailable. (`T-341`)

**Exit:** Full agent lifecycle handled through shim events. JSONL tracking enhances state detection for Claude and Codex. Crash recovery and context exhaustion are automated.

### Wave 4: Cleanup & Validation (T-342, T-343)

Remove legacy code and validate the complete system.

- **Remove tmux-direct agent management** — delete watcher module, tmux capture-pane polling, paste-buffer injection, legacy compatibility path. Keep tmux for display. Dead code audit. (`T-342`)
- **End-to-end validation** — full multi-agent team run with shim backend. Mixed backends (Claude + Codex). Test dispatch, review, merge, crash recovery, context exhaustion, session resume. Performance validation (state detection latency). (`T-343`)

**Exit:** Legacy agent management code removed. Multi-agent team operates correctly through shims. State detection latency improved from 5-second poll cycles to sub-second event delivery.

**Success criteria for v0.7.0:** A multi-agent team run completes autonomously with shim backend, with no regressions vs. tmux-direct mode and measurably better state detection latency.

### Immediate Reliability Follow-Up

Recent dogfooding exposed two concrete reliability gaps in shim-era unattended operation: warm resume could fail against missing Codex saved sessions, and branch integration could fail because the main merge context was not clean.

- **Resume fallback hardening** — if saved session lookup fails during resume, the runtime must immediately cold-start the agent and restore task context rather than retrying the invalid session path. Sequencing anchor for the wave. (`T-365`)
- **Crash policy defaults** — unattended team configs should default to `auto_respawn_on_crash: true`, with explicit operator guidance that disabling it is for debugging only and that healthy live panes do not need proactive restart after the setting is enabled. (`T-363`)
- **Recovery validation** — end-to-end shim validation must include missing-session resume cases across architect, manager, and engineer roles so a single stale session reference cannot collapse the whole team. Depends on fallback hardening landing first. (`T-364`, depends on `T-365`)
- **Merge-preflight cleanliness** — merges must verify that the main integration worktree is clean before attempting branch integration, and must surface a structured blocker when local tracked or untracked changes would be overwritten. Recovery should preserve legitimate local work on an isolation branch or worktree before retrying. Added after task `#352` was blocked by dirty main-worktree files and recovered via preserved local state plus clean-retry merge. (`T-367`)

**Exit:** Missing or stale saved-session state no longer causes cascading agent failure. Teams recover automatically without manual pane restarts, and branch integration only runs from a verified clean main context.

---

## Goal-Directed Architecture with Console TUI (In Progress — v0.8.0)

Replace the pre-configured-team-then-execute model with a goal-directed conversational interface backed by a console TUI. The user describes what they want, the architect operationalizes it, dynamically scales the team, and drives execution through assessment cycles.

**Motivation:** Batty v0.7.0 is a powerful execution engine, but requires users to pre-configure topology, populate boards, and navigate tmux. This phase makes Batty conversational: `batty` opens a chat with the architect, who creates the plan, scales the team, and measures progress against the goal — all from a single terminal interface.

**PRD:** `planning/prd-goal-directed-tui.md`

### Wave 1: Foundation (T-351, T-352, T-353)

Build the three independent foundations that everything else depends on.

- **Goal spec and evaluation** — `goal.yaml` schema (description, criteria with automated/agent/human types, budget, constraints). `batty eval` CLI command runs automated criteria and reports results. Architect creates goal.yaml from conversation. (`T-351`)
- **Dynamic topology** — `batty scale engineers N`, `batty scale add-manager <name>`, `batty scale remove-manager <name>` commands. Daemon hot-reloads team.yaml on change: diff running state, spawn/kill shims, update routing. Graceful removal (finish current task before kill). (`T-352`)
- **Daemon console socket** — Unix socket server at `.batty/console.sock`. Protocol: ChatMessage, AgentStatusUpdate, PtyData, SystemEvent, TopologyCommand. Multiple simultaneous console connections. Length-prefixed JSON framing. (`T-353`)

**Exit:** `batty eval` measures goal progress. `batty scale` dynamically adds/removes agents. Daemon serves console connections via socket.

### Wave 2: TUI and Architect Brain (T-354, T-355, T-356, T-357)

Build the console TUI and the goal-directed architect intelligence.

- **TUI skeleton and chat view** — ratatui application with tab-based views (Chat/Board/Agents/Peek), hotkey switching, status bar. Chat view: scrollable message history, input line, system events inline. Connects to daemon console socket. (`T-354`, depends on T-353)
- **Agents view** — live-updating table of agents (name, role, state, current task, inbox count, health age). Aggregate metrics in footer (throughput, review queue, goal progress). Enter to peek. (`T-355`, depends on T-354)
- **Architect goal-directed prompt and assessment loop** — rewrite architect system prompt for goal-directed operation. Assessment cycle: evaluate → strategize → decompose → configure topology → execute → re-evaluate. Architect creates goal.yaml, populates board, issues `batty scale` commands. Triggered after cycle completion or on timer. (`T-356`, depends on T-351, T-352)
- **Board view** — launches kanban-md TUI as subprocess, returns to batty TUI on exit. Board path from daemon. Human can create/edit/move tasks directly. (`T-357`, depends on T-354)

**Exit:** `batty` opens the TUI. Chat with architect works. Agents view shows live status. Board view opens kanban-md. Architect autonomously drives goal through assessment cycles with dynamic scaling.

### Wave 3: Polish and Connectivity (T-358, T-359, T-360)

Add terminal viewing, session management, and cross-interface sync.

- **Agent peek view** — stream PTY log bytes from shim through daemon socket, render with ANSI colors in ratatui. Auto-scroll. Hotkey 1-9 or Enter from agents view. Read-only. (`T-358`, depends on T-354, T-353)
- **Detach/reattach** — `q` detaches console (daemon keeps running). `batty` reattaches. On reattach, architect provides status summary of what happened while detached. Multiple simultaneous consoles. (`T-359`, depends on T-354, T-353)
- **Telegram-console sync** — messages from Telegram appear in console chat and vice versa. Both frontends see the same conversation. System events in both. (`T-360`, depends on T-354)

**Exit:** Full TUI with all 4 views. Agent peek shows live terminal output. Detach/reattach works. Telegram and console are synchronized.

### Wave 4: Persistence and Budget (T-361, T-362)

Make the system resumable and budget-aware.

- **Resume state persistence** — on stop: persist goal.yaml, team.yaml, board, conversation history (last N messages), cycle metrics to `.batty/resume/`. On restart: respawn architect with context, respawn team, architect summarizes progress and continues. (`T-361`, depends on T-356)
- **Budget tracking and cycle history** — track compute spend (time-based or API-derived estimate). Display in TUI status bar. Structured cycle history log (tasks completed, metrics, strategy changes). Architect factors budget into strategy. (`T-362`, depends on T-356, T-354)

**Exit:** Stop and restart preserves full context. Budget tracked and visible. Cycle history informs strategy.

**Success criteria for v0.8.0:** A user types `batty`, describes a goal, and the system autonomously plans, scales, executes, and measures progress. The TUI provides chat, board, agent status, and terminal peek without tmux knowledge. Detach/reattach preserves context.

---

## Tech Stack

| Layer | Choice |
|---|---|
| Core | Rust (clap + tokio) |
| Agent Runtime | Shim (portable-pty + vt100) |
| Human Interface | Console TUI (ratatui) |
| Display (legacy) | tmux (optional, for PTY log tailing) |
| Config | YAML (team + goal) |
| Tasks | Markdown kanban board |
| Logs | JSON lines |
| Comms | Telegram (optional) |
| Monitoring | Grafana + SQLite (optional) |

---

## Architectural Concerns

- ~~interventions.rs ~2,400 lines~~ — decomposed into 7 submodules.
- ~~dispatch.rs ~700 lines~~ — stable.
- All modules now under 2,300 lines after decomposition wave (#312–#321).
- ~~tmux as agent runtime~~ — replaced by shim architecture (v0.7.0). tmux retained as display layer only.
- Console TUI adds ratatui dependency and a new daemon socket server — scope must be kept tight to avoid sprawl.

## Risks

1. **Agent reliability** — coding agents produce inconsistent output. Mitigated by test gating and manager review.
2. ~~**Message delivery** — tmux paste injection can fail if pane is in wrong state.~~ Resolved by shim direct-PTY write.
3. **Context limits** — long-running agents hit context windows. Mitigated by focused task scoping, fresh agent sessions per task, and shim ContextExhausted event detection.
4. **Coordination overhead** — multi-agent communication adds latency. Mitigated by keeping the hierarchy shallow and messages concise.
5. **Shim complexity** — the shim adds a new process boundary. Mitigated by the typed channel protocol, comprehensive integration tests, and the validated POC.
6. **Architect prompt quality** — goal-directed prompts need careful engineering for consistent strategy. Mitigated by iteration on prompt + assessment loop design.
7. **TUI scope creep** — TUI development can sprawl. Mitigated by delegating board to kanban-md and keeping other views minimal (chat=text, agents=table, peek=log stream).
8. **Hot-reload complexity** — dynamically adding/removing agents during execution. Mitigated by graceful shutdown for removed agents and clean init for new agents.
