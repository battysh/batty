# Batty Roadmap

## North Star

**Stability and resilience.** Batty must run multi-agent teams for hours without stalling, looping, or losing work. The nether_earth project is our live test bed — every hour we inspect it, find issues, fix them, and validate that the system self-recovers. Feature work is secondary to operational reliability.

## Principles

- **Stability first.** No feature ships unless the system can run unattended for 8+ hours without intervention.
- **Self-healing by default.** Every failure mode the daemon can detect, it must recover from automatically — orphaned tasks, stuck agents, stale worktrees, dead shims, state desync.
- **Observable.** Every problem must be visible in logs, telemetry, and Grafana dashboards before it becomes a stall.
- **Tested against reality.** The nether_earth project is the integration test. If it stalls, batty has a bug.
- The shim is the agent runtime. Each agent runs inside a dedicated shim process.
- Compose, don't monolith. shim + kanban-md + BYO agents.
- Markdown as backend. Files in, files out, git tracks everything.

---

## Current Phase: Continuous Stabilization (Active)

**Goal:** Make batty a factory that runs indefinitely. Every failure mode discovered during nether_earth runs must be detected, logged, and auto-recovered.

### Hourly Health Check Protocol

The architect receives a periodic nudge to:
1. Inspect nether_earth daemon logs for warnings/errors
2. Check Grafana dashboards for anomalies (stalls, crash spikes, zero-activity periods)
3. Review board state (orphaned tasks, stuck dispatches, empty boards)
4. Identify any new failure patterns
5. Fix the root cause in batty code if found
6. Restart nether_earth with the fix and verify recovery

### Known Failure Modes (Fixed)

These were all discovered and fixed during the nether_earth stabilization session:

| Failure Mode | Root Cause | Fix |
|---|---|---|
| Permanent "working" state stall | Shim classifier missed idle prompt behind "esc to interrupt" footer | Classifier rewrite: status bar is sole signal |
| 10-hour deadlock | `mark_member_working` overrode shim state | No-op for shim agents; shim is source of truth |
| Pipeline starvation suppressed | Manager "working" permanently blocked detection | Time-bounded suppression (10min grace) |
| Messages stuck in pending queue | No expiry mechanism | Pending queue expiry with digest collapsing |
| Completion rejection loop | Codex agents don't commit | 3-strike auto-reset + RestartAgent + CLAUDE.md commit discipline |
| Worktree stuck on old branch | Dispatch didn't auto-reset | Auto-reset in dispatch queue on readiness failure |
| Merge conflict permanent stall | No merge conflict detection | Auto-recovery: merge --abort + reset to base |
| Orphaned review tasks | WIP reconciliation left tasks in review with no owner | Orphan rescue for review AND in-progress tasks |
| Manager not merging | Review intervention didn't fire after restart | Removed idle_epoch gate; fires immediately |
| Agent process dies inside shim | Shim exits but daemon doesn't detect EOF | kill(pid, 0) check on channel EOF |
| Orphan shim processes accumulate | Old shims not killed on restart | kill_orphan_shims with parent PID check |
| Dispatch→reconcile infinite loop | active_tasks cleared immediately for todo tasks | 60-second grace period before clearance |
| Task body dependencies ignored | Only frontmatter depends_on checked | Body "Blocked on:" parser skips tasks with unmet deps |
| "Pasted text" not submitted | Character-by-character injection lost Enter | Bracketed paste mode for all TUI agents |
| Narrow pane misclassification | "esc to interrupt" truncated to "esc t…" | Match truncated prefixes |
| Tool execution misclassified as idle | "ctrl+b to run in background" not matched | Added to classifier working signals |
| PTY reader blocks on idle agent | Classifier only ran on PTY output | Periodic 5-second screen poll thread |
| WIP violation via manager bypass | `batty assign` didn't check WIP | Dual check: active_tasks + board state |
| Dashboard shows wrong year | Timestamps multiplied by 1000 twice | Use raw unix seconds |
| 8-hour gap invisible in graphs | Only hours with events plotted | Recursive CTE fills every calendar hour with zeros |

### Remaining Known Issues

