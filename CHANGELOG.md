# Changelog

All notable changes to Batty are documented here.

## 0.8.0 — 2026-04-05

Agent health and dispatch reliability improvements, discovered during a
24-hour marketing team run where one agent was silently dead for 22 hours.

### Fixes

- **Fix `preserve_working` state desync after daemon restart** — when a
  shim sends `Event::Ready` after respawn, only the shim handle's own
  state is used to decide whether to preserve Working. Previously the
  persisted daemon state (`self.states`) was also checked, causing freshly
  spawned agents to get permanently stuck as Working after a daemon restart.
  This was the root cause of priya-writer-1-1 being dead for 22+ hours.
- **Dispatch queue prunes stale entries regardless of engineer state** —
  `process_dispatch_queue()` now checks task validity (done/claimed/missing)
  before checking if the engineer is idle. Previously, entries for non-idle
  engineers were retained forever even when the underlying task was already
  completed by another engineer.
- **Zero-output agent detection and auto-restart** — agents with 0 output
  bytes after 10 minutes of uptime are now detected and cold-respawned.
  The health system previously had context *pressure* detection (too much
  output) and stall detection (no output *change*), but nothing to catch
  agents that never produced any output at all.

## 0.7.3 — 2026-04-04

Patch release to fix the failed v0.7.2 release workflow (crate already
published when tag was force-updated, causing a duplicate publish attempt).
No code changes — identical to v0.7.2.

## 0.7.2 — 2026-04-02

SDK communication modes for all three agent backends, replacing PTY
screen-scraping as the primary agent I/O mechanism. Each backend now
communicates via its native structured protocol when `use_sdk_mode: true`
(the default).

### Features

- **Claude Code SDK mode** — stream-json NDJSON protocol on stdin/stdout
  (`claude -p --input-format=stream-json --output-format=stream-json`).
  Persistent subprocess with auto-approval of tool use, structured
  completion detection, and context exhaustion handling.
- **Codex CLI SDK mode** — JSONL spawn-per-message model (`codex exec
  --json`). Each message spawns a new subprocess; multi-turn context
  preserved via thread ID resume.
- **Kiro CLI ACP SDK mode** — Agent Client Protocol (ACP) JSON-RPC 2.0
  on stdin/stdout (`kiro-cli acp --trust-all-tools`). Initialization
  handshake (`initialize` + `session/new`), streaming via
  `session/update` notifications, permission auto-approval via
  `session/request_permission`, and session resume via `session/load`.
- **`use_sdk_mode: true` default** — all three backends default to
  structured JSON protocols. PTY screen-scraping remains as fallback.
- **`batty chat --sdk-mode`** — test SDK mode interactively for any
  agent type.

### Stability

- Context pressure tracking with proactive warnings
- Narration loop detection for agents stuck in output cycles
- Stale Codex resume degrades to cold respawn instead of hanging
- Crash auto-respawn defaults to on for unattended teams
- Tact planning engine with harness tests
- Comprehensive stall prevention and system stabilization
- Dynamic scaling via `batty scale` commands
- Daemon config hot-reload

### Fixes

- Dispatch queue retry loop and shim warning noise
- Poll_shim now uses `agent_supports_sdk_mode()` instead of hardcoded
  claude-only checks for SDK mode dispatch
- Clippy warnings resolved for CI compliance (Rust 1.94)

### Documentation

- README, architecture, getting-started, and config reference updated
  to document SDK modes as the primary agent communication mechanism
- Config reference now includes team.yaml shim settings table

## 0.7.1 — 2026-03-26

Patch release focused on shim hardening and live-runtime defaults.

- **Kiro shim delivery/completion hardening** — Kiro now uses `kiro-cli`
  consistently, sends input via bracketed paste, and waits for a stable idle
  screen before emitting `Completion`, fixing truncated multi-line responses in
  `batty chat` and live-agent shim tests.
- **Live Kiro validation** — `cargo test --features live-agent live_kiro`
  passes against the real CLI after the shim timing fixes.
