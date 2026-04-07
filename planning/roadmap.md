# Batty Roadmap

## North Star

**Reliable autonomous throughput.** Batty should run hierarchical coding teams
for hours without stalling, losing work, or requiring humans to babysit the
board. Stability still comes first, but the bar is now broader: shipped changes
must move cleanly from idea to merged code.

## Principles

- **Stability first.** No feature ships unless the system can run unattended for 8+ hours without intervention.
- **Self-healing by default.** Every failure mode the daemon can detect, it must recover from automatically — orphaned tasks, stuck agents, stale worktrees, dead shims, state desync.
- **Observable.** Every problem must be visible in logs, telemetry, and Grafana dashboards before it becomes a stall.
- **Tested against reality.** The nether_earth project is the integration test. If it stalls, batty has a bug.
- The shim is the agent runtime. Each agent runs inside a dedicated shim process.
- Compose, don't monolith. shim + kanban-md + BYO agents.
- Markdown as backend. Files in, files out, git tracks everything.

---

## Current Phase: Release Hardening And Proof (Active)

**Goal:** Prove the v0.10.0 operating model end-to-end: auto-dispatch,
verification, review routing, auto-merge, and release documentation should all
reflect the same system behavior.

### Shipped In v0.10.0

- Auto-dispatch as the default board posture
- Auto-merge queue for green, low-risk diffs
- Claim TTL reclaim policy
- Per-worktree Cargo target isolation
- Claude stall detection with restart behavior
- Smart worktree recovery and additive-conflict reduction
- OAuth-friendly auth posture for Claude and Codex
- Team scaling and richer role metadata (`posture`, `model_class`)
- Updated release documentation, quick start, and config guidance

### Active Validation Loop

The architect receives a periodic nudge to:
1. Inspect nether_earth daemon logs for warnings/errors
2. Check Grafana dashboards for anomalies (stalls, crash spikes, zero-activity periods)
3. Review board state (orphaned tasks, stuck dispatches, empty boards)
4. Identify any new failure patterns
5. Fix the root cause in batty code if found
6. Restart nether_earth with the fix and verify recovery

### Latest Verification Snapshot

On April 7, 2026, architect verification regained a clean baseline:

- `cargo fmt --check` passed
- `cargo test` passed end-to-end in 931.02s
- the default suite still emitted 4 dead-code warnings in `src/team/tact/parser.rs`
- multiple worktree, merge, and supervision tests exceeded 60 seconds before completing

Later on April 7, 2026, a fresh architect loop found the baseline had regressed:

- `cargo fmt --check` still passed
- `cargo test` failed at compile time in `src/team/delivery/routing.rs` and `src/team/delivery/verification.rs`
- the root tree carried partial supervisory-delivery edits while engineer worktrees remained on older completed-task branches
- board state showed a worker simultaneously claiming multiple in-progress tasks, indicating claim/worktree reconciliation drift still exists under recovery paths

At 04:46 EDT on April 7, 2026, a follow-up architect loop narrowed the current red-main failure:

- `cargo fmt --check` still passed
- `cargo test` failed compiling `src/team/delivery/verification.rs` because a `TeamDaemon` test fixture omitted the newer `discord_bot`, `discord_event_cursor`, and `recent_escalations` fields
- task `#520` was re-queued to `todo`, but the task file initially retained `claimed_by: worker-2`, confirming claim metadata can survive a status rollback unless the board is reconciled explicitly

At 05:05 EDT on April 7, 2026, another architect review loop reproduced the same rollback drift on a different task:

- task `#536` was sent from `review` back to `todo`, but the task file again retained `claimed_by: worker-2` after the rollback
- the stale claim had to be cleared manually, confirming the claim/status reconciliation bug is broader than the earlier `#520` incident

At 05:07 EDT on April 7, 2026, the next root-tree verification found the delivery test fixture drift had widened:

- `cargo fmt --check` still passed
- `cargo test` failed compiling `src/team/delivery/routing.rs` because a `TeamDaemon` test helper there also omitted `discord_bot`, `discord_event_cursor`, and `recent_escalations`
- the same file also constructed `ChannelConfig` without the newer `agents_channel_id`, `commands_channel_id`, and `events_channel_id` fields, so the green-main task now needs to cover both daemon and config test-builder drift

At 05:15 EDT on April 7, 2026, the next architect loop confirmed the red-main break is broader than delivery fixtures alone:

- `cargo fmt --check` still passed
- `cargo test` failed compiling `src/team/daemon/health/poll_shim.rs` because `TeamDaemon` no longer exposes the expected `supervisory_stall_summary` and `handle_supervisory_stall` paths there
- `cargo test` also failed compiling `src/team/openclaw_contract.rs` because that contract still expects a stale `stall_summary` field instead of the current `AgentHealthSummary` shape
- re-queuing stale tasks `#536` and `#540` required `--claim`, and both task files still retained `claimed_by` metadata after the rollback until it was cleared manually, confirming the claim/status reconciliation bug also affects `in-progress -> todo` recovery paths

At 05:23 EDT on April 7, 2026, another architect loop re-verified `main` after the local review commits landed:

