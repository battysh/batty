# Changelog

All notable changes to Batty are documented here.

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
