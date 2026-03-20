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

## Future: Intelligence Layer

Make the team smarter over time.

- **Standup summaries** — periodic team status digests sent to architect and user
- **Retrospectives** — post-project analysis of what worked and what didn't
- **Prompt evolution** — templates improve based on observed failure patterns
- **Cross-project learning** — reusable team configurations for different project types

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
