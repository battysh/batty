//! Shim event polling: reads events from shim channels, updates AgentHandle
//! state, and triggers existing daemon flows (completion, context exhaustion,
//! pane death, health monitoring, and stale detection).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::launcher::{
    LaunchIdentity, agent_supports_sdk_mode, canonical_agent_name, member_session_tracker_config,
    new_member_session_id, strip_nudge_section, write_launch_script,
};
use super::super::*;
use super::CONTEXT_RESTART_COOLDOWN;
use crate::shim::protocol::{Event, ShimState, ShutdownReason};
use crate::team::watcher::{SessionTrackerConfig, discover_claude_session_file};
use crate::team::{append_shim_event_log, shim_log_path};

const SUPERVISORY_STALLED_REASON: &str = "supervisory_stalled";
const SUPERVISORY_STALL_ESCALATED_REASON: &str = "supervisory_stall_escalated";

#[derive(Debug, Clone)]
struct ShimRespawnPlan {
    agent_type: String,
    agent_cmd: String,
    work_dir: PathBuf,
    identity: Option<LaunchIdentity>,
    mode: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimDisconnectDisposition {
    RestartHandoff,
    CrashRespawn,
    ContextExhausted,
}

impl ShimDisconnectDisposition {
    fn reason(self) -> &'static str {
        match self {
            Self::RestartHandoff => "restart_handoff",
            Self::CrashRespawn => "crash_respawn",
            Self::ContextExhausted => "context_exhausted",
        }
    }

    fn expected(self) -> bool {
        true
    }

    fn shutdown_reason(self) -> ShutdownReason {
        match self {
            Self::RestartHandoff => ShutdownReason::RestartHandoff,
            Self::CrashRespawn => ShutdownReason::Requested,
            Self::ContextExhausted => ShutdownReason::ContextExhausted,
        }
    }
}

