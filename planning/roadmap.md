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

## Stability & Predictability (Current)

Make the system deterministic, well-tested, and resilient. The last runtime completed 56 tasks in 7.5 hours with zero merge failures and 812 tests, but exposed: 3,494 failed Telegram sends, 14 stuck message injections, review queue bottlenecks, 1,649 unwrap/expect calls in production code, and zero integration tests. This phase closes those gaps.

### Wave 1: Orchestrator & Intervention Integration Tests (T-501 through T-507)

The orchestrator/intervention system is the brain of autonomous operation but has no dedicated integration tests. interventions.rs (1482 lines) has zero in-file unit tests.

- **Integration test harness** — create `tests/` directory with fixture helpers for constructing mock daemon state, board state, and member availability without tmux. Reusable across all integration test files. (`T-501`)
- **Triage intervention tests** — test the full triage flow: idle manager + pending direct-report results → intervention fires → message queued → cooldown respected → signature dedup prevents repeats. (`T-502`)
- **Owned-task intervention tests** — idle engineer with unfinished assigned task → nudge fires → escalation after threshold → parent role notified. Test the state machine: idle epoch, signature change, escalation flag. (`T-503`)
- **Review/dispatch/utilization intervention tests** — cover the remaining 3 intervention types: review backlog detection, manager dispatch gap, architect utilization alert. Each with cooldown and dedup. (`T-504`)
- **Orchestrator log verification tests** — verify that orchestrator actions produce correct timestamped log entries, respect the enabled/disabled flag, and survive log rotation. (`T-505`)
- **Multi-intervention choreography test** — verify the strict ordering (triage → owned → review → dispatch → utilization) and that one intervention firing doesn't suppress others in the same cycle. (`T-506`)
- **Intervention edge cases** — zero engineers, single-role topology, all members working (no intervention should fire), inbox-suppression correctness, cooldown boundary conditions. (`T-507`)

**Exit:** Orchestrator and intervention system has dedicated integration test suite. Each intervention type tested end-to-end including state machine, cooldown, escalation, and dedup.

### Wave 2: Test Coverage Expansion (T-508 through T-514)

Close the critical coverage gaps: status.rs (8 tests / 1446 lines), doctor.rs (15 tests / 1962 lines), merge.rs (14 tests / 984 lines), nudge.rs edge cases.

- **status.rs test coverage** — add tests for workflow metric computation, health summary generation, JSON output formatting, edge cases (empty board, all-idle, all-working). Target: 25+ tests. (`T-508`)
- **doctor.rs test coverage** — add tests for each diagnostic check: worktree validation, branch consistency, board-git cross-reference, dependency visualization, --fix cleanup. Target: 30+ tests. (`T-509`)
- **merge.rs scenario tests** — test rebase-before-merge, cherry-pick fallback, conflict detection and retry, branch cleanup after merge, concurrent merge lock contention. (`T-510`)
- **interventions.rs unit tests** — extract testable units from the 1482-line file: message builders, eligibility checks, cooldown logic, signature computation. Add in-file unit tests. (`T-511`)
- **nudge.rs edge case tests** — zero members, conflicting capabilities, transitive dependencies, multi-hop blocked chains, all members busy, capability overrides from config. (`T-512`)
- **event log round-trip tests** — verify event serialization/deserialization, rotation behavior, large file handling, concurrent write safety. (`T-513`)
- **config validation tests** — malformed YAML, missing required fields, conflicting settings, workflow mode edge cases, automation config combinations. (`T-514`)

**Exit:** Critical modules reach 2-3% test-to-line ratio minimum. All major code paths covered with happy path + edge case + error tests.

### Wave 3: Refactoring & Error Handling (T-515 through T-520)

Reduce crash surface and improve error diagnostics. Currently 1,649 unwrap/expect calls in the team module.

