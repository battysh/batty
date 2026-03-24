//! Shim event polling: reads events from shim channels, updates AgentHandle
//! state, and triggers existing daemon flows (completion, context exhaustion,
//! pane death).

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

                if let Err(error) = self.handle_pane_death(member_name) {
                    warn!(
                        member = member_name,
                        error = %error,
                        "shim death respawn handling failed"
                    );
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

                if let Some(handle) = self.shim_handles.get_mut(member_name) {
                    handle.apply_state_change(ShimState::ContextExhausted);
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
            }

            Event::ScreenCapture { .. } | Event::State { .. } | Event::Error { .. } => {
                debug!(member = member_name, event = ?event, "shim event (unhandled in poll)");
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::TestDaemonBuilder;

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

        // Insert a mock handle
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        daemon.handle_shim_event("eng-1", Event::Ready).unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
        assert!(daemon.shim_handles["eng-1"].is_ready());
    }

    #[test]
    fn handle_shim_event_state_changed_to_working() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

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

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

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

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

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
    fn handle_shim_event_context_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

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
    fn handle_shim_event_pong_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle =
            crate::team::daemon::agent_handle::AgentHandle::new("eng-1".into(), channel, 999, "claude".into(), "claude".into(), std::path::PathBuf::from("/tmp/test"));
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        // Should not panic or error
        daemon.handle_shim_event("eng-1", Event::Pong).unwrap();
    }
}
