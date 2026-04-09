# Daemon Module Map

`src/team/daemon.rs` is the integration layer for the long-running Batty control plane. It owns the `TeamDaemon` state, starts and resumes agents, polls watchers, routes messages, persists runtime state, and sequences all automation from one poll loop.

## Poll Loop Flow

Each daemon iteration runs the same high-level sequence:

1. `poll_watchers()` updates member state, detects pane death and context exhaustion, and hands engineer completions to `merge::handle_engineer_completion`.
1. `retry_failed_deliveries()` retries previously failed live pane injections.
1. Intervention automation runs in order:
   `maybe_intervene_triage_backlog()`,
   `maybe_intervene_review_backlog()`,
   `maybe_intervene_owned_tasks()`,
   `maybe_detect_pipeline_starvation()`,
   `maybe_auto_unblock_blocked_tasks()`,
   `maybe_intervene_manager_dispatch_gap()`,
   `maybe_intervene_architect_utilization()`.
1. `maybe_fire_nudges()` sends idle timeout nudges.
1. `standup::maybe_generate_standup(...)` emits standup reports.
1. `maybe_generate_retrospective()` writes a retrospective when the board reaches done.
1. Runtime state, telemetry, and hot-reload checks are persisted around those steps.

The extracted modules below keep that loop readable without changing the daemon's role as the orchestrator.

## Module Responsibilities

### `src/team/dispatch.rs`

- Responsibility: assignment launch flow and pane working-directory correction.
- Key entrypoints: `TeamDaemon::launch_task_assignment`, `TeamDaemon::ensure_member_pane_cwd`, `engineer_task_branch_name`, `summarize_assignment`.
- Replaced in monolithic daemon: branch/worktree setup, launch-script generation, assignment prompt shaping, and `cd` correction logic that used to sit inline in assignment handling.
- Called from daemon flow: assignment and restart paths, especially when dispatching engineers onto task branches or re-launching after context exhaustion.

### `src/team/delivery.rs`

- Responsibility: live pane injection verification, inbox fallback, failed-delivery retry, and escalation.
- Key entrypoints: `TeamDaemon::retry_failed_deliveries`, `message_delivery_marker`, `capture_contains_message_marker`, `FailedDelivery`, `MessageDelivery`.
- Replaced in monolithic daemon: message routing retry bookkeeping and content-based delivery verification.
- Called from daemon flow: immediately after watcher polling so failed live sends are retried before new interventions or nudges fire.

### `src/team/daemon/interventions.rs`

- Responsibility: idle automation decisions for nudges, triage backlog, review backlog, owned-task escalation, manager dispatch-gap alerts, and architect utilization alerts.
- Key entrypoints: `TeamDaemon::maybe_fire_nudges`, `TeamDaemon::maybe_intervene_triage_backlog`, `TeamDaemon::maybe_intervene_review_backlog`, `TeamDaemon::maybe_intervene_owned_tasks`, `TeamDaemon::maybe_intervene_manager_dispatch_gap`, `TeamDaemon::maybe_intervene_architect_utilization`, `NudgeSchedule`.
- Replaced in monolithic daemon: all intervention threshold, cooldown, dedupe, and idle-grace logic.
- Called from daemon flow: the middle of each poll loop after delivery retry and before standups.

### `src/team/daemon/telemetry.rs`

- Responsibility: event emission and orchestrator log recording that should not clutter business logic.
- Key entrypoints: `append_orchestrator_log_line` plus `TeamDaemon` helpers implemented in this module for event and telemetry writes.
- Replaced in monolithic daemon: scattered event-sink and orchestrator-log append code.
- Called from daemon flow: every lifecycle, intervention, delivery, and retrospective path that records audit data.

### `src/team/launcher.rs`

- Responsibility: agent launch-script generation, launch identity persistence, session-id handling, and Codex/Claude-specific prompt shaping.
- Key entrypoints: `write_launch_script`, `strip_nudge_section`, `canonical_agent_name`, `new_member_session_id`, `load_launch_state`, `duplicate_claude_session_ids`, `member_session_tracker_config`.
- Replaced in monolithic daemon: inline shell-script generation and launch-state persistence.
- Called from daemon flow: startup, restart, resume, and task dispatch paths.

### `src/team/merge.rs`