- **Team runtime defaults updated** — the Batty project team config now uses
  `codex` for architect, manager, and engineer roles with `use_shim: true`,
  aligning the live system with the shim-first runtime migration.

## 0.7.0 — 2026-03-24

Architecture release replacing tmux-direct agent management with a
process-per-agent shim runtime. Every agent now runs inside its own PTY-owning
subprocess (`batty shim`), communicates over a typed socketpair protocol, and
uses a vt100 virtual screen for sub-second state classification. Tmux becomes a
display-only surface. 33 shim-related commits, 5,120 lines of new shim code,
2,421 tests passing.

### Agent Shim Architecture

- **`batty shim` subcommand** — standalone agent container process that owns a
  PTY, runs a vt100 virtual terminal, and communicates with the daemon over a
  Unix socketpair using newline-delimited JSON.
- **Typed socketpair protocol** — 7 Commands (`SendMessage`, `CaptureScreen`,
  `GetState`, `Resize`, `Shutdown`, `Kill`, `Ping`) and 10 Events (`Ready`,
  `StateChanged`, `Completion`, `Died`, `ContextExhausted`, `ScreenCapture`,
  `State`, `Pong`, `Warning`, `Error`). Fully serializable with serde.
- **Screen classifiers** — per-backend state classification from vt100 screen
  content: `classify_claude`, `classify_codex`, `classify_kiro`,
  `classify_generic`. Detects Idle, Working, Prompting, and Error states
  without polling tmux.
- **PTY log writer** — agent PTY output forwarded to log files and piped into
  tmux panes via `tail -f`, making tmux a read-only display layer.
- **`AgentHandle` abstraction** — daemon manages agents through handles backed
  by socketpair file descriptors, replacing direct tmux pane manipulation.

### JSONL Session Tracking

- **Claude tracker** — parses Claude's `~/.claude/projects/` session JSONL for
  conversation turns, token usage, and tool calls. Merge priority: screen
  classification wins over tracker data.
- **Codex tracker** — parses Codex session output for task progress. Merge
  priority: tracker data wins over screen classification.
- **Tracker-classifier fusion** — combined signal improves state accuracy,
  especially for detecting context exhaustion and stalled agents.

### Chat Frontend

- **`batty chat` command** — interactive shim frontend for manual agent
  interaction. Connects to a running shim's socketpair and renders state
  changes, completions, and screen captures in the terminal.

### Agent Lifecycle

- **Crash recovery** — shim detects agent process death, emits `Died` event
  with exit code and last terminal lines, enabling daemon-side restart.
- **Context exhaustion detection** — classifiers recognize context-limit
  signals per backend; shim emits `ContextExhausted` event for automatic
  session rotation.
- **Graceful shutdown** — `Shutdown` command with configurable timeout allows
  agents to finish current work before termination. `Kill` command for
  immediate termination.
- **Ping/Pong health monitoring** — daemon sends periodic `Ping` commands;
  shim responds with `Pong`. Missed pongs trigger stall warnings and
  eventual restart.

### Message Queuing

- **Shim-side message queue** — messages arriving while the agent is in
  Working state are buffered (depth 16, FIFO). Queue drains automatically
  when the agent transitions to Idle. Oldest messages dropped when queue is
  full, with tracing warnings.

### Daemon Integration

- **Shim-based agent spawning** — daemon launches agents as `batty shim`
  subprocesses connected via socketpair, replacing tmux `send-keys` injection.
- **Event-driven polling** — daemon reads shim events from socketpair file
  descriptors instead of polling tmux pane content on 5-second cycles.
- **`use_shim` config flag** — opt-in migration path in team.yaml; legacy
  tmux-direct path removed after full migration.

### Doctor

- **Shim health checks** — `batty doctor` validates shim process liveness,
  socketpair connectivity, and PTY state for all running agents.

### Legacy Removal

- **Removed tmux-direct agent management** — `inject_message`,
  `inject_standup`, `poll_watchers`, `restart_dead_members`, and
  `reset_context_keys` deleted from daemon and delivery modules.