fn wait_for_expected_disconnect_handoff(
    member_name: String,
    mut handle: crate::team::daemon::agent_handle::AgentHandle,
    timeout: Duration,
) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let alive = unsafe { libc::kill(handle.child_pid as i32, 0) } == 0;
            if !alive {
                debug!(
                    member = member_name.as_str(),
                    pid = handle.child_pid,
                    "retired shim exited after disconnect handoff"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                warn!(
                    member = member_name.as_str(),
                    pid = handle.child_pid,
                    "retired shim exceeded disconnect handoff timeout; forcing kill"
                );
                let _ = handle.send_kill();
                unsafe {
                    libc::kill(handle.child_pid as i32, libc::SIGKILL);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
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
                        handle.clear_in_flight_message();
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

            Event::MessageDelivered { id } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- message_delivered id={id}"),
                );
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_activity();
                }
                debug!(
                    member = member_name,
                    message_id = id,
                    "shim delivered message"
                );
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
                    handle.clear_in_flight_message();
                }
                self.states
                    .insert(member_name.to_string(), MemberState::Idle);
                self.update_automation_timers_for_state(member_name, MemberState::Idle);
                self.maybe_persist_member_session_id(member_name);

                if self.handle_stalled_mid_turn_completion(member_name, &response)? {
                    return Ok(());
                }

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
                    if let Err(error) =
                        handle.send_shutdown_with_reason(5, ShutdownReason::ContextExhausted)
                    {
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

            Event::ContextApproaching {
                message,
                input_tokens,
                output_tokens,
            } => {
                let task_id = self.active_task_id(member_name);
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!(
                        "<- context_approaching: {message} (input_tokens={input_tokens}, output_tokens={output_tokens})"
                    ),
                );
                warn!(
                    member = member_name,
                    task_id,
                    input_tokens,
                    output_tokens,
                    "shim reports context approaching limit — proactive handoff"
                );
                self.record_orchestrator_action(format!(
                    "health: proactive context handoff for {} — {} (tokens: {}/{})",
                    member_name, message, input_tokens, output_tokens
                ));

                if let Err(error) = self.handle_context_pressure_restart(member_name) {
                    warn!(
                        member = member_name,
                        error = %error,
                        "proactive context handoff restart failed"
                    );
                } else if let Some(task_id) = task_id {
                    self.record_agent_restarted(
                        member_name,
                        task_id.to_string(),
                        "proactive_context_handoff",
                        1,
                    );
                }
                self.context_pressure_tracker.clear_member(member_name);
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
                context_usage_pct,
                ..
            } => {
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_output_bytes(output_bytes);
                }
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!(
                        "<- stats output_bytes={output_bytes} uptime_secs={uptime_secs} context_usage_pct={context_usage_pct:?}"
                    ),
                );
                self.handle_context_pressure_stats(
                    member_name,
                    output_bytes,
                    uptime_secs,
                    context_usage_pct,
                )?;
            }

            Event::ContextWarning {
                model,
                output_bytes,
                uptime_secs,
                used_tokens,
                context_limit_tokens,
                usage_pct,
                ..
            } => {
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_output_bytes(output_bytes);
                }
                let model_label = model
                    .as_deref()
                    .map(|value| format!(" model={value}"))
                    .unwrap_or_default();
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!(
                        "<- context_warning usage_pct={usage_pct} used_tokens={used_tokens} limit_tokens={context_limit_tokens}{model_label}"
                    ),
                );
                warn!(
                    member = member_name,
                    model = model.as_deref().unwrap_or("unknown"),
                    output_bytes,
                    uptime_secs,
                    used_tokens,
                    context_limit_tokens,
                    usage_pct,
                    "shim reports proactive context pressure"
                );
                self.record_orchestrator_action(format!(
                    "health: proactive context warning for {} at {}% of {} tokens{}",
                    member_name, usage_pct, context_limit_tokens, model_label
                ));
                self.handle_context_pressure_warning(
                    member_name,
                    output_bytes,
                    uptime_secs,
                    usage_pct,
                )?;
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

            Event::DeliveryFailed { id, reason } => {
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    member_name,
                    &format!("<- delivery_failed id={id}: {reason}"),
                );
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_activity();
                }
                warn!(
                    member = member_name,
                    message_id = id,
                    reason = reason.as_str(),
                    "shim failed to deliver message"
                );
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

                // Quota exhaustion: mark backend unhealthy and stop dispatching.
                // Don't restart the agent — it will just hit the same error.
                if command == "QuotaExhausted" {
                    warn!(
                        member = member_name,
                        reason = reason.as_str(),
                        "backend quota exhausted — pausing agent"
                    );
                    self.backend_health.insert(
                        member_name.to_string(),
                        crate::agent::BackendHealth::QuotaExhausted,
                    );
                    self.record_orchestrator_action(format!(
                        "quota: {member_name} backend quota exhausted — {reason}"
                    ));
                    self.emit_event(crate::team::events::TeamEvent::backend_quota_exhausted(
                        member_name,
                        &reason,
                    ));
                    return Ok(());
                }

                self.record_context_pressure_failure(member_name);
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

    fn cold_respawn_plan(&mut self, member_name: &str) -> Result<Option<ShimRespawnPlan>> {
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
        let existing_agent_type = handle.agent_type.clone();

        if let Some(task) = self.active_task(member_name)? {
            let team_config_dir = self.config.project_root.join(".batty").join("team_config");
            let role_context = strip_nudge_section(&self.load_prompt(&member, &team_config_dir));
            let work_dir = self.member_work_dir(&member);
            let prompt = self.restart_assignment_with_handoff(member_name, &task, &work_dir);
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
                agent_type: existing_agent_type,
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

    fn classify_shim_disconnect(
        &self,
        member_name: &str,
        reason: &str,
    ) -> ShimDisconnectDisposition {
        if reason == "shim_context_exhaustion" || reason == "context_exhaustion" {
            return ShimDisconnectDisposition::ContextExhausted;
        }
        if self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| {
                matches!(member.role_type, RoleType::Architect | RoleType::Manager)
            })
        {
            return ShimDisconnectDisposition::RestartHandoff;
        }
        ShimDisconnectDisposition::CrashRespawn
    }

    fn replay_or_requeue_pending_messages_after_respawn(
        &mut self,
        member_name: &str,
        previous_handle: &mut crate::team::daemon::agent_handle::AgentHandle,
        disposition: ShimDisconnectDisposition,
    ) -> Result<bool> {
        if disposition != ShimDisconnectDisposition::RestartHandoff
            || !self
                .config
                .members
                .iter()
                .find(|member| member.name == member_name)
                .is_some_and(|member| {
                    matches!(member.role_type, RoleType::Architect | RoleType::Manager)
                })
        {
            return Ok(false);
        }

        let Some(in_flight) = previous_handle.take_in_flight_message() else {
            return Ok(false);
        };

        let queue = self
            .pending_delivery_queue
            .entry(member_name.to_string())
            .or_default();
        queue.insert(
            0,
            PendingMessage {
                from: in_flight.from.clone(),
                body: in_flight.body.clone(),
                queued_at: std::time::Instant::now(),
            },
        );

        let preview = in_flight
            .body
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(160)
            .collect::<String>();
        let details = format!(
            "requeued in-flight supervisory message during restart handoff: from={} preview={preview}",
            in_flight.from
        );
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- replay pending after respawn: {details}"),
        );
        self.record_orchestrator_action(format!(
            "delivery: requeued supervisory in-flight message for {member_name} during restart handoff"
        ));
        Ok(true)
    }

    fn restart_shim_with_disconnect_handoff(
        &mut self,
        member_name: &str,
        plan: &ShimRespawnPlan,
        log_path: Option<&std::path::Path>,
        reason: &str,
        disposition: ShimDisconnectDisposition,
    ) -> Result<()> {
        let sdk_mode =
            agent_supports_sdk_mode(&plan.agent_type) && self.config.team_config.use_sdk_mode;
        let new_handle = super::super::shim_spawn::spawn_shim(
            member_name,
            &plan.agent_type,
            &plan.agent_cmd,
            &plan.work_dir,
            log_path,
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

        let handoff_timeout = Duration::from_secs(
            self.config
                .team_config
                .shim_shutdown_timeout_secs
                .max(1)
                .into(),
        );
        let requeued = if let Some(mut previous_handle) = self.shim_handles.remove(member_name) {
            let requeued = self.replay_or_requeue_pending_messages_after_respawn(
                member_name,
                &mut previous_handle,
                disposition,
            )?;
            let details = format!(
                "reason={} disposition={} requeued_inflight={requeued}",
                reason,
                disposition.reason()
            );
            self.record_shim_disconnect_event(
                member_name,
                disposition.reason(),
                &details,
                disposition.expected(),
            );
            let _ = append_shim_event_log(
                &self.config.project_root,
                member_name,
                &format!(
                    "<- disconnect handoff: {} (reason={reason}, requeued_inflight={requeued})",
                    disposition.reason()
                ),
            );
            if !previous_handle.is_terminal() {
                if let Err(error) =
                    previous_handle.send_shutdown_with_reason(5, disposition.shutdown_reason())
                {
                    debug!(
                        member = member_name,
                        error = %error,
                        "failed to send classified shutdown before respawn"
                    );
                }
                wait_for_expected_disconnect_handoff(
                    member_name.to_string(),
                    previous_handle,
                    handoff_timeout,
                );
            }
            requeued
        } else {
            false
        };

        self.shim_handles
            .insert(member_name.to_string(), new_handle);
        self.record_orchestrator_action(format!(
            "lifecycle: restarted shim for {member_name} with {} (reason={reason}, requeued_inflight={requeued})",
            disposition.reason()
        ));
        Ok(())
    }

    pub(super) fn handle_shim_cold_respawn(
        &mut self,
        member_name: &str,
        reason: &str,
    ) -> Result<()> {
        if let Some(task) = self.active_task(member_name)?
            && let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == member_name)
                .cloned()
        {
            let pane_id = self.config.pane_map.get(member_name).cloned();
            let work_dir = self.member_work_dir(&member);
            self.preserve_restart_context(
                member_name,
                &task,
                pane_id.as_deref(),
                &work_dir,
                reason,
            );
        }

        let Some(plan) = self.cold_respawn_plan(member_name)? else {
            return Ok(());
        };

        self.preserve_worktree_before_restart(member_name, &plan.work_dir, reason);

        info!(
            member = member_name,
            reason, "downgrading warm resume to cold shim respawn"
        );

        let log_path = shim_log_path(&self.config.project_root, member_name);
        let disposition = self.classify_shim_disconnect(member_name, reason);
        self.restart_shim_with_disconnect_handoff(
            member_name,
            &plan,
            Some(&log_path),
            reason,
            disposition,
        )?;
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
        let Some(plan) = self.cold_respawn_plan(member_name)? else {
            return Ok(());
        };

        self.preserve_worktree_before_restart(member_name, &plan.work_dir, "shim crash respawn");
        if let Some(task) = self.active_task(member_name)?
            && let Some(pane_id) = self.config.pane_map.get(member_name).cloned()
        {
            self.preserve_restart_context(
                member_name,
                &task,
                Some(&pane_id),
                &plan.work_dir,
                "shim_crash",
            );
        }

        info!(member = member_name, "auto-respawning shim after crash");

        self.restart_shim_with_disconnect_handoff(
            member_name,
            &plan,
            None,
            "shim_crash",
            ShimDisconnectDisposition::CrashRespawn,
        )?;
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
    /// timeout. Claude shims get a cold respawn; other backends are
    /// force-transitioned to Idle to unblock queueing and scheduling.
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
            let supervisory_role = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
                .map(|member| {
                    matches!(
                        member.role_type,
                        crate::team::config::RoleType::Architect
                            | crate::team::config::RoleType::Manager
                    )
                })
                .unwrap_or(false);
            let agent_type = self
                .shim_handles
                .get(&name)
                .map(|h| h.agent_type.clone())
                .unwrap_or_default();

            if supervisory_role {
                if let Err(error) = self.handle_supervisory_stall(&name, SUPERVISORY_STALLED_REASON)
                {
                    warn!(
                        member = name.as_str(),
                        error = %error,
                        "supervisory stall handling failed; continuing"
                    );
                }
                continue;
            }

            if is_claude_agent_type(&agent_type) {
                let cooldown_key = format!("stale-claude-respawn::{name}");
                let on_cooldown = self
                    .intervention_cooldowns
                    .get(&cooldown_key)
                    .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
                if on_cooldown {
                    continue;
                }

                warn!(
                    member = name.as_str(),
                    secs_in_working = secs,
                    timeout_secs,
                    "auto-restarting stale Claude shim"
                );
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    &name,
                    &format!(
                        "<- stale claude respawn (stuck working for {}s, timeout={}s)",
                        secs, timeout_secs
                    ),
                );
                self.record_orchestrator_action(format!(
                    "health: cold-respawn stale Claude agent {} after {}s stuck in Working (timeout={}s)",
                    name, secs, timeout_secs
                ));

                match self.handle_shim_cold_respawn(&name, "stale_claude_agent") {
                    Ok(()) => {
                        self.intervention_cooldowns
                            .insert(cooldown_key, std::time::Instant::now());
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            member = name.as_str(),
                            error = %error,
                            "stale Claude respawn failed; falling back to force-idle"
                        );
                    }
                }
            }

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

    fn supervisory_stall_summary(&self, name: &str) -> Option<String> {
        let timeout_secs = self.config.team_config.shim_working_state_timeout_secs;
        let stall_secs = self.shim_handles.get(name)?.secs_since_state_change();
        let signal = self.supervisory_progress_signal(name, timeout_secs);
        Some(format!(
            "{} (timeout={}s)",
            self.format_supervisory_stall_summary(name, stall_secs, &signal),
            timeout_secs
        ))
    }

    fn handle_supervisory_stall(&mut self, name: &str, reason: &str) -> anyhow::Result<()> {
        let stall_secs = self
            .shim_handles
            .get(name)
            .map(|handle| handle.secs_since_state_change())
            .unwrap_or(0);
        let summary = self.supervisory_stall_summary(name).unwrap_or_else(|| {
            format!("{name} stayed in Working for {stall_secs}s (reason={reason})")
        });
        let supervisory_task = format!("supervisory::{name}");

        tracing::warn!(member = name, reason, summary = %summary, "supervisory stall detected");

        let mut event = crate::team::events::TeamEvent::stall_detected_with_reason(
            name,
            None,
            stall_secs,
            Some(reason),
        );
        event.task = Some(supervisory_task.clone());
        event.details = Some(summary.clone());
        self.emit_event(event);

        let _ = append_shim_event_log(
            &self.config.project_root,
            name,
            &format!("<- supervisory stall detected: {summary}"),
        );
        self.record_orchestrator_action(format!("supervisory stall: {name} — {summary}"));

        let prior_restarts = self.supervisory_stall_restart_count(name)?;
        let restart_count = prior_restarts + 1;
        let max_restarts = self.config.team_config.workflow_policy.max_stall_restarts;
        let escalation_target = self
            .config
            .members
            .iter()
            .find(|member| member.name == name)
            .and_then(|member| member.reports_to.clone());

        if prior_restarts >= max_restarts && escalation_target.is_some() {
            let escalation_key = format!("stall-escalation::{name}");
            let on_cooldown = self
                .intervention_cooldowns
                .get(&escalation_key)
                .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
            if on_cooldown {
                return Ok(());
            }

            self.handle_shim_cold_respawn(name, reason)?;
            self.intervention_cooldowns
                .insert(escalation_key, std::time::Instant::now());

            if let Some(manager) = escalation_target
                && let Err(error) = self.queue_message(
                    "daemon",
                    &manager,
                    &format!(
                        "{name} stalled repeatedly while marked working ({summary}). Please intervene directly."
                    ),
                )
            {
                warn!(
                    member = name,
                    manager = manager.as_str(),
                    error = %error,
                    "failed to escalate supervisory stall"
                );
            }

            self.record_task_escalated(name, supervisory_task.clone(), Some(reason));
            self.record_agent_restarted(
                name,
                supervisory_task,
                SUPERVISORY_STALL_ESCALATED_REASON,
                restart_count,
            );
            return Ok(());
        }

        self.handle_shim_cold_respawn(name, reason)?;
        self.intervention_cooldowns
            .insert(format!("stall-restart::{name}"), std::time::Instant::now());
        self.record_agent_restarted(name, supervisory_task, reason, restart_count);
        Ok(())
    }

    fn supervisory_stall_restart_count(&self, name: &str) -> anyhow::Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let supervisory_task = format!("supervisory::{name}");
        let count = crate::team::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.role.as_deref() == Some(name))
            .filter(|event| event.task.as_deref() == Some(supervisory_task.as_str()))
            .filter(|event| event.reason.as_deref() == Some(SUPERVISORY_STALLED_REASON))
            .count() as u32;
        Ok(count)
    }
}

