//! Topology reconciliation: apply a TopologyDiff to the running daemon state.
//!
//! When team.yaml changes and the daemon detects a topology diff, this module
//! spawns new agents for added members and gracefully shuts down removed ones.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use crate::shim::protocol::ShimState;
use crate::team::config::{RoleType, TeamConfig};
use crate::team::config_diff::TopologyDiff;
use crate::team::events::TeamEvent;
use crate::team::hierarchy::MemberInstance;
use crate::team::inbox;
use crate::team::standup::MemberState;
use crate::team::watcher::SessionWatcher;
use crate::tmux;

impl TeamDaemon {
    /// Apply a topology diff to the running daemon: spawn new agents, remove old ones,
    /// and update internal state.
    pub(super) fn reconcile_topology(
        &mut self,
        diff: TopologyDiff,
        new_config: TeamConfig,
        new_members: Vec<MemberInstance>,
    ) -> Result<()> {
        // Phase 1: Gracefully remove agents that are no longer in the topology
        for change in &diff.removed {
            if change.member.role_type == RoleType::User {
                continue;
            }
            self.remove_member(&change.name)?;
        }

        // Phase 2: Create tmux panes and spawn agents for added members
        for change in &diff.added {
            if change.member.role_type == RoleType::User {
                continue;
            }
            if let Err(e) = self.add_member(&change.member) {
                warn!(
                    member = change.name.as_str(),
                    error = %e,
                    "failed to add member during reconciliation"
                );
            }
        }

        // Phase 3: Update daemon config to reflect new topology
        self.config.team_config = new_config;
        self.config.members = new_members;

        info!(
            added = diff.added.len(),
            removed = diff.removed.len(),
            total_members = self.config.members.len(),
            "topology reconciliation complete"
        );

        Ok(())
    }

    /// Add a new member: create tmux pane, set up worktree/inbox, spawn shim.
    fn add_member(&mut self, member: &MemberInstance) -> Result<()> {
        info!(
            member = member.name.as_str(),
            "adding new member to topology"
        );

        // Create inbox
        let inboxes = inbox::inboxes_root(&self.config.project_root);
        if let Err(e) = inbox::init_inbox(&inboxes, &member.name) {
            warn!(member = member.name.as_str(), error = %e, "failed to init inbox");
        }

        // Create tmux pane by splitting an existing one in the session
        let pane_id = self.create_pane_for_member(member)?;
        self.config
            .pane_map
            .insert(member.name.clone(), pane_id.clone());

        // Create watcher for the new pane
        let stale_secs = self.config.team_config.standup.interval_secs * 2;
        self.watchers.insert(
            member.name.clone(),
            SessionWatcher::new(&pane_id, &member.name, stale_secs, None),
        );

        // Set initial state
        self.states.insert(member.name.clone(), MemberState::Idle);

        // Spawn shim subprocess if use_shim is enabled
        if self.config.team_config.use_shim {
            let work_dir = self.member_work_dir(member);
            let agent_name = member.agent.as_deref().unwrap_or("claude");
            // The shim --cmd expects the CLI command name (e.g. "claude", "codex")
            let agent_cmd = agent_name.to_string();

            let pty_log_path = crate::team::shim_log_path(&self.config.project_root, &member.name);
            let events_log_path =
                crate::team::shim_events_log_path(&self.config.project_root, &member.name);
            if let Some(parent) = pty_log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = crate::team::layout::respawn_as_display_pane(
                &pane_id,
                &self.config.project_root,
                &member.name,
                &events_log_path,
                &pty_log_path,
            ) {
                warn!(
                    member = member.name.as_str(),
                    pane = pane_id.as_str(),
                    error = %error,
                    "failed to respawn topology-added member pane as console pane"
                );
            }

            let sdk_mode = agent_name == "claude" && self.config.team_config.use_sdk_mode;
            match super::shim_spawn::spawn_shim(
                &member.name,
                agent_name,
                &agent_cmd,
                &work_dir,
                Some(&pty_log_path),
                sdk_mode,
            ) {
                Ok(handle) => {
                    self.shim_handles.insert(member.name.clone(), handle);
                    info!(member = member.name.as_str(), "shim spawned for new member");
                }
                Err(e) => {
                    warn!(
                        member = member.name.as_str(),
                        error = %e,
                        "failed to spawn shim for new member"
                    );
                }
            }
        }

        // Log event
        if let Err(e) = self.event_sink.emit(TeamEvent::agent_spawned(&member.name)) {
            warn!(error = %e, "failed to emit agent_spawned event");
        }

        self.record_orchestrator_action(format!("topology: added member {}", member.name));
        Ok(())
    }

