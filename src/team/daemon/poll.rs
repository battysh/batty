//! Main daemon poll loop — signal handling, subsystem sequencing, heartbeat.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::config_reload::ConfigReloadMonitor;
use super::hot_reload::HotReloadMonitor;
use super::tick_report::TickReport;
use super::{TeamDaemon, standup, status};
use crate::team;
use crate::team::config::RoleType;
use crate::tmux;

impl TeamDaemon {
    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    ///
    /// If `resume` is true, agents are launched with session-resume flags
    /// (`claude --resume <session-id>` / `codex resume --last`) instead of fresh starts.
    pub fn run(&mut self, resume: bool) -> Result<()> {
        self.record_daemon_started();
        let is_hot_reload = self.acknowledge_hot_reload_marker();
        info!(session = %self.config.session, resume, "daemon started");
        self.record_orchestrator_action(format!(
            "runtime: orchestrator started (mode={}, resume={resume})",
            self.config.team_config.workflow_mode.as_str()
        ));

        // Install signal handler so we log clean shutdowns
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = shutdown_flag.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        }) {
            warn!(error = %e, "failed to install signal handler");
        }

        self.run_startup_preflight()?;

        // Spawn agents in all panes
        self.spawn_all_agents(resume)?;
        if resume {
            self.restore_runtime_state();
        }
        // After a hot-reload, agents are freshly spawned and have no memory of
        // their prior tasks. Clear active_tasks so the board becomes the source
        // of truth again; reconcile will not reconstruct in-progress ownership
        // from stale worktree branches.
        if is_hot_reload {
            info!(
                cleared = self.active_tasks.len(),
                "hot-reload: clearing active_tasks to rely on board state after restart"
            );
            self.active_tasks.clear();
        }

        // Reclaim orphaned in-progress tasks right away, instead of waiting
        // for the next auto-doctor sweep (which only runs every 10 poll
        // cycles). After a daemon restart, the board may carry tasks still
        // marked in-progress and claimed by an engineer whose active
        // assignment the daemon no longer tracks; leaving them claimed
        // blocks dispatch until auto-doctor rediscovers them.
        match self.auto_doctor_reset_orphaned_in_progress() {
            Ok(actions) if !actions.is_empty() => {
                info!(
                    reclaimed = actions.len(),
                    "startup: reclaimed orphaned in-progress tasks"
                );
            }
            Ok(_) => {}
            Err(error) => {
                warn!(error = %error, "startup: failed to reclaim orphaned in-progress tasks");
            }
        }

        // #707: resume prompts for engineers still holding an active_task
        // after the restart path above ran. Without this, fresh shim
        // sessions spawned with only the role prompt (SDK mode wires it
        // into --append-system-prompt; user messages arrive via stdin
        // NDJSON) have no stimulus to continue their pre-restart work.
        // Observed 2026-04-17 12:00 UTC post v0.11.45 deploy: alex-dev-1-1
        // (#570) and sam-designer-1-1 (#572) each held pre-restart
        // claims but produced 0 bytes of output for 12+ minutes because
        // neither got any inbox delivery and the dispatcher skips
        // already-claimed tasks.
        if resume && !is_hot_reload {
            self.enqueue_restart_resume_prompts();
        }

        self.persist_runtime_state(false)?;

        let started_at = Instant::now();
        let heartbeat_interval = Duration::from_secs(300); // 5 minutes
        let mut last_heartbeat = Instant::now();
        let mut hot_reload = match HotReloadMonitor::for_current_exe() {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                warn!(error = %error, "failed to initialize daemon hot-reload monitor");
                None
            }
        };
        let config_path = team::team_config_path(&self.config.project_root);
        let mut config_reload = match ConfigReloadMonitor::new(&config_path) {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                warn!(error = %error, "failed to initialize config reload monitor");
                None
            }
        };

        // Main polling loop
        let shutdown_reason;
        loop {
            // Check for signal-based shutdown
            if shutdown_flag.load(std::sync::atomic::Ordering::SeqCst) {
                shutdown_reason = "signal";
                info!("received shutdown signal");
                break;
            }

            if !tmux::session_exists(&self.config.session) {
                shutdown_reason = if tmux::server_running() {
                    "tmux session disappeared"
                } else {
                    "tmux server died"
                };
                info!("tmux session gone, shutting down");
                break;
            }

            // Run one productive iteration of the daemon's work. Hot-reload
            // and heartbeat persistence stay outside of `tick()` because
            // they need run-loop state (the HotReloadMonitor / ConfigReloadMonitor
            // owned by `run()`).
            let _tick_report = self.tick();

            // Hot-reload checks: kept in `run()` because they own the
            // monitor handles. They reuse the same recoverable-step error
            // handling so a panic is logged, not fatal.
            self.run_recoverable_step("maybe_reload_binary", |daemon| {
                daemon.maybe_hot_reload_binary(hot_reload.as_mut())
            });
            self.run_recoverable_step("maybe_reload_config", |daemon| {
                daemon.maybe_hot_reload_config(config_reload.as_mut())
            });

            // Periodic heartbeat
            if last_heartbeat.elapsed() >= heartbeat_interval {
                let uptime = started_at.elapsed().as_secs();
                self.record_daemon_heartbeat(uptime);
                if let Err(error) = self.persist_runtime_state(false) {
                    warn!(error = %error, "failed to persist daemon checkpoint");
                }
                debug!(uptime_secs = uptime, "daemon heartbeat");
                last_heartbeat = Instant::now();
            }

            std::thread::sleep(self.poll_interval);
        }

        // Graceful shutdown of all shim subprocesses
        self.shutdown_all_shims();

        // Save shim state for session resume
        if let Err(error) = self.save_shim_state() {
            warn!(error = %error, "failed to save shim state for resume");
        }

        let uptime = started_at.elapsed().as_secs();
        if let Err(error) = self.persist_runtime_state(true) {
            warn!(error = %error, "failed to persist final daemon checkpoint");
        }
        self.record_daemon_stopped(shutdown_reason, uptime);
        Ok(())
    }

    /// #707: after daemon restart, enqueue a resume prompt for each
    /// engineer still holding an active_task. Without this, engineers
    /// with pre-restart claims spawn into a fresh Claude session with
    /// only the role prompt (wired via `--append-system-prompt` in SDK
    /// mode; user messages arrive via stdin NDJSON) — no stimulus ever
    /// arrives to resume the task because the dispatcher skips
    /// already-claimed tasks and the manager can't know a restart just
    /// happened. The task sits idle indefinitely.
    ///
    /// Uses the same `restart_assignment_message` format used by the
    /// single-member restart path in `restart_member_with_task_context`,
    /// so the engineer gets a consistent "continuing Task #N" brief.
    pub(super) fn enqueue_restart_resume_prompts(&mut self) {
        if self.active_tasks.is_empty() {
            return;
        }

        let board_tasks_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let tasks = match crate::task::load_tasks_from_dir(&board_tasks_dir) {
            Ok(tasks) => tasks,
            Err(error) => {
                warn!(error = %error, "failed to load tasks for restart resume prompts");
                return;
            }
        };

        let active: Vec<(String, u32)> = self
            .active_tasks
            .iter()
            .map(|(engineer, task_id)| (engineer.clone(), *task_id))
            .collect();

        for (engineer, task_id) in active {
            let Some(task) = tasks.iter().find(|task| task.id == task_id) else {
                continue;
            };
            let message = Self::restart_assignment_message(task);
            match self.queue_message("daemon", &engineer, &message) {
                Ok(()) => info!(
                    engineer = %engineer,
                    task_id,
                    "enqueued restart resume prompt for pre-existing active task"
                ),
                Err(error) => warn!(
                    engineer = %engineer,
                    task_id,
                    error = %error,
                    "failed to enqueue restart resume prompt"
                ),
            }
        }
    }

    /// Run one iteration of the daemon's productive work without sleeping
    /// or touching hot-reload state. This is the inner body of `run()`'s
    /// loop, factored out so tests (and a future `batty debug tick`
    /// subcommand) can drive the daemon a single step at a time.
    ///
    /// Returns a [`TickReport`] capturing observable side effects of the
    /// tick. Phase 1 populates `cycle` and `subsystem_errors`; the other
    /// fields are placeholders that the scenario framework fills in by
    /// snapshotting state around the call.
    pub fn tick(&mut self) -> TickReport {
        // Reset per-tick error capture so the returned report only reflects
        // failures that happened during this call.
        self.current_tick_errors.clear();

        self.poll_cycle_count = self.poll_cycle_count.saturating_add(1);

        // -- Recoverable subsystems: log-and-skip with consecutive-failure tracking --
        self.run_recoverable_step("poll_shim_handles", |daemon| daemon.poll_shim_handles());
        self.run_recoverable_step("shim_health_check", |daemon| daemon.shim_health_check());
        self.run_recoverable_step("check_working_state_timeouts", |daemon| {
            daemon.check_working_state_timeouts()
        });
        self.run_recoverable_step("check_narration_loops", |daemon| {
            daemon.check_narration_loops()
        });
        self.run_recoverable_step("sync_launch_state_session_ids", |daemon| {
            daemon.sync_launch_state_session_ids()
        });
        self.run_recoverable_step("drain_legacy_command_queue", |daemon| {
            daemon.drain_legacy_command_queue()
        });

        // -- Critical subsystems: errors logged but no consecutive-failure tracking --
        self.run_loop_step("deliver_inbox_messages", |daemon| {
            daemon.deliver_inbox_messages()
        });
        self.run_loop_step("retry_failed_deliveries", |daemon| {
            daemon.retry_failed_deliveries()
        });
        self.run_recoverable_step("expire_stale_pending_messages", |daemon| {
            daemon.expire_stale_pending_messages()
        });
        self.run_recoverable_step("maybe_sweep_tiered_inboxes", |daemon| {
            daemon.maybe_sweep_tiered_inboxes();
            Ok(())
        });
        self.run_recoverable_step("check_github_verification_feedback", |daemon| {
            daemon.check_github_verification_feedback()
        });

        // -- Recoverable subsystems --
        self.run_recoverable_step("maybe_intervene_triage_backlog", |daemon| {
            daemon.maybe_intervene_triage_backlog()
        });
        self.run_recoverable_step("maybe_intervene_owned_tasks", |daemon| {
            daemon.maybe_intervene_owned_tasks()
        });
        self.run_recoverable_step("maybe_intervene_review_backlog", |daemon| {
            daemon.maybe_intervene_review_backlog()
        });
        self.run_recoverable_step("maybe_escalate_stale_reviews", |daemon| {
            daemon.maybe_escalate_stale_reviews()
        });
        self.run_recoverable_step("maybe_emit_task_aging_alerts", |daemon| {
            daemon.maybe_emit_task_aging_alerts()
        });
        self.run_recoverable_step("maybe_auto_unblock_blocked_tasks", |daemon| {
            daemon.maybe_auto_unblock_blocked_tasks()
        });
        self.run_recoverable_step("process_merge_queue", |daemon| daemon.process_merge_queue());

        // -- Critical subsystems --
        self.run_loop_step("reconcile_active_tasks", |daemon| {
            daemon.reconcile_active_tasks()
        });
        self.run_loop_step("maybe_manage_task_claim_ttls", |daemon| {
            daemon.maybe_manage_task_claim_ttls()
        });
        self.run_recoverable_step("maybe_run_main_smoke", |daemon| {
            daemon.maybe_run_main_smoke()
        });
        self.run_loop_step("maybe_auto_dispatch", |daemon| daemon.maybe_auto_dispatch());
        self.run_recoverable_step("maybe_recycle_cron_tasks", |daemon| {
            daemon.maybe_recycle_cron_tasks()
        });

        // -- Recoverable subsystems --
        self.run_recoverable_step("maybe_intervene_manager_dispatch_gap", |daemon| {
            daemon.maybe_intervene_manager_dispatch_gap()
        });
        self.run_recoverable_step("maybe_intervene_architect_utilization", |daemon| {
            daemon.maybe_intervene_architect_utilization()
        });
        self.run_recoverable_step("maybe_intervene_board_replenishment", |daemon| {
            daemon.maybe_intervene_board_replenishment()
        });
        self.run_recoverable_step("maybe_detect_pipeline_starvation", |daemon| {
            daemon.maybe_detect_pipeline_starvation()
        });
        self.run_recoverable_step("tact_check", |daemon| daemon.tact_check());

        // -- Recoverable with catch_unwind (panic-safe) --
        self.run_optional_subsystem_step("process_discord_queue", "discord", |daemon| {
            daemon.process_discord_queue()
        });
        self.run_optional_subsystem_step("process_telegram_queue", "telegram", |daemon| {
            daemon.process_telegram_queue()
        });
        self.run_recoverable_step("maybe_fire_nudges", |daemon| daemon.maybe_fire_nudges());
        self.run_recoverable_step("check_backend_health", |daemon| {
            daemon.check_backend_health()
        });
        self.run_recoverable_step("maybe_check_binary_freshness", |daemon| {
            daemon.maybe_check_binary_freshness();
            Ok(())
        });
        self.run_recoverable_step("maybe_reconcile_stale_worktrees", |daemon| {
            daemon.maybe_reconcile_stale_worktrees()
        });
        self.run_recoverable_step("check_worktree_staleness", |daemon| {
            daemon.check_worktree_staleness()
        });
        self.run_recoverable_step("maybe_warn_uncommitted_work", |daemon| {
            daemon.maybe_warn_uncommitted_work()
        });
        self.run_recoverable_step("maybe_cleanup_shared_cargo_target", |daemon| {
            daemon.maybe_cleanup_shared_cargo_target()
        });
        self.run_recoverable_step("maybe_run_disk_hygiene", |daemon| {
            daemon.maybe_run_disk_hygiene()
        });
        self.run_recoverable_step("record_parity_snapshot", |daemon| {
            if daemon.config.team_config.automation.clean_room_mode {
                daemon.sync_cleanroom_specs()?;
                if let Ok(report) =
                    crate::team::parity::ParityReport::load(&daemon.config.project_root)
                {
                    daemon.record_parity_updated(&report.summary());
                }
                crate::team::parity::sync_gap_tasks(&daemon.config.project_root)?;
            }
            Ok(())
        });
        self.run_optional_subsystem_step("maybe_generate_standup", "standup", |daemon| {
            let generated = standup::maybe_generate_standup(standup::StandupGenerationContext {
                project_root: &daemon.config.project_root,
                team_config: &daemon.config.team_config,
                members: &daemon.config.members,
                watchers: &daemon.watchers,
                states: &daemon.states,
                pane_map: &daemon.config.pane_map,
                telegram_bot: daemon.telegram_bot.as_ref(),
                paused_standups: &daemon.paused_standups,
                last_standup: &mut daemon.last_standup,
                backend_health: &daemon.backend_health,
            })?;
            for recipient in generated {
                daemon.record_standup_generated(&recipient);
            }
            Ok(())
        });
        self.run_recoverable_step("maybe_rotate_board", |daemon| daemon.maybe_rotate_board());
        self.run_recoverable_step("maybe_auto_archive", |daemon| daemon.maybe_auto_archive());
        self.run_recoverable_step("run_auto_doctor", |daemon| {
            daemon.run_auto_doctor().map(|_| ())
        });
        self.run_recoverable_step_with_catch_unwind("maybe_generate_retrospective", |daemon| {
            daemon.maybe_generate_retrospective()
        });
        self.run_recoverable_step("maybe_notify_failure_patterns", |daemon| {
            daemon.maybe_notify_failure_patterns()
        });
        status::update_pane_status_labels(status::PaneStatusLabelUpdateContext {
            project_root: &self.config.project_root,
            members: &self.config.members,
            pane_map: &self.config.pane_map,
            states: &self.states,
            nudges: &self.nudges,
            last_standup: &self.last_standup,
            paused_standups: &self.paused_standups,
            standup_interval_for_member: |member_name| {
                standup::standup_interval_for_member_name(
                    &self.config.team_config,
                    &self.config.members,
                    member_name,
                )
            },
        });

        // Drain the per-tick error buffer into a fresh report.
        let mut report = TickReport::new(self.poll_cycle_count);
        report.subsystem_errors = std::mem::take(&mut self.current_tick_errors);
        report
    }

    /// Send Shutdown to all active shim handles, wait for exit, fall back to Kill.
    fn shutdown_all_shims(&mut self) {
        self.warn_members_about_shutdown();

        if self.shim_handles.is_empty() {
            return;
        }

        let warning_secs = self
            .config
            .team_config
            .workflow_policy
            .graceful_shutdown_timeout_secs;
        self.warn_agents_of_shutdown(warning_secs);
        self.preserve_work_before_shutdown();

        let timeout_secs = self.config.team_config.shim_shutdown_timeout_secs;
        info!(
            count = self.shim_handles.len(),
            timeout_secs, "sending graceful shutdown to shim subprocesses"
        );

        // Phase 1: Send Shutdown command to all handles
        let names: Vec<String> = self.shim_handles.keys().cloned().collect();
        for name in &names {
            if let Some(handle) = self.shim_handles.get_mut(name) {
                if handle.is_terminal() {
                    continue;
                }
                if let Err(error) = handle.send_shutdown(timeout_secs) {
                    warn!(
                        member = name.as_str(),
                        error = %error,
                        "failed to send shim shutdown, sending kill"
                    );
                    let _ = handle.send_kill();
                }
            }
        }

        // Phase 2: Wait for child processes to exit within the timeout
        let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
        let mut pids: Vec<(String, u32)> = names
            .iter()
            .filter_map(|name| {
                self.shim_handles
                    .get(name)
                    .filter(|h| !h.is_terminal())
                    .map(|h| (name.clone(), h.child_pid))
            })
            .collect();

        while !pids.is_empty() && Instant::now() < deadline {
            pids.retain(|(name, pid)| {
                // Check if process still alive via kill(0)
                let alive = unsafe { libc::kill(*pid as i32, 0) } == 0;
                if !alive {
                    debug!(member = name.as_str(), pid, "shim process exited cleanly");
                }
                alive
            });
            if !pids.is_empty() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // Phase 3: Force-kill any survivors
        for (name, pid) in &pids {
            warn!(
                member = name.as_str(),
                pid, "shim did not exit within timeout, sending Kill"
            );
            if let Some(handle) = self.shim_handles.get_mut(name) {
                let _ = handle.send_kill();
            }
            // Also send SIGKILL directly
            unsafe {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
        }
    }

    fn warn_agents_of_shutdown(&mut self, warning_secs: u64) {
        let body = format!("Shutting down in {warning_secs}s — commit your work now");
        let mut delivered = 0usize;

        for (member_name, handle) in self.shim_handles.iter_mut() {
            if handle.is_terminal() || !handle.is_ready() {
                debug!(
                    member = member_name.as_str(),
                    state = %handle.state,
                    "skipping shutdown warning because agent is not ready for live delivery"
                );
                continue;
            }

            match handle.send_message("daemon", &body) {
                Ok(()) => {
                    delivered += 1;
                    let _ = crate::team::append_shim_event_log(
                        &self.config.project_root,
                        member_name,
                        &format!("-> daemon: {body}"),
                    );
                }
                Err(error) => {
                    warn!(
                        member = member_name.as_str(),
                        error = %error,
                        "failed to send live shutdown warning"
                    );
                }
            }
        }

        info!(
            warning_secs,
            delivered, "sent live shutdown warning to ready agents"
        );
        if warning_secs > 0 {
            std::thread::sleep(Duration::from_secs(warning_secs));
        }
    }

    fn preserve_work_before_shutdown(&mut self) {
        let names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.use_worktrees)
            .map(|member| member.name.clone())
            .collect();
        for member_name in names {
            let worktree = self.worktree_dir(&member_name);
            self.preserve_worktree_before_restart(&member_name, &worktree, "daemon shutdown");
        }
    }

    fn warn_members_about_shutdown(&mut self) {
        let timeout_secs = self.config.team_config.shim_shutdown_timeout_secs;
        let recipients: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type != RoleType::User)
            .map(|member| member.name.clone())
            .collect();
        if recipients.is_empty() {
            return;
        }

        let warning = format!("Shutting down in {timeout_secs}s - commit your work now");
        info!(
            recipients = recipients.len(),
            timeout_secs, "warning members before shutdown"
        );
        for recipient in recipients {
            let delivery_result = if let Some(handle) = self.shim_handles.get_mut(&recipient) {
                if handle.is_terminal() {
                    Ok(())
                } else {
                    handle.send_message("daemon", &warning)
                }
            } else {
                self.queue_message("daemon", &recipient, &warning)
            };

            if let Err(error) = delivery_result {
                warn!(
                    member = recipient.as_str(),
                    error = %error,
                    "failed to send shutdown warning"
                );
            }
        }

        if timeout_secs > 0 {
            std::thread::sleep(Duration::from_secs(timeout_secs as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::team::inbox;
    use crate::team::test_support::{TestDaemonBuilder, engineer_member, write_owned_task_file};

    /// Ticket #636 acceptance test: invoking `tick()` on an empty daemon
    /// produces a `Default`-shaped report (cycle advances, no errors, all
    /// observability vecs empty). This pins the `TickReport` contract for
    /// later phases of the scenario framework.
    #[test]
    fn tick_on_empty_daemon_returns_default_shaped_report() {
        let tmp = tempfile::tempdir().unwrap();
        // Bootstrap the board tasks directory so subsystems that read it
        // (owned-tasks intervention, review backlog, auto-unblock, cron
        // recycling, manager dispatch gap, architect utilization, board
        // replenishment, pipeline starvation) see an empty-but-valid
        // board instead of logging a read-directory failure.
        let tasks_dir = tmp.path().join(".batty/team_config/board/tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let report = daemon.tick();

        assert_eq!(report.cycle, 1, "first tick should bump cycle to 1");
        assert!(
            report.subsystem_errors.is_empty(),
            "empty daemon should record no subsystem errors, got {:?}",
            report.subsystem_errors
        );
        assert!(report.events_emitted.is_empty());
        assert!(report.state_transitions.is_empty());
        assert!(report.main_advanced_to.is_none());
        assert!(report.inbox_delivered.is_empty());
        assert!(report.tasks_transitioned.is_empty());
        assert!(report.ok(), "report.ok() should be true with no errors");

        // Ticking again advances the cycle counter monotonically.
        let second = daemon.tick();
        assert_eq!(second.cycle, 2, "second tick should bump cycle to 2");
        assert!(second.ok());
    }

    /// #707: ensures the daemon-restart resume-prompt path enqueues a
    /// "continuing Task #N" message for each engineer still holding an
    /// active_task after `restore_runtime_state`. Pre-fix, managers would
    /// not see these tasks (no claim), fresh-session engineers had no
    /// stimulus, and the dispatcher skipped already-claimed tasks — so
    /// the task+claim pair sat idle forever.
    #[test]
    fn enqueue_restart_resume_prompts_queues_message_for_each_active_task() {
        let tmp = tempfile::tempdir().unwrap();
        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 42, "resume-me", "in-progress", member_name);
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(member_name, Some("manager"), false)])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.enqueue_restart_resume_prompts();

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), member_name).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "expected one restart resume prompt in the engineer inbox"
        );
        let body = &messages[0].body;
        assert!(
            body.contains("Task #42"),
            "resume prompt must reference the task id, got {body:?}"
        );
        assert!(
            body.contains("Continuing"),
            "resume prompt must use the restart_assignment_message phrasing, got {body:?}"
        );
    }

    /// #707: empty-active-tasks fast path — no work, no I/O, no panic.
    #[test]
    fn enqueue_restart_resume_prompts_is_noop_when_no_active_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.enqueue_restart_resume_prompts();
    }
}