fn is_missing_codex_saved_session(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("no saved session found with id") || detail.contains("no saved session found")
}

fn is_claude_agent_type(agent_type: &str) -> bool {
    matches!(agent_type, "claude" | "claude-code")
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
    use crate::team::inbox;
    use crate::team::test_support::{
        EnvVarGuard, PATH_LOCK, TestDaemonBuilder, architect_member, engineer_member,
        init_git_repo, manager_member, setup_fake_backend, write_owned_task_file_with_context,
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
                input_tokens: 0,
                output_tokens: 0,
                context_usage_pct: None,
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
    fn handle_shim_event_context_warning_uses_policy_path_without_immediate_restart() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        insert_mock_handle(&mut daemon, "eng-1");
        daemon.states.insert("eng-1".into(), MemberState::Working);

        daemon
            .handle_shim_event(
                "eng-1",
                Event::ContextWarning {
                    model: Some("claude-sonnet-4-5".into()),
                    output_bytes: 64_000,
                    uptime_secs: 600,
                    input_tokens: 80_000,
                    cached_input_tokens: 5_000,
                    cache_creation_input_tokens: 4_000,
                    cache_read_input_tokens: 3_000,
                    output_tokens: 6_000,
                    reasoning_output_tokens: 2_000,
                    used_tokens: 100_000,
                    context_limit_tokens: 200_000,
                    usage_pct: 80,
                },
            )
            .unwrap();

        assert!(
            !daemon
                .intervention_cooldowns
                .contains_key(&TeamDaemon::context_restart_cooldown_key("eng-1")),
            "first proactive warning should not restart immediately"
        );
        assert!(
            !daemon.shim_handles["eng-1"].is_terminal(),
            "warning path should not force the shim into a terminal state"
        );

        let messages = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert!(
            messages
                .iter()
                .any(|msg| msg.body.contains("Context pressure is high")),
            "warning path should still nudge through the normal policy flow"
        );
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

    #[test]
    fn handle_shim_event_error_counts_toward_context_pressure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        insert_mock_handle(&mut daemon, "eng-1");

        daemon
            .handle_shim_event(
                "eng-1",
                Event::Error {
                    command: "SendMessage".into(),
                    reason: "PTY write failed".into(),
                },
            )
            .unwrap();
        daemon
            .handle_shim_event(
                "eng-1",
                Event::Error {
                    command: "SendMessage".into(),
                    reason: "agent in dead state".into(),
                },
            )
            .unwrap();
        daemon
            .handle_shim_event(
                "eng-1",
                Event::Error {
                    command: "startup".into(),
                    reason: "prompt timeout".into(),
                },
            )
            .unwrap();

        assert_eq!(
            daemon.context_pressure_tracker.shim_failure_count("eng-1"),
            3
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
        insert_mock_codex_handle(
            &mut daemon,
            "eng-1",
            "codex exec",
            std::path::PathBuf::from("/tmp/test"),
        );
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
    fn check_working_state_timeouts_restarts_stale_claude_agents() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "batty", "fake-batty.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = if original_path.is_empty() {
            fake_bin.display().to_string()
        } else {
            format!("{}:{original_path}", fake_bin.display())
        };
        let _path_guard = EnvVarGuard::set("PATH", &combined_path);
        let _batty_guard = EnvVarGuard::set(
            "BATTY_BINARY_PATH",
            fake_bin.join("batty").to_string_lossy().as_ref(),
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), false),
            ])
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

        daemon.check_working_state_timeouts().unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        assert_eq!(daemon.shim_handles["eng-1"].state, ShimState::Starting);
        assert!(
            daemon
                .intervention_cooldowns
                .contains_key("stale-claude-respawn::eng-1")
        );
    }

    #[test]
    fn check_working_state_timeouts_restarts_stale_claude_management_agents() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "batty", "fake-batty.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = if original_path.is_empty() {
            fake_bin.display().to_string()
        } else {
            format!("{}:{original_path}", fake_bin.display())
        };
        let _path_guard = EnvVarGuard::set("PATH", &combined_path);
        let _batty_guard = EnvVarGuard::set(
            "BATTY_BINARY_PATH",
            fake_bin.join("batty").to_string_lossy().as_ref(),
        );
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1;

        for member_name in ["architect", "manager"] {
            insert_mock_handle(&mut daemon, member_name);
            daemon
                .shim_handles
                .get_mut(member_name)
                .unwrap()
                .apply_state_change(ShimState::Working);
            daemon
                .shim_handles
                .get_mut(member_name)
                .unwrap()
                .state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
            daemon
                .states
                .insert(member_name.to_string(), MemberState::Working);
        }

        daemon.check_working_state_timeouts().unwrap();

        for member_name in ["architect", "manager"] {
            assert_eq!(
                daemon.states.get(member_name),
                Some(&MemberState::Working),
                "management role {member_name} should stay working while the replacement shim starts"
            );
            assert_eq!(daemon.shim_handles[member_name].state, ShimState::Starting);
            assert!(
                daemon
                    .intervention_cooldowns
                    .contains_key(&format!("stall-restart::{member_name}"))
            );
            assert!(
                !daemon.pending_delivery_queue.contains_key(member_name),
                "supervisory recovery chatter should stay out of {member_name}'s pending PTY queue"
            );
            assert!(
                inbox::pending_messages(&inbox_root, member_name)
                    .unwrap()
                    .is_empty(),
                "supervisory recovery chatter should not be queued into {member_name}'s inbox"
            );
        }

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        for member_name in ["architect", "manager"] {
            assert!(events.iter().any(|event| {
                event.event == "stall_detected"
                    && event.role.as_deref() == Some(member_name)
                    && event.reason.as_deref() == Some("supervisory_stalled")
            }));
            assert!(events.iter().any(|event| {
                event.event == "agent_restarted"
                    && event.role.as_deref() == Some(member_name)
                    && event.reason.as_deref() == Some("supervisory_stalled")
            }));
        }
    }

    #[test]
    fn supervisory_restart_handoff_requeues_inflight_message_and_marks_shutdown_reason() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "batty", "fake-batty.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = if original_path.is_empty() {
            fake_bin.display().to_string()
        } else {
            format!("{}:{original_path}", fake_bin.display())
        };
        let _path_guard = EnvVarGuard::set("PATH", &combined_path);
        let _batty_guard = EnvVarGuard::set(
            "BATTY_BINARY_PATH",
            fake_bin.join("batty").to_string_lossy().as_ref(),
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1;

        let mut child = insert_mock_handle(&mut daemon, "manager");
        {
            let handle = daemon.shim_handles.get_mut("manager").unwrap();
            handle.apply_state_change(ShimState::Working);
            handle
                .send_message(
                    "architect",
                    "Dispatch recovery needed: idle reports still have active work.",
                )
                .unwrap();
            handle.state_changed_at =
                std::time::Instant::now() - std::time::Duration::from_secs(10);
        }
        daemon
            .states
            .insert("manager".to_string(), MemberState::Working);

        daemon.check_working_state_timeouts().unwrap();

        let queue = daemon
            .pending_delivery_queue
            .get("manager")
            .expect("supervisory in-flight message should be requeued");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].from, "architect");
        assert!(
            queue[0]
                .body
                .contains("Dispatch recovery needed: idle reports still have active work.")
        );

        let first: crate::shim::protocol::Command = child.recv().unwrap().unwrap();
        assert!(matches!(
            first,
            crate::shim::protocol::Command::SendMessage { .. }
        ));

        let shutdown: crate::shim::protocol::Command = child.recv().unwrap().unwrap();
        match shutdown {
            crate::shim::protocol::Command::Shutdown { reason, .. } => {
                assert_eq!(reason, ShutdownReason::RestartHandoff);
            }
            other => panic!("expected Shutdown, got {other:?}"),
        }

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "shim_disconnect"
                && event.role.as_deref() == Some("manager")
                && event.reason.as_deref() == Some("restart_handoff")
                && event.success == Some(true)
        }));
    }

    #[test]
    fn check_working_state_timeouts_escalates_repeated_supervisory_stalls() {
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "batty", "fake-batty.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = if original_path.is_empty() {
            fake_bin.display().to_string()
        } else {
            format!("{}:{original_path}", fake_bin.display())
        };
        let _path_guard = EnvVarGuard::set("PATH", &combined_path);
        let _batty_guard = EnvVarGuard::set(
            "BATTY_BINARY_PATH",
            fake_bin.join("batty").to_string_lossy().as_ref(),
        );

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "architect").unwrap();
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1;
        daemon.config.team_config.workflow_policy.max_stall_restarts = 2;

        insert_mock_handle(&mut daemon, "manager");
        daemon
            .shim_handles
            .get_mut("manager")
            .unwrap()
            .apply_state_change(ShimState::Working);
        daemon
            .shim_handles
            .get_mut("manager")
            .unwrap()
            .state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        daemon.states.insert("manager".into(), MemberState::Working);

        crate::team::test_helpers::write_event_log(
            tmp.path(),
            &[
                crate::team::events::TeamEvent::agent_restarted(
                    "manager",
                    "supervisory::manager",
                    "supervisory_stalled",
                    1,
                ),
                crate::team::events::TeamEvent::agent_restarted(
                    "manager",
                    "supervisory::manager",
                    "supervisory_stalled",
                    2,
                ),
            ],
        );

        daemon.check_working_state_timeouts().unwrap();

        assert_eq!(daemon.states.get("manager"), Some(&MemberState::Working));
        assert_eq!(daemon.shim_handles["manager"].state, ShimState::Starting);
        assert!(
            daemon
                .intervention_cooldowns
                .contains_key("stall-escalation::manager")
        );

        let architect_messages = inbox::pending_messages(&inbox_root, "architect").unwrap();
        assert!(architect_messages.iter().any(|message| {
            message.body.contains("manager stalled repeatedly")
                || message
                    .body
                    .contains("manager stalled repeatedly while marked working")
        }));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_escalated"
                && event.role.as_deref() == Some("manager")
                && event.reason.as_deref() == Some("supervisory_stalled")
        }));
        assert!(events.iter().any(|event| {
            event.event == "agent_restarted"
                && event.role.as_deref() == Some("manager")
                && event.reason.as_deref() == Some("supervisory_stall_escalated")
        }));
    }

    #[test]
    fn check_working_state_timeouts_does_not_restart_recent_supervisory_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .build();
        daemon.config.team_config.shim_working_state_timeout_secs = 1800;

        for member_name in ["architect", "manager"] {
            insert_mock_handle(&mut daemon, member_name);
            daemon
                .shim_handles
                .get_mut(member_name)
                .unwrap()
                .apply_state_change(ShimState::Working);
            daemon
                .states
                .insert(member_name.to_string(), MemberState::Working);
        }

        daemon.check_working_state_timeouts().unwrap();

        for member_name in ["architect", "manager"] {
            assert_eq!(
                daemon.states.get(member_name),
                Some(&MemberState::Working),
                "recent supervisory work should not be restarted"
            );
            assert_eq!(daemon.shim_handles[member_name].state, ShimState::Working);
        }
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
        insert_mock_codex_handle(
            &mut daemon,
            "eng-1",
            "codex exec",
            std::path::PathBuf::from("/tmp/test"),
        );
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
        let _path_lock = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-cold-respawn-plan");
        std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.to_string_lossy().as_ref());

        let worktree_path = repo.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&worktree_path).unwrap();
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

        let handoff_path = worktree_path.join(crate::shim::runtime::HANDOFF_FILE_NAME);
        std::fs::write(
            &handoff_path,
            "# Carry-Forward Summary\n## Recent Activity\nedited src/team/daemon/health/poll_shim.rs\n",
        )
        .unwrap();

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
        assert!(identity.prompt.contains("Carry-Forward Summary"));
        assert!(
            identity
                .prompt
                .contains("edited src/team/daemon/health/poll_shim.rs")
        );
        assert!(
            !handoff_path.exists(),
            "cold respawn should consume the handoff file into the prompt"
        );
    }
}