    /// Remove a member: graceful shim shutdown, kill tmux pane, clean up state.
    fn remove_member(&mut self, name: &str) -> Result<()> {
        info!(member = name, "removing member from topology");

        // Check if agent is currently working
        let is_working = self
            .shim_handles
            .get(name)
            .is_some_and(|h| h.state == ShimState::Working);

        if is_working {
            info!(
                member = name,
                "agent is working — sending graceful shutdown with timeout"
            );
        }

        // Phase 1: Send graceful shutdown to shim handle
        let timeout_secs = self.config.team_config.shim_shutdown_timeout_secs;
        if let Some(handle) = self.shim_handles.get_mut(name) {
            if !handle.is_terminal() {
                if let Err(e) = handle.send_shutdown(timeout_secs) {
                    warn!(member = name, error = %e, "failed to send shutdown to shim");
                    let _ = handle.send_kill();
                }
            }
        }

        // Phase 2: Wait for shim to exit (with timeout)
        if let Some(handle) = self.shim_handles.get(name) {
            if !handle.is_terminal() {
                let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
                let pid = handle.child_pid;
                while Instant::now() < deadline {
                    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
                    if !alive {
                        debug!(member = name, pid, "shim process exited cleanly");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // Force kill if still alive
                let still_alive = unsafe { libc::kill(pid as i32, 0) } == 0;
                if still_alive {
                    warn!(
                        member = name,
                        pid, "shim did not exit in time — sending SIGKILL"
                    );
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        }

        // Phase 3: Remove shim handle
        self.shim_handles.remove(name);

        // Phase 4: Kill tmux pane
        if let Some(pane_id) = self.config.pane_map.remove(name) {
            if let Err(e) = tmux::kill_pane(&pane_id) {
                debug!(
                    member = name,
                    error = %e,
                    "failed to kill pane (may already be gone)"
                );
            }
        }

        // Phase 5: Clean up daemon state maps
        self.watchers.remove(name);
        self.states.remove(name);
        self.idle_started_at.remove(name);
        self.active_tasks.remove(name);
        self.retry_counts.remove(name);
        self.triage_idle_epochs.remove(name);
        self.triage_interventions.remove(name);
        self.owned_task_interventions.remove(name);
        self.intervention_cooldowns.remove(name);
        self.nudges.remove(name);
        self.last_standup.remove(name);
        self.backend_health.remove(name);
        self.last_uncommitted_warn.remove(name);
        self.pending_delivery_queue.remove(name);

        // Log event
        let reason = if is_working {
            "removed after graceful shutdown (was working)"
        } else {
            "removed (was idle)"
        };
        if let Err(e) = self.event_sink.emit(TeamEvent::agent_removed(name, reason)) {
            warn!(error = %e, "failed to emit agent_removed event");
        }

        self.record_orchestrator_action(format!("topology: removed member {name}"));
        Ok(())
    }

    /// Create a new tmux pane for a member by splitting an existing one.
    fn create_pane_for_member(&self, member: &MemberInstance) -> Result<String> {
        let target_pane = self.find_split_target(member);

        let pane_id = tmux::split_window_vertical_in_pane(&self.config.session, &target_pane, 50)
            .with_context(|| format!("failed to create pane for {}", member.name))?;

        // Label the pane with @batty_role
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", "-t", &pane_id, "-T", &member.name])
            .output();
        let _ = std::process::Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-t",
                &pane_id,
                "@batty_role",
                &member.name,
            ])
            .output();

        info!(
            member = member.name.as_str(),
            pane_id = pane_id.as_str(),
            "created tmux pane"
        );
        Ok(pane_id)
    }

    /// Find the best existing pane to split when adding a new member.
    /// Prefers splitting a pane that belongs to the same role type.
    fn find_split_target(&self, member: &MemberInstance) -> String {
        // Try to find a pane with the same role type
        for existing in &self.config.members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = self.config.pane_map.get(&existing.name) {
                    return pane_id.clone();
                }
            }
        }
        // Fall back to any non-user pane
        for existing in &self.config.members {
            if existing.role_type != RoleType::User {
                if let Some(pane_id) = self.config.pane_map.get(&existing.name) {
                    return pane_id.clone();
                }
            }
        }
        // Last resort: first pane in the map
        self.config
            .pane_map
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| "%0".to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::team::config::RoleType;
    use crate::team::config_diff::{MemberChange, TopologyDiff};
    use crate::team::hierarchy::MemberInstance;

    fn make_member(name: &str, role_type: RoleType) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: match role_type {
                RoleType::Engineer => "engineer".to_string(),
                RoleType::Manager => "manager".to_string(),
                RoleType::Architect => "architect".to_string(),
                RoleType::User => "user".to_string(),
            },
            role_type,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: role_type == RoleType::Engineer,
        }
    }

    #[test]
    fn empty_diff_is_noop() {
        let diff = TopologyDiff {
            added: vec![],
            removed: vec![],
            unchanged: vec!["architect".to_string()],
        };
        assert!(diff.is_empty());
        assert_eq!(diff.change_count(), 0);
    }

    #[test]
    fn diff_with_additions_counts_correctly() {
        let diff = TopologyDiff {
            added: vec![
                MemberChange {
                    name: "eng-1-3".to_string(),
                    member: make_member("eng-1-3", RoleType::Engineer),
                },
                MemberChange {
                    name: "eng-1-4".to_string(),
                    member: make_member("eng-1-4", RoleType::Engineer),
                },
            ],
            removed: vec![],
            unchanged: vec!["architect".to_string()],
        };
        assert!(!diff.is_empty());
        assert_eq!(diff.change_count(), 2);
    }

    #[test]
    fn diff_with_mixed_changes() {
        let diff = TopologyDiff {
            added: vec![MemberChange {
                name: "eng-1-5".to_string(),
                member: make_member("eng-1-5", RoleType::Engineer),
            }],
            removed: vec![MemberChange {
                name: "eng-1-1".to_string(),
                member: make_member("eng-1-1", RoleType::Engineer),
            }],
            unchanged: vec!["architect".to_string()],
        };
        assert!(!diff.is_empty());
        assert_eq!(diff.change_count(), 2);
    }

    #[test]
    fn find_split_target_prefers_same_role_type() {
        let member = make_member("eng-1-3", RoleType::Engineer);
        let members = vec![
            make_member("architect", RoleType::Architect),
            make_member("manager", RoleType::Manager),
            make_member("eng-1-1", RoleType::Engineer),
            make_member("eng-1-2", RoleType::Engineer),
        ];
        let mut pane_map = HashMap::new();
        pane_map.insert("architect".to_string(), "%0".to_string());
        pane_map.insert("manager".to_string(), "%1".to_string());
        pane_map.insert("eng-1-1".to_string(), "%2".to_string());
        pane_map.insert("eng-1-2".to_string(), "%3".to_string());

        // Simulate the find_split_target logic
        let mut target = None;
        for existing in &members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = pane_map.get(&existing.name) {
                    target = Some(pane_id.clone());
                    break;
                }
            }
        }
        assert_eq!(target, Some("%2".to_string()));
    }

    #[test]
    fn find_split_target_falls_back_to_any_non_user() {
        let member = make_member("eng-1-1", RoleType::Engineer);
        let members = vec![
            make_member("user", RoleType::User),
            make_member("architect", RoleType::Architect),
        ];
        let mut pane_map = HashMap::new();
        pane_map.insert("architect".to_string(), "%0".to_string());

        // No engineer panes exist, should fall back to architect
        let mut target = None;
        for existing in &members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = pane_map.get(&existing.name) {
                    target = Some(pane_id.clone());
                    break;
                }
            }
        }
        if target.is_none() {
            for existing in &members {
                if existing.role_type != RoleType::User {
                    if let Some(pane_id) = pane_map.get(&existing.name) {
                        target = Some(pane_id.clone());
                        break;
                    }
                }
            }
        }
        assert_eq!(target, Some("%0".to_string()));
    }
}