- **Removed `AgentAdapter` tmux methods** — `reset_context_keys` removed
  from the backend trait interface.
- **Net code reduction** — legacy tmux agent management code removed,
  offset by new shim modules.

### Performance

- **Sub-second state detection** — vt100 screen classification runs on every
  PTY write, replacing the previous 5-second tmux capture-pane polling cycle.
- **Debounce tuning** — classifier debounce prevents spurious state
  transitions during rapid terminal output. Benchmarks added for
  classification throughput.

### Testing

- **E2E shim validation suite** — integration tests exercising the full
  shim lifecycle: spawn, classify, deliver, complete, shutdown.
- **Shim delivery routing tests** — verify message delivery through
  socketpair protocol end-to-end.
- **Performance benchmarks** — classification throughput benchmarks in
  `src/shim/bench.rs`.
- **2,421 unit tests passing** — up from 2,381 in v0.6.0.

### Documentation

- **CLI reference updated** — `batty shim` and `batty chat` subcommands
  documented.
- **Config reference updated** — shim lifecycle config fields
  (`use_shim`, `shim_ping_interval_secs`, `shim_stall_threshold_secs`)
  documented.
- **Getting-started guide refreshed** — updated for shim-based workflow.
- **Agent shim spec and v0.7.0 roadmap** — design spec and POC published
  in `planning/`.

---

## 0.6.0 — 2026-03-23

Major release adding Grafana monitoring, agent backend abstraction,
SQLite telemetry migration, and a large-scale codebase decomposition.
38 commits since v0.5.2.

### Features

