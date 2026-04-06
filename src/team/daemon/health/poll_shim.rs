//! Shim event polling: reads events from shim channels, updates AgentHandle
//! state, and triggers existing daemon flows (completion, context exhaustion,
//! pane death, health monitoring, and stale detection).

use std::path::PathBuf;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::launcher::{
    LaunchIdentity, agent_supports_sdk_mode, canonical_agent_name, member_session_tracker_config,
    new_member_session_id, strip_nudge_section, write_launch_script,
};
use super::super::*;
use crate::shim::protocol::{Event, ShimState};
use crate::team::watcher::{SessionTrackerConfig, discover_claude_session_file};
use crate::team::{append_shim_event_log, shim_log_path};

#[derive(Debug, Clone)]
struct ShimRespawnPlan {
    agent_type: String,
    agent_cmd: String,
    work_dir: PathBuf,
    identity: Option<LaunchIdentity>,
    mode: &'static str,
}

impl TeamDaemon {
    fn maybe_persist_member_session_id(&mut self, member_name: &str) {
        let session_id = {
            let Some(watcher) = self.watchers.get_mut(member_name) else {
                return;
            };
            if let Err(error) = watcher.refresh_session_tracking() {
                debug!(
                    member = member_name,
                    error = %error,
                    "failed to refresh session tracking while persisting shim session id"
                );
                return;
            }
            watcher.current_session_id()
        };

        let Some(session_id) = session_id else {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == member_name)
            else {
                return;
            };
            let Some(tracker) = member_session_tracker_config(&self.config.project_root, member)
            else {
                return;
            };
            let SessionTrackerConfig::Claude { cwd } = tracker else {
                return;
            };
            let projects_root = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("/"))
                .join(".claude")
                .join("projects");
            let Some(session_file) = discover_claude_session_file(&projects_root, &cwd, None)
                .ok()
                .flatten()
            else {
                return;
            };
            let Some(session_id) = session_file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem.to_string())
            else {
                return;
            };
            if let Err(error) = self.persist_member_session_id(member_name, &session_id) {
                debug!(
                    member = member_name,
                    session_id,
                    error = %error,
                    "failed to persist member session id from claude file fallback"
                );
            }
            return;
        };

        if let Err(error) = self.persist_member_session_id(member_name, &session_id) {
            debug!(
                member = member_name,
                session_id,
                error = %error,
                "failed to persist member session id after shim event"
            );
        }
    }

    /// Poll all shim handles for events and process state transitions.
    ///
    /// Called from the main poll loop when `use_shim` is enabled. This is the
    /// shim equivalent of `poll_watchers()`.
    pub(in crate::team) fn poll_shim_handles(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.shim_handles.keys().cloned().collect();

        for name in &member_names {
            loop {
                // Drain all currently queued events for this shim before
                // moving on. A busy shim can emit SessionStats, state
                // transitions, and a Pong between daemon polls; reading only
                // one event per 5s daemon tick can leave Pong buried long
                // enough to trigger false stale warnings.
                let event = {
                    let Some(handle) = self.shim_handles.get_mut(name) else {
                        break;
                    };
                    match handle.try_recv_event() {
                        Ok(Some(event)) => event,
                        Ok(None) => break,
                        Err(error) => {
                            debug!(
                                member = name.as_str(),
                                error = %error,
                                "shim channel recv error"
                            );
                            break;
                        }
                    }
                };

                self.handle_shim_event(name, event)?;
            }
        }

        Ok(())
    }

    /// Process a single shim event for a named agent.
    fn handle_shim_event(&mut self, member_name: &str, event: Event) -> Result<()> {
        match event {
            Event::Ready => {
                let _ = append_shim_event_log(&self.config.project_root, member_name, "<- ready");
                info!(member = member_name, "shim agent ready");
                self.context_pressure_tracker.clear_member(member_name);

                // Drain any pending messages
                if self.pending_delivery_queue.contains_key(member_name) {
                    if let Err(error) = self.drain_pending_queue(member_name) {
                        warn!(
                            member = member_name,
                            error = %error,
                            "failed to drain pending queue after shim ready"
                        );
                    }
                }

                // Only preserve Working state if the *shim handle* was Working
                // (meaning work was mid-flight when the SDK connection dropped).
                // Do NOT use self.states (persisted daemon state) here — after a
                // full daemon restart the agent is freshly spawned and not working
                // on anything, even if the persisted state said Working.
                let preserve_working = self
                    .shim_handles
                    .get(member_name)
                    .map(|handle| handle.state == ShimState::Working)
                    .unwrap_or(false);

                if preserve_working {
                    debug!(
                        member = member_name,
                        "shim ready received while work already in flight; preserving working state"
                    );
                } else {
                    if let Some(handle) = self.shim_handles.get_mut(member_name) {
                        handle.apply_state_change(ShimState::Idle);
                    }
                    self.states
                        .insert(member_name.to_string(), MemberState::Idle);
                    self.update_automation_timers_for_state(member_name, MemberState::Idle);
                }

                self.maybe_persist_member_session_id(member_name);
            }

            Event::StateChanged { from, to, summary } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- state {from} -> {to}: {summary}"),
                );
                debug!(
                    member = member_name,
                    from = %from,
                    to = %to,
                    "shim state change"
                );

                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(to);
                }

                let member_state = match to {
                    ShimState::Idle => MemberState::Idle,
                    ShimState::Working => MemberState::Working,
                    ShimState::Starting => MemberState::Working,
                    ShimState::Dead | ShimState::ContextExhausted => MemberState::Working,
                };

                let prev_state = self.states.get(member_name).copied();
                if prev_state != Some(member_state) {
                    self.states.insert(member_name.to_string(), member_state);
                    self.update_automation_timers_for_state(member_name, member_state);
                }
                if member_state != MemberState::Working {
                    self.context_pressure_tracker.mark_not_working(member_name);
                }

                self.maybe_persist_member_session_id(member_name);

                // Handle terminal states
                if to == ShimState::Dead {
                    warn!(member = member_name, "shim reports agent died");
                    self.emit_event(TeamEvent::pane_death(member_name));
                    self.record_orchestrator_action(format!(
                        "lifecycle: shim reported death for {} — {}",
                        member_name, summary
                    ));
                } else if to == ShimState::ContextExhausted {
                    let task_id = self.active_task_id(member_name);
                    warn!(
                        member = member_name,
                        task_id, "shim reports context exhaustion"
                    );
                    self.record_context_exhausted(member_name, task_id, None);
                    self.record_orchestrator_action(format!(
                        "lifecycle: shim reported context exhaustion for {} — {}",
                        member_name, summary
                    ));
                }
            }

            Event::Completion {
                message_id: _,
                response,
                last_lines,
            } => {
                let summary = if response.trim().is_empty() {
                    last_lines.lines().last().unwrap_or("").trim().to_string()
                } else {
                    response.lines().next().unwrap_or("").trim().to_string()
                };
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- completion: {summary}"),
                );
                info!(
                    member = member_name,
                    response_len = response.len(),
                    "shim reports completion"
                );

                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(ShimState::Idle);
                }
                self.states
                    .insert(member_name.to_string(), MemberState::Idle);
                self.update_automation_timers_for_state(member_name, MemberState::Idle);
                self.maybe_persist_member_session_id(member_name);

                let is_architect = self
                    .config
                    .members
                    .iter()
                    .find(|member| member.name == member_name)
                    .is_some_and(|member| member.role_type == RoleType::Architect);
                if is_architect && self.planning_cycle_active {
                    if let Err(error) = self.handle_planning_response(&response) {
                        warn!(
                            member = member_name,
                            error = %error,
                            "planning response handling failed"
                        );
                    }
                    return Ok(());
                }

                // Trigger engineer completion flow if there's an active task
                if self.active_task_id(member_name).is_some() {
                    if let Err(error) = merge::handle_engineer_completion(self, member_name) {
                        warn!(
                            member = member_name,
                            error = %error,
                            "shim completion handling failed"
                        );
                    }
                }

                let _ = last_lines; // Available for logging if needed
            }

            Event::Died {
                exit_code,
                last_lines,
            } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!(
                        "<- died exit={exit_code:?}: {}",
                        last_lines.lines().last().unwrap_or("").trim()
                    ),
                );
                warn!(member = member_name, exit_code, "shim agent process died");

                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(ShimState::Dead);
                }

                self.emit_event(TeamEvent::pane_death(member_name));
                self.record_orchestrator_action(format!(
                    "lifecycle: shim process died for {} (exit_code={:?}): {}",
                    member_name, exit_code, last_lines
                ));

                if self.config.team_config.auto_respawn_on_crash {
                    self.record_member_crashed(member_name, true);
                    let respawn = if self.should_cold_respawn_codex_member(member_name, &last_lines)
                    {
                        self.handle_shim_cold_respawn(member_name, "missing saved Codex session")
                    } else {
                        self.handle_shim_crash_respawn(member_name)
                    };
                    if let Err(error) = respawn {
                        warn!(
                            member = member_name,
                            error = %error,
                            "shim crash respawn failed"
                        );
                    }
                } else {
                    self.record_member_crashed(member_name, false);
                    self.escalate_shim_crash(member_name, exit_code);
                }
            }

            Event::ContextExhausted {
                message,
                last_lines,
            } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- context_exhausted: {message}"),
                );
                let task_id = self.active_task_id(member_name);
                warn!(
                    member = member_name,
                    task_id,
                    message = message.as_str(),
                    "shim reports context exhaustion"
                );

                // Send Shutdown to the exhausted shim before respawning
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(ShimState::ContextExhausted);
                    if let Err(error) = handle.send_shutdown(5) {
                        debug!(
                            member = member_name,
                            error = %error,
                            "failed to send shutdown to context-exhausted shim"
                        );
                    }
                }

                self.record_context_exhausted(member_name, task_id, None);
                self.record_orchestrator_action(format!(
                    "lifecycle: shim context exhausted for {}: {} — {}",
                    member_name, message, last_lines
                ));

                if let Err(error) = self.handle_context_exhaustion(member_name) {
                    warn!(
                        member = member_name,
                        error = %error,
                        "shim context exhaustion restart failed"
                    );
                }
            }

            Event::Pong => {
                // Pong events are high-frequency health heartbeats — don't log
                // them to event logs or daemon output to reduce noise.
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_pong();
                }
            }

            Event::SessionStats {
                output_bytes,
                uptime_secs,
            } => {
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_output_bytes(output_bytes);
                }
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- stats output_bytes={output_bytes} uptime_secs={uptime_secs}"),
                );
                self.handle_context_pressure_stats(member_name, output_bytes, uptime_secs)?;
            }

            Event::Warning { message, idle_secs } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- warning idle_secs={idle_secs:?}: {message}"),
                );

                // "message queued while agent working" is normal queue behavior,
                // not a stall. Only surface genuine stall warnings to the manager.
                if message.contains("message queued while agent working") {
                    info!(
                        member = member_name,
                        message = message.as_str(),
                        "shim warning: message queued (not a stall)"
                    );
                } else {
                    info!(
                        member = member_name,
                        idle_secs,
                        message = message.as_str(),
                        "shim warning: potential stall"
                    );
                    self.surface_shim_stall_warning(member_name, &message, idle_secs);
                }
            }

            Event::ScreenCapture { .. } | Event::State { .. } => {
                debug!(member = member_name, event = ?event, "shim event (unhandled in poll)");
            }

            Event::Error { command, reason } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- error {command}: {reason}"),
                );
                if self.should_cold_respawn_codex_member(member_name, &reason) {
                    self.record_member_crashed(member_name, true);
                    if let Err(error) =
                        self.handle_shim_cold_respawn(member_name, "missing saved Codex session")
                    {
                        warn!(
                            member = member_name,
                            error = %error,
                            "cold shim respawn after resume failure failed"
                        );
                    }
                    return Ok(());
                }
                debug!(member = member_name, command, reason, "shim error");
            }
        }

        Ok(())
    }

    fn should_cold_respawn_codex_member(&self, member_name: &str, detail: &str) -> bool {
        let Some(handle) = self.shim_handles.get(member_name) else {
            return false;
        };
        if handle.agent_type != "codex" {
            return false;
        }
        if !is_missing_codex_saved_session(detail) {
            return false;
        }
        shim_agent_cmd_uses_resume(&handle.agent_cmd)
    }

    fn cold_respawn_plan(&self, member_name: &str) -> Result<Option<ShimRespawnPlan>> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(None);
        };
        let Some(handle) = self.shim_handles.get(member_name) else {
            return Ok(None);
        };

        if let Some(task) = self.active_task(member_name)? {
            let team_config_dir = self.config.project_root.join(".batty").join("team_config");
            let role_context = strip_nudge_section(&self.load_prompt(&member, &team_config_dir));
            let prompt = Self::restart_assignment_message(&task);
            let work_dir = self.member_work_dir(&member);
            let role = self
                .config
                .team_config
                .role_def(&member.role_name)
                .with_context(|| format!("missing role definition for member '{}'", member.name))?;
            let claude_auth = self.config.team_config.resolve_claude_auth(role);
            let normalized_agent =
                canonical_agent_name(member.agent.as_deref().unwrap_or("claude"));
            let session_id = new_member_session_id(&normalized_agent);
            let agent_name = member.agent.as_deref().unwrap_or("claude");
            let sdk_mode_task =
                agent_supports_sdk_mode(agent_name) && self.config.team_config.use_sdk_mode;
            let agent_cmd = write_launch_script(
                member_name,
                agent_name,
                &claude_auth,
                &prompt,
                Some(&role_context),
                &work_dir,
                &self.config.project_root,
                false,
                false,
                session_id.as_deref(),
                sdk_mode_task,
            )?;
            return Ok(Some(ShimRespawnPlan {
                agent_type: handle.agent_type.clone(),
                agent_cmd,
                work_dir,
                identity: Some(LaunchIdentity {
                    agent: normalized_agent,
                    prompt,
                    session_id,
                }),
                mode: "cold-task-respawn",
            }));
        }

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let prompt = strip_nudge_section(&self.load_prompt(&member, &team_config_dir));
        let work_dir = self.member_work_dir(&member);
        let role = self
            .config
            .team_config
            .role_def(&member.role_name)
            .with_context(|| format!("missing role definition for member '{}'", member.name))?;
        let claude_auth = self.config.team_config.resolve_claude_auth(role);
        let normalized_agent = canonical_agent_name(member.agent.as_deref().unwrap_or("claude"));
        let session_id = new_member_session_id(&normalized_agent);
        let agent_name_idle = member.agent.as_deref().unwrap_or("claude");
        let sdk_mode_idle =
            agent_supports_sdk_mode(agent_name_idle) && self.config.team_config.use_sdk_mode;
        let agent_cmd = write_launch_script(
            member_name,
            agent_name_idle,
            &claude_auth,
            &prompt,
            Some(&prompt),
            &work_dir,
            &self.config.project_root,
            true,
            false,
            session_id.as_deref(),
            sdk_mode_idle,
        )?;
        Ok(Some(ShimRespawnPlan {
            agent_type: handle.agent_type.clone(),
            agent_cmd,
            work_dir,
            identity: Some(LaunchIdentity {
                agent: normalized_agent,
                prompt,
                session_id,
            }),
            mode: "cold-member-respawn",
        }))
    }

    pub(super) fn handle_shim_cold_respawn(
        &mut self,
        member_name: &str,
        reason: &str,
    ) -> Result<()> {
        let Some(plan) = self.cold_respawn_plan(member_name)? else {
            return Ok(());
        };

        self.preserve_worktree_before_restart(member_name, &plan.work_dir, reason);

        if let Some(handle) = self.shim_handles.get_mut(member_name)
            && let Err(error) = handle.send_shutdown(5)
        {
            debug!(
                member = member_name,
                error = %error,
                "failed to send shutdown before cold respawn"
            );
        }

        info!(
            member = member_name,
            reason, "downgrading warm resume to cold shim respawn"
        );

        let log_path = shim_log_path(&self.config.project_root, member_name);
        let sdk_mode =
            agent_supports_sdk_mode(&plan.agent_type) && self.config.team_config.use_sdk_mode;
        let new_handle = super::super::shim_spawn::spawn_shim(
            member_name,
            &plan.agent_type,
            &plan.agent_cmd,
            &plan.work_dir,
            Some(&log_path),
            self.config
                .team_config
                .workflow_policy
                .graceful_shutdown_timeout_secs,
            self.config
                .team_config
                .workflow_policy
                .auto_commit_on_restart,
            sdk_mode,
        )?;

        if let Some(identity) = plan.identity.clone() {
            if let Some(watcher) = self.watchers.get_mut(member_name) {
                watcher.set_session_id(identity.session_id.clone());
            }
            self.persist_member_launch_identity(member_name, identity)?;
        }

        self.shim_handles
            .insert(member_name.to_string(), new_handle);
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        self.update_automation_timers_for_state(member_name, MemberState::Working);
        self.emit_event(TeamEvent::pane_respawned(member_name));
        self.record_orchestrator_action(format!(
            "lifecycle: downgraded warm resume to {} for {} after {}",
            plan.mode, member_name, reason
        ));
        Ok(())
    }

    /// Respawn a shim after a crash when auto_respawn_on_crash is enabled.
    fn handle_shim_crash_respawn(&mut self, member_name: &str) -> Result<()> {
        let (agent_type, agent_cmd, work_dir) = {
            let Some(handle) = self.shim_handles.get(member_name) else {
                return Ok(());
            };
            (
                handle.agent_type.clone(),
                handle.agent_cmd.clone(),
                handle.work_dir.clone(),
            )
        };

        self.preserve_worktree_before_restart(member_name, &work_dir, "shim crash respawn");

        info!(member = member_name, "auto-respawning shim after crash");

        let sdk_mode = agent_supports_sdk_mode(&agent_type) && self.config.team_config.use_sdk_mode;
        let new_handle = super::super::shim_spawn::spawn_shim(
            member_name,
            &agent_type,
            &agent_cmd,
            &work_dir,
            None,
            self.config
                .team_config
                .workflow_policy
                .graceful_shutdown_timeout_secs,
            self.config
                .team_config
                .workflow_policy
                .auto_commit_on_restart,
            sdk_mode,
        )?;
        self.shim_handles
            .insert(member_name.to_string(), new_handle);
        self.emit_event(TeamEvent::pane_respawned(member_name));
        self.record_orchestrator_action(format!(
            "lifecycle: auto-respawned shim for {} after crash",
            member_name
        ));
        Ok(())
    }

    /// Escalate a crash to the manager when auto_respawn_on_crash is disabled.
    fn escalate_shim_crash(&mut self, member_name: &str, exit_code: Option<i32>) {
        let manager = self
            .config
            .members
            .iter()
            .find(|m| m.name == member_name)
            .and_then(|m| m.reports_to.clone());

        let body = format!(
            "Agent {} crashed (exit_code={:?}). Auto-respawn is disabled.\n\
             Please investigate and restart manually, or enable auto_respawn_on_crash in team.yaml.",
            member_name, exit_code,
        );

        if let Some(manager_name) = manager {
            if let Err(error) = self.queue_message("daemon", &manager_name, &body) {
                warn!(
                    member = member_name,
                    manager = manager_name.as_str(),
                    error = %error,
                    "failed to escalate crash to manager"
                );
            }
        } else {
            warn!(
                member = member_name,
                "shim crashed but no manager to escalate to"
            );
        }
        self.record_orchestrator_action(format!(
            "lifecycle: escalated crash for {} (no auto-respawn)",
            member_name
        ));
    }

    /// Surface a shim stall warning to the manager via the nudge system.
    fn surface_shim_stall_warning(
        &mut self,
        member_name: &str,
        message: &str,
        idle_secs: Option<u64>,
    ) {
        let manager = self
            .config
            .members
            .iter()
            .find(|m| m.name == member_name)
            .and_then(|m| m.reports_to.clone());

        let idle_info = idle_secs
            .map(|s| format!(" (idle for {}s)", s))
            .unwrap_or_default();
        let body = format!("Shim warning for {}{}: {}", member_name, idle_info, message);

        if let Some(manager_name) = manager {
            if let Err(error) = self.queue_message("daemon", &manager_name, &body) {
                warn!(
                    member = member_name,
                    error = %error,
                    "failed to surface stall warning to manager"
                );
            }
        }
        self.record_orchestrator_action(format!(
            "lifecycle: stall warning for {}{}: {}",
            member_name, idle_info, message
        ));
    }

    /// Detect agents stuck in "Working" state longer than the configured
    /// timeout and force-transition them to Idle. This prevents the cascading
    /// deadlock where a stuck shim state classifier permanently blocks message
    /// delivery and pipeline starvation detection.
    pub(in crate::team) fn check_working_state_timeouts(&mut self) -> Result<()> {
        let timeout_secs = self.config.team_config.shim_working_state_timeout_secs;
        if timeout_secs == 0 {
            return Ok(());
        }

        let stuck_members: Vec<String> = self
            .shim_handles
            .iter()
            .filter(|(_, handle)| {
                handle.state == ShimState::Working
                    && handle.secs_since_state_change() > timeout_secs
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in stuck_members {
            let secs = self
                .shim_handles
                .get(&name)
                .map(|h| h.secs_since_state_change())
                .unwrap_or(0);
            warn!(
                member = name.as_str(),
                secs_in_working = secs,
                timeout_secs,
                "force-transitioning stuck agent from Working to Idle"
            );
            let _ = append_shim_event_log(
                &self.config.project_root,
                &name,
                &format!(
                    "<- forced idle (stuck working for {}s, timeout={}s)",
                    secs, timeout_secs
                ),
            );
            self.record_orchestrator_action(format!(
                "health: force-idle {} after {}s stuck in Working (timeout={}s)",
                name, secs, timeout_secs
            ));

            if let Some(handle) = self.shim_handles.get_mut(&name) {
                handle.apply_state_change(ShimState::Idle);
            }
            self.states.insert(name.clone(), MemberState::Idle);
            self.update_automation_timers_for_state(&name, MemberState::Idle);

            // Drain any pending messages that were stuck waiting
            if self.pending_delivery_queue.contains_key(&name) {
                if let Err(error) = self.drain_pending_queue(&name) {
                    warn!(
                        member = name.as_str(),
                        error = %error,
                        "failed to drain pending queue after working-state timeout"
                    );
                }
            }
        }
        Ok(())
    }
}

fn is_missing_codex_saved_session(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("no saved session found with id") || detail.contains("no saved session found")
}

fn shim_agent_cmd_uses_resume(agent_cmd: &str) -> bool {
    if agent_cmd.contains("codex resume ") {
        return true;
    }

    let trimmed = agent_cmd.trim();
    if let Some(path) = trimmed
        .strip_prefix("bash '")
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            trimmed
                .strip_prefix("bash \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
    {
        return std::fs::read_to_string(path)
            .map(|script| script.contains("codex resume "))
            .unwrap_or(false);
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{
        EnvVarGuard, PATH_LOCK, TestDaemonBuilder, engineer_member, init_git_repo, manager_member,
        write_owned_task_file_with_context,
    };
    use std::collections::HashMap;

    fn insert_mock_handle(daemon: &mut TeamDaemon, name: &str) -> crate::shim::protocol::Channel {
        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let mut channel = crate::shim::protocol::Channel::new(parent);
        channel
            .set_read_timeout(Some(std::time::Duration::from_millis(5)))
            .unwrap();
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            name.into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert(name.to_string(), handle);
        crate::shim::protocol::Channel::new(child)
    }

    fn insert_mock_codex_handle(
        daemon: &mut TeamDaemon,
        name: &str,
        agent_cmd: impl Into<String>,
        work_dir: PathBuf,
    ) {
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            name.into(),
            crate::shim::protocol::Channel::new(parent),
            999,
            "codex".into(),
            agent_cmd.into(),
            work_dir,
        );
        daemon.shim_handles.insert(name.to_string(), handle);
    }

    #[test]
    fn poll_shim_handles_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        assert!(daemon.shim_handles.is_empty());
        daemon.poll_shim_handles().unwrap();
    }

    #[test]
    fn handle_shim_event_ready_sets_idle_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon.handle_shim_event("eng-1", Event::Ready).unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
        assert!(daemon.shim_handles["eng-1"].is_ready());
    }

    #[test]
    fn handle_shim_event_ready_preserves_existing_working_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);
        daemon.states.insert("eng-1".into(), MemberState::Working);

        daemon.handle_shim_event("eng-1", Event::Ready).unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        assert!(daemon.shim_handles["eng-1"].is_working());
    }

    #[test]
    fn handle_shim_event_state_changed_to_working() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::StateChanged {
                    from: ShimState::Idle,
                    to: ShimState::Working,
                    summary: "working now".into(),
                },
            )
            .unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        assert!(daemon.shim_handles["eng-1"].is_working());
    }

    #[test]
    fn poll_shim_handles_drains_multiple_queued_events_in_one_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let mut child = insert_mock_handle(&mut daemon, "eng-1");

        child
            .send(&Event::StateChanged {
                from: ShimState::Idle,
                to: ShimState::Working,
                summary: "started work".into(),
            })
            .unwrap();
        child
            .send(&Event::SessionStats {
                output_bytes: 512,
                uptime_secs: 42,
            })
            .unwrap();
        child.send(&Event::Pong).unwrap();

        daemon.poll_shim_handles().unwrap();

        let handle = daemon.shim_handles.get("eng-1").unwrap();
        assert!(handle.is_working());
        assert_eq!(handle.output_bytes, 512);
        assert!(handle.last_pong_at.is_some());
        assert!(handle.last_activity_at.is_some());
    }

    #[test]
    fn handle_shim_event_completion_sets_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::Completion {
                    message_id: None,
                    response: "done!".into(),
                    last_lines: "$ ".into(),
                },
            )
            .unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
        assert!(daemon.shim_handles["eng-1"].is_ready());
    }

    #[test]
    fn handle_shim_event_died_marks_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::Died {
                    exit_code: Some(1),
                    last_lines: "error".into(),
                },
            )
            .unwrap();

        assert!(daemon.shim_handles["eng-1"].is_terminal());
    }

    #[test]
    fn handle_shim_event_died_escalates_when_no_auto_respawn() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::Died {
                    exit_code: Some(137),
                    last_lines: "killed".into(),
                },
            )
            .unwrap();

        // Verify escalation message was sent to manager
        let messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(
            messages.iter().any(|msg| msg.body.contains("crashed")),
            "manager should receive crash escalation"
        );
    }

    #[test]
    fn handle_shim_event_context_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::ContextExhausted {
                    message: "out of context".into(),
                    last_lines: "last output".into(),
                },
            )
            .unwrap();

        assert!(daemon.shim_handles["eng-1"].is_terminal());
    }

    #[test]
    fn handle_shim_event_pong_records_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        assert!(daemon.shim_handles["eng-1"].last_pong_at.is_none());

        daemon.handle_shim_event("eng-1", Event::Pong).unwrap();

        assert!(daemon.shim_handles["eng-1"].last_pong_at.is_some());
    }

    #[test]
    fn handle_shim_event_warning_surfaces_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::Warning {
                    message: "no screen change detected".into(),
                    idle_secs: Some(300),
                },
            )
            .unwrap();

        let messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(
            messages.iter().any(|msg| msg.body.contains("Shim warning")),
            "manager should receive stall warning"
        );
    }

    // ── working-state timeout tests ──

    #[test]
    fn check_working_state_timeouts_noop_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.shim_working_state_timeout_secs = 0;
        insert_mock_handle(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);
        daemon.states.insert("eng-1".into(), MemberState::Working);

        daemon.check_working_state_timeouts().unwrap();

        // Should remain Working since timeout is disabled
        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
    }

    #[test]
    fn check_working_state_timeouts_no_change_when_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1800;
        insert_mock_handle(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);
        daemon.states.insert("eng-1".into(), MemberState::Working);

        daemon.check_working_state_timeouts().unwrap();

        // Should remain Working since it just transitioned
        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
    }

    #[test]
    fn check_working_state_timeouts_forces_idle_when_expired() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        // Set timeout to 0 seconds effectively — but we need > 0 to not skip.
        // Instead, set it to 1 second and backdate the state change.
        daemon.config.team_config.shim_working_state_timeout_secs = 1;
        insert_mock_handle(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);
        // Backdate the state_changed_at to simulate being stuck
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        daemon.states.insert("eng-1".into(), MemberState::Working);

        daemon.check_working_state_timeouts().unwrap();

        // Should have been force-transitioned to Idle
        assert_eq!(
            daemon.states.get("eng-1"),
            Some(&MemberState::Idle),
            "stuck agent should be force-transitioned to idle"
        );
        assert!(
            daemon.shim_handles["eng-1"].is_ready(),
            "shim handle should be in Idle state"
        );
    }

    #[test]
    fn check_working_state_timeouts_skips_idle_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1;
        insert_mock_handle(&mut daemon, "eng-1");
        // Agent is Idle, not Working — should not be affected
        daemon.states.insert("eng-1".into(), MemberState::Idle);

        daemon.check_working_state_timeouts().unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
    }

    #[test]
    fn check_working_state_timeouts_drains_pending_queue() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", None, false)])
            .build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1;
        insert_mock_handle(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        daemon.states.insert("eng-1".into(), MemberState::Working);

        // Queue a pending message
        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(crate::team::delivery::PendingMessage {
                from: "architect".to_string(),
                body: "do something".to_string(),
                queued_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
            });

        daemon.check_working_state_timeouts().unwrap();

        // Pending queue should have been drained
        assert!(
            !daemon.pending_delivery_queue.contains_key("eng-1"),
            "pending queue should be drained after working-state timeout"
        );
    }

    #[test]
    fn missing_codex_saved_session_detection_matches_known_error_text() {
        assert!(is_missing_codex_saved_session(
            "No saved session found with ID codex-session-123"
        ));
        assert!(!is_missing_codex_saved_session("different startup failure"));
    }

    #[test]
    fn shim_agent_cmd_uses_resume_detects_resume_in_wrapper_script() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("launch.sh");
        std::fs::write(&script, "#!/bin/bash\nexec codex resume 'stale-id'\n").unwrap();

        assert!(shim_agent_cmd_uses_resume(&format!(
            "bash '{}'",
            script.display()
        )));
        assert!(!shim_agent_cmd_uses_resume("bash '/tmp/missing.sh'"));
    }

    #[test]
    fn should_cold_respawn_codex_member_ignores_unrelated_healthy_members() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        insert_mock_handle(&mut daemon, "eng-1");

        assert!(
            !daemon.should_cold_respawn_codex_member(
                "eng-1",
                "No saved session found with ID stale-id"
            )
        );
    }

    #[test]
    fn cold_respawn_plan_rebuilds_active_task_context_for_codex_engineer() {
        let _path_lock = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-cold-respawn-plan");
        std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.to_string_lossy().as_ref());

        let worktree_path = repo.join(".batty").join("worktrees").join("eng-1");
        let branch_name = "eng-1/42";
        write_owned_task_file_with_context(
            &repo,
            42,
            "respawn-task",
            "in-progress",
            "eng-1",
            branch_name,
            worktree_path.to_string_lossy().as_ref(),
        );

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
                engineer_member("eng-2", Some("manager"), true),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Working),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        let launch_script = std::env::temp_dir().join("batty-test-codex-resume.sh");
        std::fs::write(
            &launch_script,
            "#!/bin/bash\nexec codex resume 'stale-session'\n",
        )
        .unwrap();
        insert_mock_codex_handle(
            &mut daemon,
            "eng-1",
            format!("bash '{}'", launch_script.display()),
            worktree_path.clone(),
        );
        insert_mock_codex_handle(
            &mut daemon,
            "eng-2",
            "bash '/tmp/batty-test-cold-healthy.sh'",
            repo.join(".batty").join("worktrees").join("eng-2"),
        );

        assert!(daemon.should_cold_respawn_codex_member(
            "eng-1",
            "No saved session found with ID stale-session"
        ));
        assert!(!daemon.should_cold_respawn_codex_member(
            "eng-2",
            "No saved session found with ID stale-session"
        ));

        let plan = daemon
            .cold_respawn_plan("eng-1")
            .unwrap()
            .expect("expected cold respawn plan");
        assert_eq!(plan.mode, "cold-task-respawn");
        assert_eq!(plan.work_dir, worktree_path);
        assert!(!plan.agent_cmd.contains("codex resume"));
        let identity = plan.identity.expect("missing launch identity");
        assert_eq!(identity.agent, "codex-cli");
        assert!(
            identity
                .prompt
                .contains("Continuing Task #42: respawn-task")
        );
        assert!(identity.prompt.contains(&format!("Branch: {branch_name}")));
        assert!(
            identity
                .prompt
                .contains(&format!("Worktree: {}", worktree_path.display()))
        );
    }
}
