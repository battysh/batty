//! Main daemon poll loop — signal handling, subsystem sequencing, heartbeat.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::config_reload::ConfigReloadMonitor;
use super::hot_reload::HotReloadMonitor;
use super::{TeamDaemon, standup, status};
use crate::team;
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
                shutdown_reason = "session_gone";
                info!("tmux session gone, shutting down");
                break;
            }

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
            self.run_optional_subsystem_step("process_telegram_queue", "telegram", |daemon| {
                daemon.process_telegram_queue()
            });
            self.run_recoverable_step("maybe_fire_nudges", |daemon| daemon.maybe_fire_nudges());
            self.run_recoverable_step("check_backend_health", |daemon| {
                daemon.check_backend_health()
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
                let generated =
                    standup::maybe_generate_standup(standup::StandupGenerationContext {
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
            self.run_recoverable_step("maybe_reload_binary", |daemon| {
                daemon.maybe_hot_reload_binary(hot_reload.as_mut())
            });
            self.run_recoverable_step("maybe_reload_config", |daemon| {
                daemon.maybe_hot_reload_config(config_reload.as_mut())
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

    /// Send Shutdown to all active shim handles, wait for exit, fall back to Kill.
    fn shutdown_all_shims(&mut self) {
        if self.shim_handles.is_empty() {
            return;
        }

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

    fn preserve_work_before_shutdown(&self) {
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
}