- **Grafana monitoring integration** (#306) — new `batty grafana` CLI with
  `setup`, `status`, and `open` subcommands. Bundled dashboard template with
  21 panels and 6 alerts covering task throughput, agent health, cycle time,
  and failure rates. Auto-registers datasource on `batty start`/`stop`.
  Configurable via `GrafanaConfig` in team.yaml.
- **Agent backend abstraction** — `AgentAdapter` trait enables mixed-backend
  teams (Claude, Codex, Kiro). Per-role and per-instance `agent` config in
  team.yaml. `BackendRegistry` discovers and validates available backends.
  `BackendHealth` enum tracks per-backend liveness.
- **Backend health checks in validate** (#325) — `batty validate` now probes
  each configured backend for reachability and reports health status.
- **`batty init --agent`** (#303) — set the default agent backend when
  scaffolding a new project. Also available via the `install` alias.
- **Shell completion coverage** (#330) — verified and tested completions for
  all current commands across bash, zsh, and fish shells.

### Telemetry

- **SQLite telemetry migration** (#316) — `batty retro` and `batty status`
  now query `telemetry.db` first with automatic JSONL fallback. Review
  metrics (#315) also migrated to SQLite.

### Codebase Health

- **Module decomposition** — 8 large modules split into focused submodules:
  `health.rs` (6 submodules), `daemon.rs` (6 submodules), `config.rs`,
  `delivery.rs`, `doctor.rs` (4 submodules), `watcher.rs`, `merge.rs`
  (4 submodules), and `team/mod.rs` (extracted `init.rs`, `load.rs`,
  `messaging.rs`, `lifecycle.rs`).
- **Error resilience sentinel tests** (#308, #311) — dedicated tests
  confirming `daemon.rs` and `task_loop.rs` handle error paths without panics.
- **Dead code audit** (#309) — removed 28 stale `#[allow(dead_code)]`
  annotations.
- **MockBackend for testing** (#325) — `MockBackend` implements
  `AgentAdapter`, enabling 18 trait contract tests without real backend
  dependencies.

### Documentation

- **Grafana getting-started walkthrough** (#328) — step-by-step guide for
  setting up monitoring with Grafana and the bundled dashboard.
- **Agent Backend Abstraction docs** — architecture.md updated with backend
  trait design, registry, and mixed-team configuration.
- **README and getting-started refresh** — updated for v0.5.x and v0.6.0
  features, CLI reference regenerated.

---

## 0.5.2 — 2026-03-23

Patch release adding crates.io publishing and Enter key delivery fix.

### Reliability

- **Enter key reliability** (#302) — paste verification + retry in `inject_message()`. Messages now reliably submit after injection instead of sitting idle in the pane.

### Infrastructure

- **crates.io publishing** — `cargo install batty-cli` now installs the latest release from crates.io. Release workflow publishes automatically on tag push.

---

## 0.5.1 — 2026-03-22

Patch release with developer experience improvements and delivery reliability fix.

### Features

- **Daemon auto-archive** (#298) — done tasks older than `archive_after_secs` (default: 3600) are automatically moved to archive by the daemon.
- **Checkpoint wiring for restart** (#299) — agent restart resume prompts now include `.batty/progress/<role>.md` checkpoint content.
- **Inbox purge** (#300) — `batty inbox purge <role>` deletes delivered messages. Supports `--older-than` for selective cleanup.
- **Telemetry dashboard** (#301) — `batty metrics` shows tasks completed, avg cycle time, failure rate, merge rate from the telemetry DB.

### Reliability

- **Delivery marker scrolloff fix** (#296) — infer successful delivery from agent state transition when the marker scrolls past the capture window. Eliminates ~80% false-positive delivery failures.
- **Starvation detection false positive fix** (#286) — suppress alerts when all engineers have active board tasks.
- **Config validation improvements** (#291) — better error messages for common team.yaml mistakes.

### Maintenance

- **Makefile targets** (#294) — `make test`, `make coverage`, `make release` match CI behavior.
- **Markdown lint compliance** (#293) — all docs pass markdownlint.
- **CI skip list stabilization** — skip timing-sensitive and environment-dependent tests in CI.

---

## 0.5.0 — 2026-03-22

Feature release adding board archival, delivery reliability, worktree
intelligence, telemetry completeness, and session summary. 13 commits
since v0.4.1.

### Features

- **Board archive command** (#277) — `batty board archive` moves completed
  tasks older than a configurable threshold (`--older-than 7d`) out of the
  active board. Supports `--dry-run` for safe previewing.
- **Delivery readiness gate** (#276) — messages sent to agents still starting
  up are buffered in a pending queue instead of being dropped. Messages drain
  automatically once the agent reaches Ready state.
- **Cherry-pick worktree reconciliation** (#278) — detects when all commits on
  a task branch have been cherry-picked onto main and auto-resets the worktree,
  preventing stale-branch accumulation.
- **Agent metrics telemetry wiring** (#275) — `delivery_failed` and
  `context_exhausted` events now correctly increment failure and restart
  counters in the `agent_metrics` SQLite table.
- **Session summary on stop** — `batty stop` now prints run statistics
  (duration, tasks completed, messages routed) when ending a session.

### Reliability

- **Error handling tests** (#279) — additional tests for `error_handling.rs`
  covering telemetry split edge cases.
- **Clippy cleanup** (#282) — zero warnings on `cargo clippy --all-targets`.

### Documentation

- **Intervention system docs** (#283) — complete documentation of the
  intervention subsystem (health checks, nudges, escalation, auto-restart).
- **README and getting-started refresh** — updated for post-v0.4.1 features.

### Maintenance

- **Dependency updates** (#273) — toml 0.8→1.0, cron 0.13→0.15,
  rusqlite 0.32→0.39.
- **Property-based tests** (#270) — 16 proptest-driven config parsing tests
  for fuzz-level confidence in YAML deserialization.
- **Board archive integration tests** — helpers for testing archive workflows
  end-to-end.

## 0.4.1 — 2026-03-22

Stability patch focused on test coverage expansion and reliability. 664 new
tests added across 4 waves, bringing the suite from ~1,285 to 1,949 tests.
Zero new features — pure quality investment.

### Test Infrastructure

- **Unit/integration test split** (#251) — tests categorized with a Cargo
  feature gate (`--features integration`). Unit tests run without tmux; 56
  integration tests require a running tmux server and are auto-skipped in CI.
- **Flaky test stabilization** (#250) — timing-dependent tmux tests converted
  to retry/poll patterns, eliminating intermittent CI failures.

### Coverage Expansion — Wave 1

- **daemon/automation.rs + cost.rs** (#254) — 78 new tests covering automation
  rules and cost calculation edge cases.
- **daemon/health.rs** (#256) — 24 tests covering health check scheduling and
  state transitions.

### Coverage Expansion — Wave 2

- **board_cmd, resolver, workflow, nudge** (#260) — 59 tests across 4 board
  and workflow modules.
- **daemon interventions** (#253) — 72 tests covering all 6 intervention
  subsystem submodules.
- **delivery.rs** (#258) — 43 tests for message delivery, circuit breaker, and
  Telegram retry logic.
- **standup.rs + retrospective.rs** (#259) — 57 tests for periodic summary
  generation and retrospective reports.
- **layout.rs + telegram_bridge.rs** (#255) — 35 tests for tmux layout
  building and Telegram bridge communication.
- **Cross-module behavioral verification** (#257) — 28 tests validating
  interactions across module boundaries.

### Coverage Expansion — Wave 3

- **tmux.rs** (#262) — 42 tests for core tmux runtime infrastructure (pane
  ops, session management, output capture).
- **task_loop.rs + message.rs** (#263) — 36 tests for the autonomous dispatch
  loop and message routing types.
- **capability.rs + policy.rs** (#261) — 33 tests for topology-independent
  capabilities and config-driven workflow policies.

### Coverage Expansion — Wave 4

- **Config validation edge cases** (#264) — 43 tests for YAML config parsing
  boundaries, invalid inputs, and default handling.
- **Error path and recovery** (#265) — 76 tests exercising error propagation,
  fallback behavior, and graceful degradation paths.
- **CLI argument parsing** (#266) — 38 tests verifying all subcommands parse
  correctly with valid and invalid argument combinations.

## 0.4.0 — 2026-03-22

Major release introducing agent backend abstraction, backend health monitoring,
session resilience features, telemetry infrastructure, and significant internal
decomposition. 39 commits across 20+ tasks since v0.3.2.

### Agent Backend Abstraction

- **AgentAdapter trait** (#230) — unified `launch()`, `session()`, and `resume()`
  behind a single trait, replacing scattered per-backend dispatch logic.
- **Mixed-backend teams** (#231) — team-level `agent_default` config allows
  heterogeneous teams where individual roles can override the team default backend.
- **Backend health monitoring** (#232) — `BackendHealth` enum and `health_check()`
  trait method detect backend failures; health status surfaces in `batty status`,
  daemon polling, and periodic standups.

### Session Resilience

- **Agent stall detection and auto-restart** (#235) — watcher detects
  context-exhausted and stalled agents, triggers automatic restart with backoff.
- **Agent readiness gate** (#233) — prevents message injection into panes that
  haven't finished initializing, eliminating dropped-message failures on startup.
- **Progress checkpoint** (#239) — writes a context file before stall/context
  restart so the restarted agent can resume with prior task context.
- **Daemon restart budget** (#214) — caps total daemon restarts with a rolling
  window, adds exponential backoff, and recovers from pane death gracefully.
- **Commit-before-reset** (#216) — replaces stash-based worktree cleanup with
  auto-commit so engineer work is never silently lost during resets.

### Telemetry

- **SQLite telemetry database** (#220) — persistent storage for agent, task, and
  event metrics with dual-write from the daemon event emitter.
- **`batty telemetry` CLI** — `summary`, `agents`, `tasks`, `events`, and
  `reviews` subcommands surface pipeline metrics from the telemetry DB.
- **DB counter wiring** (#238) — six missing telemetry counters connected to the
  database layer.

### Review Automation

- **Per-priority review timeout overrides** (#218) — configurable timeout
  thresholds per priority level, with YAML parsing and daemon enforcement.
- **Merge confidence scoring** (#221) — risk-based auto-merge gating evaluates
  diff size, module count, sensitive files, and unsafe blocks.
- **Review metrics in retrospectives** (#224) — review stall duration and per-task
  rework counts included in generated retrospective reports.

### Board Tooling

- **Dependency graph** (#236) — `batty board deps` command visualizes task
  dependency relationships.

### Module Decomposition

- **dispatch.rs decomposition** (#234) — split monolithic dispatch module into
  focused submodules under `src/team/dispatch/`.
- **daemon.rs decomposition** (#237) — extracted subsystems from the daemon
  polling loop for maintainability.

### Error Resilience

- **Unwrap cleanup** (#225) — replaced panicking `unwrap()`/`expect()` calls in
  daemon.rs and task_loop.rs with proper `Result` propagation.
- **Dead code audit** (#229) — removed unused code, achieving zero clippy
  warnings across the codebase.

### Workflow Improvements

- **Assignment dedup window** (#213) — prevents duplicate task dispatches within
  a configurable time window.
- **Completion event tracking** (#215) — `task_id` added to `task_completed`
  events and `reason` field added to `task_escalated` events for traceability.

### Documentation

- **README and docs refresh** (#228) — updated README, getting-started guide, CLI
  reference, and config reference for all post-v0.3.0 features.

## 0.3.2 — 2026-03-22

Scheduled tasks, cron recycling, nudge CLI, and intervention module decomposition.

### Scheduled Tasks

- **Task scheduling fields** — `scheduled_for`, `cron_schedule`, and `cron_last_run`
  fields on the Task model enable time-gated and recurring task support.
- **`Task::is_schedule_blocked()` helper** — centralizes future-dated schedule
  check logic, replacing scattered date-parsing code.
- **Schedule-aware resolver and dispatch** — resolver skips tasks with a
  `scheduled_for` in the future; dispatch filtering respects schedule gates.
- **Cron recycler** — daemon poll loop auto-recycles done cron tasks, resetting
  status to todo when the next cron window arrives.
- **`batty task schedule` CLI** — manage task schedules with `--at`, `--cron`,
  and `--clear` flags.

### Nudge CLI

- **`batty nudge` subcommand** — enable, disable, and query status of individual
  intervention types (triage, dispatch, review, utilization, replenish, owned-task).

### Internal Improvements

- **Interventions decomposition** — `interventions.rs` split into 9 focused
  submodules (triage, dispatch, review, utilization, replenishment, owned_tasks,
  telemetry, board_replenishment, mod).
- **Worktree prep guard** — validates engineer worktree health before assignment,
  preventing stale-worktree failures.
- **`utilization_recovery_interval_secs` config** — separate cooldown for
  utilization interventions, independent of general intervention cooldown.

### Documentation

- **README and docs refresh** — scheduled tasks guide, nudge CLI usage, and
  getting-started updates for all v0.3.2 features.

## 0.3.1 — 2026-03-22

Dogfooding-driven fixes, review automation, error resilience, and documentation
refresh. 19 tasks across 4 phases, shipped in a single session.

### Review Automation

- **Auto-merge policy engine** — configurable confidence scoring evaluates diffs
  by size, module count, sensitive file presence, and unsafe blocks. Low-risk
  completions merge without manual review when policy is enabled.
- **Auto-merge daemon integration** — wired into the completion path with
  per-task override support (`batty task auto-merge <id> enable|disable`).
- **Review timeout escalation** — tasks in review beyond a configurable threshold
  trigger nudges to the reviewer, then escalate to architect. Dedup prevents spam.
- **Structured review feedback** — `batty review <id> <disposition> --feedback`
  stores exact rework instructions in task frontmatter and delivers to engineer.
- **Review observability** — queue depth, average latency, auto-merge rate,
  rework rate, nudge/escalation counts surfaced in `batty status`, standups, and
  retrospectives.

### Dogfooding Fixes

- **Active-task reconciliation** — daemon clears stale `active_tasks` entries for
  done/archived/missing tasks, preventing engineers from appearing stuck.
- **Completion rejection recovery** — no-commits rejection now clears the
  assignment and marks engineer idle instead of leaving them in limbo.
- **Pane cwd correction** — retry loop with symlink-safe normalization fixes
  resume-time cwd failures on macOS.
- **Non-git-repo support** — `is_git_repo` detection gates all git operations;
  non-code projects no longer emit spurious warnings.
- **Skip worktree when disabled** — `use_worktrees: false` is respected at every
  call site, eliminating 42+ warnings per session in non-code projects.
- **External message sources** — `external_senders` config allows non-role
  senders (e.g. email-router, slack-bridge) to message any role.
- **Test session cleanup** — RAII `TestSession` guard ensures tmux cleanup on
  panic; `batty doctor --fix` kills orphaned `batty-test-*` sessions.
- **Trivial retrospective suppression** — short runs with zero completions skip
  retro generation (configurable `retro_min_duration_secs`).
- **Post-merge worktree reset** — force-clean uncommitted changes and verify HEAD
  after reset; handles dirty worktrees and detached HEAD.

### Error Resilience

- **Poll loop isolation** — subsystems categorized as critical (delivery,
  dispatch) or recoverable (standup, telegram, retro). Recoverable failures log
  and skip; 3+ consecutive failures escalate. Panic-safe `catch_unwind` wraps
  telegram, standup, and retrospective subsystems.
- **Unwrap/expect sentinel tests** — production code in mod.rs, events.rs,
  watcher.rs, inbox.rs, and merge.rs verified free of unwrap/expect calls.

### Documentation & Hygiene

- **Intervention system docs** — comprehensive documentation of all intervention
  types with triggers, state machines, cooldown behavior, and config tables.
- **Docs refresh** — README, getting-started, CLI reference, and config reference
  updated for all post-v0.3.0 features.

## 0.2.0 — 2026-03-18

This release expands Batty's runtime controls and makes long-running team
sessions easier to observe, pause, resume, and recover without losing routing
state.

### Highlights

- **Operational control commands** — add `batty pause` / `batty resume` to
  suppress nudges and standups during manual intervention, plus `batty load` to
  report historical worker utilization from recorded team events.
- **Richer runtime visibility** — `batty status` now reports live worker
  states, and the daemon emits heartbeat, shutdown, loop-step, and panic
  diagnostics for post-run inspection.
- **More reliable message delivery** — after tmux injection, Batty now verifies
  that the target pane actually left the prompt and retries Enter when terminal
  timing drops the keypress.
- **Safer resume behavior** — daemon state now persists across heartbeats so
  restored sessions can recover activity, and Claude watchers can rebind cleanly
  after manual resumes.

### Reliability

- Improve assignment delivery, engineer branch handling, idle detection, and
  completion event restoration across the team runtime.
- Harden daemon error handling and simplify runtime state tracking so nudges,
  watchers, and inbox delivery stay consistent through failures and resumes.
- Fix Claude-specific watcher edge cases, including explicit session binding,
  truncated interrupt footers, resumed watcher visibility, and pause timer
  behavior.
- Resolve unique role aliases to concrete member instances and fix agent
  wrappers to use the installed `batty` binary instead of debug test binaries.
- Add an `auto_dispatch` team configuration toggle so dispatch polling can be
  disabled when a board should be driven manually.

### Documentation

- Tighten onboarding guidance in the README and getting started docs, refresh
  generated CLI/config references, and publish the demo video page with YouTube
  links.

## 0.1.5 — 2026-03-11

Follow-up release to finish the `0.1.4` stabilization work and restore a fully
green delivery pipeline.

### Fixes

- **Patch coverage on inline Rust tests** — update the CI coverage job to run
  `cargo tarpaulin --include-tests` so Codecov measures `#[cfg(test)]` modules
  inside `src/` correctly, including the Ubuntu layout regression test added in
  `0.1.4`.
- **Cross-platform layout test stability** — keep the Linux-compatible tmux
  layout assertion that tolerates the small pane-height rounding difference seen
  on Ubuntu runners once borders and status lines are enabled.

## 0.1.4 — 2026-03-11

Patch release to finish the CI stabilization work from `0.1.3`.

### Fixes

- **Linux tmux compatibility** — switch percentage-based pane splits to the
  portable `split-window -l <pct>%` form so layout tests pass on Ubuntu tmux as
  well as macOS.
- **Green cross-platform CI** — fixes the last failing `cargo test` path in the
  Ubuntu GitHub Actions job without weakening the test matrix.

## 0.1.3 — 2026-03-11

This release stabilizes the team-based Batty runtime and restores a clean
release pipeline. It folds in the hierarchical team architecture work that
landed after `v0.1.2`, plus the CI/CD fixes needed to ship it reliably.

### Highlights

- **Team-based runtime** — Batty now runs hierarchical architect, manager, and
  engineer teams instead of the earlier phase-oriented model.
- **Autonomous dispatch loop** — idle engineers can pick work from the shared
  board automatically, with active-task tracking, retry counting, and
  completion/escalation rollups in the daemon.
- **Human channel support** — Telegram-backed user roles, inbound polling, long
  message splitting, and session resume support are now built into team
  communication.
- **Manager-aware layout** — engineer panes are grouped by manager, routing
  honors compatible `talks_to` targets, and Codex roles get per-member context
  overlays for cleaner startup state.

### Reliability

- Refresh engineer worktrees before assignment and reset them after merge.
- Gate engineer completion on worktree test runs before reporting success.
- Serialize merges behind a rebase-aware merge queue to reduce conflicting
  branch integration.
- Fix Codex watcher handling so stable prompts return to idle and historical
  completions do not leak into new sessions.
- Preserve assignment sender identity for routing checks and fix manager status
  updates during completion handoff.
- Correct tmux pane stacking for vertical splits and improve manager subgroup
  layout behavior.

### Documentation

- Rewrite the README for 60-second onboarding and refresh the session demo.
- Rewrite the getting started guide and regenerate the CLI/config references.
- Refresh architecture and troubleshooting docs for the team-based model.

### CI/CD

- Keep Rust CI strict under `-Dwarnings` by resolving current Clippy findings
  and explicitly marking staged/test-only code paths that are not yet wired
  into the main binary.
- Scope docs lint/format checks to the published MkDocs surface instead of
  archival notes under `docs/new_beginnings/`.
- Regenerate and commit reference docs so the docs workflow remains reproducible.

## 0.1.0 — 2026-02-24

First public release.

### Features

- **Core agent runner** — spawn coding agents (Claude Code, Codex) in supervised tmux sessions
- **Two-tier prompt handling** — Tier 1 regex auto-answers for routine prompts, Tier 2 supervisor agent for unknowns
- **Policy engine** — observe, suggest, act modes controlling how Batty responds to agent prompts
- **Kanban-driven workflow** — reads kanban-md boards, claims tasks, tracks progress through statuses
- **Worktree isolation** — each phase run gets its own git worktree for clean parallel work
- **Test gates** — Definition-of-Done commands must pass before a phase is considered complete
- **Pause/resume** — detach and reattach to running sessions without losing state
- **Parallel execution** — `--parallel N` launches multiple agents with DAG-aware task scheduling
- **Merge queue** — serialized merge with rebase, test gates, and conflict escalation
- **Shell completions** — `batty completions <bash|zsh|fish>`
- **Tmux status bar** — live task progress, agent state, and phase status in the tmux status line

### Bug Fixes

- Fixed CLAUDECODE env var leaking into tmux sessions (blocked nested Claude launches)
- Fixed invalid `--prompt` flag in Claude adapter (now uses positional argument)
- Fixed `batty install` not scaffolding `.batty/config.toml`
- Fixed stale "phase 4 planned" error message in `batty work all --parallel`
- Fixed conflicting claim identities in parallel mode
- Fixed completion contract defaulting to `cargo test` when no DoD configured

### Documentation

- Getting started guide with milestone tag requirement
- Troubleshooting guide with common failure scenarios
- CLI reference (auto-generated)
- Configuration reference
- Architecture overview
- Module documentation