- `cargo fmt --check` still passed
- `cargo test` failed compiling `src/team/delivery/verification.rs` because the retry path still calls the removed `record_delivery_failed` helper instead of the newer `record_delivery_failed_with_details` telemetry API
- `cargo test` still failed compiling `src/team/delivery/routing.rs` and `src/team/test_support.rs` because stale `TeamDaemon` and `ChannelConfig` literals remain outside the shared fixture path
- `cargo test` still failed compiling `src/team/status.rs` and `src/team/openclaw_contract.rs` because `AgentHealthSummary` initializers and contract mapping are still split between the older stall-summary shape and the newer supervisory-digest fields
- review tasks `#536` and `#540` were re-queued to `todo`, and `kanban-md move ... todo --claim <worker>` again preserved `claimed_by` metadata until it was cleared manually

At 05:28 EDT on April 7, 2026, the next architect loop re-checked both the repo and the daemon log:

- `cargo fmt --check` still passed
- `cargo test` still failed compiling `src/team/delivery/routing.rs` and `src/team/test_support.rs` on stale `TeamDaemon` / `ChannelConfig` literals, plus `src/team/status.rs` and `src/team/openclaw_contract.rs` on stale `AgentHealthSummary` initializers, confirming `#540` and `#543` remain the active red-main fix lanes
- board state was idle but not empty: no `in-progress` or `review` tasks, with nine existing `todo` tasks before the loop and a new critical `todo` task `#544` added for the supervisory shim disconnect path
- `.batty/daemon.log` showed architect/manager stale-Pong recovery escalating into `auto-restarting stale Claude shim`, followed by multiple shim-side `orchestrator disconnected` / `Broken pipe` lines, indicating the supervisory restart path can still collapse control-plane delivery instead of recovering cleanly

At 05:36 EDT on April 7, 2026, the next architect loop verified that the earlier compile-time break has cleared, but `main` is still red on runtime regressions:

- `cargo fmt --check` still passed
- `cargo test` ran to completion and failed 34 tests in 45.08s; the nested-`tmux` failures still look environment-coupled, but the non-`tmux` failures now center on supervisory restart state, idle/review nudge behavior, and completion rejection bookkeeping
- a targeted rerun of `team::daemon::health::poll_shim::tests::check_working_state_timeouts_restarts_stale_claude_management_agents` still failed because the restarted management agent stayed `Working` when the test expects `Starting`, so supervisory restart-state recovery remains an active lane under task `#544`
- a targeted rerun of `team::merge::completion::tests::narration_only_completion_retries_then_escalates_after_two_rejections` failed with rejection count `left: 2` vs `right: 1` and emitted `Task #42 metadata updated.`, revealing a completion-bookkeeping and test-isolation regression now tracked in task `#545`
- the clean-room disassembly test `team::daemon::tests::clean_room_ghidra_disassembly_supports_multiple_non_z80_targets` passed in isolation, suggesting the clean-room and many later `tmux` fallouts are collateral once earlier panics poison shared test state rather than independent root causes
- a new critical task `#546` now tracks the idle/review nudge regression cluster; board state remained healthy with no `in-progress` or `review` tasks and twelve active `todo` items after the replenishment pass

### Known Failure Modes (Fixed)

These were all discovered and fixed during the nether_earth stabilization session:

| Failure Mode | Root Cause | Fix |
| --- | --- | --- |
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
| --- | --- | --- |
| The earlier April 7, 2026 compile drift is cleared, but `main` is still red on runtime regressions across supervisory restart state, idle/review nudges, and completion rejection bookkeeping (`#544`, `#545`, `#546`) | Active validation | Critical |
| Architect and manager stalls are less visible than engineer stalls | Needs broader non-engineer stall heuristics | Critical |
| Manager inbox noise still buries the most actionable review and dispatch items | Needs batching and signal-first routing | Critical |
| Claim ownership, `active_tasks`, and worktree branch state can still drift after recovery, allowing a worker to appear on multiple in-progress tasks while its worktree stays on an old branch | Needs stronger reconciliation hardening | Critical |
| Supervisory shim restarts can still degrade into `orchestrator disconnected` / `Broken pipe` control-plane loss instead of a clean respawn with pending-work replay | Newly observed on April 7, 2026; tracked in task `#544` | Critical |
| Auto-merge needs more production mileage on heterogeneous diffs | Needs wider dogfooding | High |
| Context exhaustion recovery is reactive; proactive restart/handoff remains open | Planned | Medium |
| Release automation still ends at local verification instead of a fully automated publish flow | Planned | Medium |

### Tact Engine Status

Daemon-driven board replenishment is now in place. The tact engine detects idle-worker starvation, composes a structured planning prompt from roadmap and board state, routes it to the architect, and creates board tasks automatically.

Next hardening work is about execution quality rather than backlog creation:
- Close the completion loop with automatic test, retry, and escalation behavior
- Verify the auto-merge path end-to-end under production-like runs
- Keep notifications and status chatter out of agent context windows
- Add proactive context-exhaustion restarts with handoff summaries

This removes the old "board empties because nobody creates tasks" failure mode from the active roadmap and shifts attention to verification, merge reliability, and context hygiene.

### Next Phase: Context And Supervision Hygiene

After release hardening, the next phase is reducing coordination drag:

- proactive context-budget handoffs before hard exhaustion
- better batching and prioritization for architect/manager inboxes
- stronger supervision visibility for non-engineer roles
- cleaner publish/release automation from green main to tagged release
- tighter GitHub verification feedback wired back into the daemon loop

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
