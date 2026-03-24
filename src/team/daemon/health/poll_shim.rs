//! Shim event polling: reads events from shim channels, updates AgentHandle
//! state, and triggers existing daemon flows (completion, context exhaustion,
//! pane death, health monitoring, and stale detection).

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::*;
use crate::shim::protocol::{Event, ShimState};

impl TeamDaemon {
    /// Poll all shim handles for events and process state transitions.
    ///
    /// Called from the main poll loop when `use_shim` is enabled. This is the
    /// shim equivalent of `poll_watchers()`.
    pub(in crate::team) fn poll_shim_handles(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.shim_handles.keys().cloned().collect();

        for name in &member_names {
            // Try to receive an event from the shim (non-blocking).
            // We need to extract the handle, process events, then put it back.
            let event = {
                let Some(handle) = self.shim_handles.get_mut(name) else {
                    continue;
                };
                match handle.try_recv_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => continue, // EOF — shim disconnected
                    Err(error) => {
                        debug!(
                            member = name.as_str(),
                            error = %error,
                            "shim channel recv error"
                        );
                        continue;
                    }
                }
            };

            self.handle_shim_event(name, event)?;
        }

        Ok(())
    }

    /// Process a single shim event for a named agent.
    fn handle_shim_event(&mut self, member_name: &str, event: Event) -> Result<()> {
        match event {
            Event::Ready => {
                info!(member = member_name, "shim agent ready");
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(ShimState::Idle);
                }
                self.states
                    .insert(member_name.to_string(), MemberState::Idle);
                self.update_automation_timers_for_state(member_name, MemberState::Idle);

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
            }

            Event::StateChanged { from, to, summary } => {
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
                    if let Err(error) = self.handle_shim_crash_respawn(member_name) {
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
                debug!(member = member_name, "shim pong received");
                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.record_pong();
                }
            }

            Event::Warning {
                message,
                idle_secs,
            } => {
                info!(
                    member = member_name,
                    idle_secs,
                    message = message.as_str(),
                    "shim warning: potential stall"
                );
                self.surface_shim_stall_warning(member_name, &message, idle_secs);
            }

            Event::ScreenCapture { .. } | Event::State { .. } | Event::Error { .. } => {
                debug!(member = member_name, event = ?event, "shim event (unhandled in poll)");
            }
        }

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

        info!(
            member = member_name,
            "auto-respawning shim after crash"
        );

        let new_handle = super::super::shim_spawn::spawn_shim(
            member_name,
            &agent_type,
            &agent_cmd,
            &work_dir,
            None,
        )?;
        self.shim_handles.insert(member_name.to_string(), new_handle);
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
        let body = format!(
            "Shim warning for {}{}: {}",
            member_name, idle_info, message
        );

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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{TestDaemonBuilder, engineer_member, manager_member};

    fn insert_mock_handle(daemon: &mut TeamDaemon, name: &str) -> crate::shim::protocol::Channel {
        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
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
            messages
                .iter()
                .any(|msg| msg.body.contains("Shim warning")),
            "manager should receive stall warning"
        );
    }
}
