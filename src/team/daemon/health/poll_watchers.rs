//! Watcher polling: state transitions, stall detection, completion gating.

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::*;

impl TeamDaemon {
    /// Poll all watchers and handle state transitions.
    #[allow(dead_code)]
    pub(in super::super) fn poll_watchers(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.watchers.keys().cloned().collect();

        for name in &member_names {
            let prev_state = self.states.get(name).copied();
            let prev_watcher_state = self
                .watchers
                .get(name)
                .map(|watcher| watcher.state)
                .unwrap_or(WatcherState::Idle);
            let (new_state, completion_observed, session_size_bytes, secs_since_output) = {
                let watcher = match self.watcher_mut(name) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        warn!(member = %name, error = %error, "watcher missing during poll");
                        continue;
                    }
                };
                match watcher.poll() {
                    Ok(new_state) => (
                        new_state,
                        watcher.take_completion_event(),
                        watcher.current_session_size_bytes(),
                        watcher.secs_since_last_output_change(),
                    ),
                    Err(e) => {
                        warn!(member = %name, error = %e, "watcher poll failed");
                        continue;
                    }
                }
            };

            let member_state = match new_state {
                WatcherState::Active => MemberState::Working,
                WatcherState::Ready | WatcherState::Idle => MemberState::Idle,
                WatcherState::PaneDead => MemberState::Idle,
                WatcherState::ContextExhausted => MemberState::Working,
            };

            // Record agent poll state for telemetry (idle_polls, working_polls, total_cycle_secs).
            if let Some(conn) = &self.telemetry_db {
                let is_working = member_state == MemberState::Working;
                let poll_secs = self.poll_interval.as_secs();
                if let Err(error) = crate::team::telemetry_db::record_agent_poll_state(
                    conn, name, is_working, poll_secs,
                ) {
                    debug!(error = %error, member = %name, "failed to record agent poll state");
                }
            }

            if prev_state != Some(member_state) {
                self.states.insert(name.clone(), member_state);

                // Update automation countdowns on state transitions.
                self.update_automation_timers_for_state(name, member_state);
            }

            // When an agent transitions to Ready for the first time, drain
            // any messages that were buffered while it was still starting.
            if new_state == WatcherState::Ready
                && prev_watcher_state != WatcherState::Ready
                && self.pending_delivery_queue.contains_key(name)
            {
                if let Err(error) = self.drain_pending_queue(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "failed to drain pending delivery queue on readiness"
                    );
                }
            }

            if new_state == WatcherState::PaneDead {
                if prev_watcher_state != WatcherState::PaneDead {
                    warn!(member = %name, "detected pane death");
                    self.emit_event(TeamEvent::pane_death(name));
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected pane death for {}",
                        name
                    ));
                }
                if let Err(error) = self.handle_pane_death(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "pane-death respawn handling failed; continuing"
                    );
                }
                continue;
            }

            if new_state == WatcherState::ContextExhausted {
                if prev_watcher_state != WatcherState::ContextExhausted {
                    let task_id = self.active_task_id(name);
                    warn!(
                        member = %name,
                        task_id,
                        session_size_bytes,
                        "detected context exhaustion"
                    );
                    self.record_context_exhausted(name, task_id, session_size_bytes);
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected context exhaustion for {} (task={:?}, session_size_bytes={:?})",
                        name, task_id, session_size_bytes
                    ));
                }
                if let Err(error) = self.handle_context_exhaustion(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "context-exhausted restart handling failed; continuing"
                    );
                }
                continue;
            }

            // Stall detection: active agent with no output change beyond threshold.
            if new_state == WatcherState::Active && self.active_task_id(name).is_some() {
                let threshold = self.config.team_config.workflow_policy.stall_threshold_secs;
                if secs_since_output >= threshold {
                    if let Err(error) = self.handle_stalled_agent(name, secs_since_output) {
                        warn!(
                            member = %name,
                            error = %error,
                            "stall restart handling failed; continuing"
                        );
                    }
                    continue;
                }
            }

            if completion_observed && self.active_task_id(name).is_some() {
                // False-done prevention: verify the engineer's branch has commits
                // beyond trunk before triggering completion. Without this check,
                // idle engineers with no commits get marked as complete, orphaning
                // their board task.
                if self.member_uses_worktrees(name) {
                    let worktree_dir = self.worktree_dir(name);
                    let range = format!("{}..HEAD", self.config.team_config.trunk_branch());
                    match crate::team::git_cmd::run_git(
                        &worktree_dir,
                        &["rev-list", "--count", &range],
                    ) {
                        Ok(output) => {
                            let count = output.stdout.trim().parse::<u32>().unwrap_or(0);
                            if count == 0 {
                                warn!(
                                    member = %name,
                                    "engineer idle but no commits on task branch — skipping completion"
                                );
                                self.record_orchestrator_action(format!(
                                    "false-done prevention: {} reported completion but branch has no commits beyond {}",
                                    name,
                                    self.config.team_config.trunk_branch()
                                ));
                                continue;
                            }
                        }
                        Err(error) => {
                            warn!(
                                member = %name,
                                error = %error,
                                "failed to check commits on task branch — skipping completion"
                            );
                            continue;
                        }
                    }
                }

                info!(member = %name, "detected task completion");
                if let Err(error) = merge::handle_engineer_completion(self, name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "engineer completion handling failed; continuing"
                    );
                }
            }
        }

        Ok(())
    }
}
