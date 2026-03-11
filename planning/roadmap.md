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

## Future: Merge and Ship

Orchestrate code integration from multiple engineers working in parallel.

- **Worktree isolation** — each engineer works in a dedicated git worktree
- **Merge queue** — serialized merges from engineer branches into main
- **Conflict resolution** — detect merge conflicts, reassign to engineer for resolution
- **Branch cleanup** — automatic worktree teardown after successful merge

**Success criteria:** Multiple engineers work in parallel on different tasks. Their code merges cleanly into main via an automated queue.

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