- **Domain error types** — define `GitError`, `BoardError`, `TmuxError`, `DeliveryError` enums with structured variants. Replace `anyhow::bail!` in core paths with typed errors that callers can match on. (`T-515`)
- **Reduce unwrap/expect in daemon.rs** — audit the 408 unwrap/expect calls in daemon.rs. Replace with `?` or explicit error handling where the panic is not safe. Preserve test-only unwraps. Target: under 50 in production code paths. (`T-516`)
- **Reduce unwrap/expect in remaining modules** — same treatment for mod.rs (234), events.rs (107), watcher.rs (89), inbox.rs (79), merge.rs (69), task_loop.rs (65). Target: total team module production unwraps under 200. (`T-517`)
- **Shell-out error context** — add `.context()` to all 14 files that invoke external commands. Every shell-out failure should include: which command, what args, what stderr, and what the daemon was trying to do. (`T-518`)
- **Daemon poll loop error isolation** — ensure a failure in one poll-loop subsystem (e.g., standup generation fails) does not crash the entire daemon. Each subsystem call should be wrapped in error recovery with logging. (`T-519`)
- **Test for panic safety** — add tests that verify production code paths do not panic on malformed input: empty strings, missing files, corrupt JSON, unexpected board states. (`T-520`)

**Exit:** Production code has typed errors, minimal unwraps, full shell-out context, and isolated subsystem failures. Panic safety verified by tests.

### Wave 4: Prompt-Sourced Nudge Messages (T-521 through T-524)

Restore and enhance prompt-sourced nudges. Each role gets a `## Nudge` section in its prompt template. The daemon extracts this at startup and combines it with the standard intervention message when nudging. This gives roles role-specific guidance ("check the board", "review pending PRs") alongside the system-generated context ("3 tasks in review, eng-1-2 idle for 5 min").

- **Nudge section in all role prompts** — add `## Nudge` sections to manager and engineer prompt templates (architect already has one). Content should be actionable role-specific reminders. (`T-521`)
- **Combined nudge message format** — when the daemon fires a nudge or intervention, prepend the role's extracted nudge text to the system-generated message. Format: nudge text first (role guidance), then the specific intervention context (board state, task IDs). (`T-522`)
- **Configurable nudge intervals per role** — ensure `nudge_interval_secs` works for manager and engineer roles (currently only architect uses it). Add to team.yaml config for all agent roles. (`T-523`)
- **Nudge message tests** — test extraction from prompts (present, absent, malformed), combination with intervention messages, per-role interval firing, and inbox suppression. (`T-524`)

**Exit:** All roles have prompt-sourced nudge guidance. Nudges combine role-specific advice with system-generated intervention context. Nudge intervals configurable per role.

### Wave 5: Documentation & Maintenance (T-525 through T-529)

- **Intervention system documentation** — document the 5 intervention types, their triggers, state machines, cooldown/dedup behavior, and configuration knobs. Add to docs/ or as inline module docs. (`T-525`)
- **Daemon module map** — document the daemon's module decomposition (dispatch.rs, delivery.rs, interventions.rs, telemetry.rs, launcher.rs, merge.rs, etc.) with responsibility descriptions and call flow diagrams. (`T-526`)
- **Dead code audit** — run `cargo +nightly udeps` or manual audit to find unused dependencies, dead functions, unreachable code paths. Remove what's dead. (`T-527`)
- **Compiler warning cleanup** — zero warnings target. Fix all clippy warnings at `warn` level. Add `#![warn(clippy::all)]` to lib.rs. (`T-528`)
- **README and getting-started refresh** — update README, getting-started guide, and CLI reference to reflect all new commands and features added in Hardened Runtime and Stability phases. (`T-529`)

**Exit:** System internals documented. Dead code removed. Zero compiler warnings. User-facing docs current.

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

## Risks

1. **Agent reliability** — coding agents produce inconsistent output. Mitigated by test gating and manager review.
2. **Message delivery** — tmux paste injection can fail if pane is in wrong state. Mitigated by daemon retry and status checking.
3. **Context limits** — long-running agents hit context windows. Mitigated by focused task scoping and fresh agent sessions per task.
4. **Coordination overhead** — multi-agent communication adds latency. Mitigated by keeping the hierarchy shallow and messages concise.