- Responsibility: engineer completion handling, test gating, merge/rebase flow, conflict handling, and worktree reset.
- Key entrypoints: `handle_engineer_completion`, `merge_engineer_branch`, `reset_engineer_worktree`, `MergeLock`, `MergeOutcome`.
- Replaced in monolithic daemon: the entire "engineer says done" control path, including merge failure escalation and post-merge cleanup.
- Called from daemon flow: `poll_watchers()` when an engineer completion event is detected.

### `src/team/daemon/merge_queue.rs`

- Responsibility: serial queue execution for daemon-owned auto-merges after completion handling accepts a task for unattended merge.
- Key entrypoints: `TeamDaemon::process_merge_queue`, `TeamDaemon::enqueue_merge_request`, `TeamDaemon::execute_queued_merge`, `MergeQueueOutcome`.
- Supported outcomes are intentionally heterogeneous per request: `Success`, `Conflict`, `Reverted`, and `Failed`. Operators should evaluate each task independently instead of expecting one queue drain to end in a uniform result.
- Called from daemon flow: each poll loop after completion handling has queued mergeable work.

### `src/team/status.rs`

- Responsibility: runtime/member status synthesis, inbox and triage counts, owned-task summaries, workflow metrics, and pane-label formatting.
- Key entrypoints: `list_runtime_member_statuses`, `build_team_status_rows`, `agent_health_by_member`, `triage_backlog_counts`, `pending_inbox_counts`, `owned_task_buckets`, `board_status_task_queues`, `compute_metrics`, `workflow_metrics_section`, `update_pane_status_labels`.
- Replaced in monolithic daemon: status-report assembly and tmux label formatting code.
- Called from daemon flow: status reporting, standup generation, intervention inputs, and UI label refresh.

### `src/team/task_loop.rs`

- Responsibility: board task selection plus engineer worktree creation, refresh, migration, branch cleanup, and test execution in worktrees.
- Key entrypoints: `next_unclaimed_task`, `run_tests_in_worktree`, `setup_engineer_worktree`, `prepare_engineer_assignment_worktree`, `refresh_engineer_worktree`, `current_worktree_branch`, `branch_is_merged_into`, `delete_branch`.
- Replaced in monolithic daemon: git/worktree lifecycle helpers and backlog-pick logic that were previously mixed into assignment handling.
- Called from daemon flow: dispatch and merge/reset paths that need worktree preparation or branch hygiene.

### `src/team/completion.rs`

- Responsibility: parsing structured completion packets and copying completion metadata into workflow fields.
- Key entrypoints: `parse_completion`, `validate_completion`, `apply_completion_to_metadata`, `ingest_completion_message`.
- Replaced in monolithic daemon: ad hoc parsing of engineer completion payloads.
- Called from daemon flow: message ingestion and workflow metadata updates outside the merge-specific path.

### `src/team/review.rs`

- Responsibility: workflow review-state transitions and merge-disposition validation.
- Key entrypoints: `apply_review`, `validate_review_readiness`, `MergeDisposition`, `ReviewState`.
- Replaced in monolithic daemon: review outcome branching logic.
- Called from daemon flow: review commands and workflow-first task transitions rather than the main poll loop itself.

## Supporting Modules the Daemon Depends On

### `src/team/config.rs`

- Responsibility: team topology, automation settings, workflow policy, and role-level configuration parsing.
- Used by daemon: startup configuration, automation intervals, cooldowns, and member capabilities.

### `src/team/workflow.rs`

- Responsibility: workflow metadata model and legal state transitions.
- Used by daemon: board/workflow-aware automation, completion ingestion, and review handling.

### `src/team/nudge.rs`

- Responsibility: workflow-aware nudge target selection based on runnable work, review queues, and ownership.
- Used by daemon: intervention and idle-automation decisions.

### `src/team/delivery.rs`, `src/team/dispatch.rs`, `src/team/merge.rs`, `src/team/status.rs`, and `src/team/task_loop.rs`

- These are the primary extracted subsystems that replaced large inline sections of the original monolithic daemon and now define the daemon's boundary with delivery, assignment, merge, status, and worktree management.

## What Still Lives in `daemon.rs`

- `TeamDaemon` state ownership and lifecycle.
- Startup and resume wiring.
- Watcher polling and state transitions.
- Runtime persistence and hot-reload coordination.
- Integration sequencing across extracted modules.

That split keeps the daemon as the control-plane shell while pushing focused policy and implementation details into modules that are easier to test and reason about.