| Issue | Status | Priority |
|---|---|---|
| Verification loop is still open after agent completion | Need daemon-run test/fix/retry cycle before merge | Critical |
| `cargo test` on `main` is still red because tmux runtime tests assume a working live session environment | Need tmux command/session bootstrap hardening or correct capability gating in `src/tmux.rs` so default verification is green again | Critical |
| Architect and manager stalls are less visible than engineer stalls | Non-engineer shim stall detection still needs hardening | Critical |
| Manager inbox noise buries actionable work | Needs batching and signal-first routing | Critical |
| Auto-merge path is not yet proven end-to-end | Review handoff and merge trigger need production verification | Critical |
| Codex agents still degrade before hitting hard context limits | RestartAgent exists; proactive restart with handoff remains open | Medium |
| Architect backlog creation can emit malformed duplicate tasks | Need creation-time validation plus duplicate-title suppression so replenishment never pastes raw logs into backlog cards (for example `#522`/`#523`) | High |

### Tact Engine Status

Daemon-driven board replenishment is now in place. The tact engine detects idle-worker starvation, composes a structured planning prompt from roadmap and board state, routes it to the architect, and creates board tasks automatically.

Next hardening work is about execution quality rather than backlog creation:
- Close the completion loop with automatic test, retry, and escalation behavior
- Verify the auto-merge path end-to-end under production-like runs
- Keep notifications and status chatter out of agent context windows
- Add proactive context-exhaustion restarts with handoff summaries

This removes the old "board empties because nobody creates tasks" failure mode from the active roadmap and shifts attention to verification, merge reliability, and context hygiene.

---

## Historical Phases (Complete)

All phases below are complete and merged. See git history for details.

- **Team Architecture Foundation** — YAML org chart, CLI, tmux layout, daemon, watchers, message routing, Telegram bridge
- **Stabilization** — build fix, docs sync, test coverage, template hardening
- **Autonomous Task Loop** — board-driven dispatch, test gating, progress reporting, failure handling
- **Merge and Ship** — worktree isolation, merge queue, conflict resolution, branch cleanup
- **Workflow Control Plane** — 17 tasks, 12 modules, capability model, state model, orchestrator
- **Intelligence Layer** — standups, retrospectives, failure patterns, team templates
- **Operational Hardening** — tracing, resume, diagnostics, starvation detection
- **Hardened Runtime** — daemon decomposition, typed commands, delivery reliability, board integrity
- **Stability & Predictability** — integration tests, coverage expansion, nudge system, domain errors
- **Production Readiness** — dispatch queue, WIP guard, auto-merge, Telegram reliability
- **Dogfooding Fixes** — active-task reconciliation, non-git support, session cleanup
- **Review Automation** — auto-merge policy, review timeout, structured feedback
- **Error Resilience** — poll loop isolation, unwrap cleanup, sentinel tests
- **Documentation & Hygiene** — intervention docs, README refresh, dead code audit
- **Agent Shim (v0.7.0)** — PTY-per-agent, structured protocol, classifier, lifecycle management

## Future: Goal-Directed TUI (v0.8.0)

Deferred until stability is proven. Console TUI with chat, board, agent peek, dynamic scaling. See `planning/prd-goal-directed-tui.md` for spec.

## Planned: OpenClaw Supervision Rollout

OpenClaw supervision should ship incrementally as a backend + supervised decision path, not as a second runtime. See `planning/openclaw-supervision-rollout.md` for phased scope, migration guidance, rollout checklist, and success metrics.

---

## Tech Stack

| Layer | Choice |
|---|---|
| Core | Rust (clap + tokio) |
| Agent Runtime | Shim (portable-pty + vt100) |
| Config | YAML (team + goal) |
| Tasks | Markdown kanban board |
| Logs | JSON lines |
| Monitoring | Grafana + SQLite |
| Comms | Telegram (optional) |

## Risks

1. **Agent reliability** — coding agents produce inconsistent output. Mitigated by test gating, auto-merge, and completion rejection counter.
2. **Context exhaustion** — long-running agents hit context limits. Mitigated by RestartAgent on 3 rejections.
3. **State desync** — daemon state vs board state drift. Mitigated by reconciliation every poll cycle.
4. **Tact engine complexity** — daemon-driven planning is a new paradigm. Mitigated by starting with simple single-turn prompts.
