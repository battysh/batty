//! Diff two team configurations to produce a topology change set.
//!
//! Used by both `batty scale` (CLI-side) and daemon hot-reload to determine
//! which agents need to be added, removed, or left unchanged.

use std::collections::HashSet;

use super::config::TeamConfig;
use super::hierarchy::{self, MemberInstance};

/// A single member that changed between two configurations.
#[derive(Debug, Clone)]
pub struct MemberChange {
    pub name: String,
    pub member: MemberInstance,
}

/// The result of diffing two resolved team topologies.
#[derive(Debug, Clone)]
pub struct TopologyDiff {
    /// Members present in new config but not in old.
    pub added: Vec<MemberChange>,
    /// Members present in old config but not in new.
    pub removed: Vec<MemberChange>,
    /// Members present in both configs (by name).
    pub unchanged: Vec<String>,
}

impl TopologyDiff {
    /// True when no members were added or removed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// Total number of changes.
    pub fn change_count(&self) -> usize {
        self.added.len() + self.removed.len()
    }
}

/// Compute the topology diff between two team configurations.
///
/// Resolves both configs into member instance lists and compares by name.
pub fn diff_configs(old: &TeamConfig, new: &TeamConfig) -> anyhow::Result<TopologyDiff> {
    let old_members = hierarchy::resolve_hierarchy(old)?;
    let new_members = hierarchy::resolve_hierarchy(new)?;
    Ok(diff_members(&old_members, &new_members))
}

/// Compute the topology diff between two resolved member lists.
pub fn diff_members(old: &[MemberInstance], new: &[MemberInstance]) -> TopologyDiff {
    let old_names: HashSet<&str> = old.iter().map(|m| m.name.as_str()).collect();
    let new_names: HashSet<&str> = new.iter().map(|m| m.name.as_str()).collect();

    let added: Vec<MemberChange> = new
        .iter()
        .filter(|m| !old_names.contains(m.name.as_str()))
        .map(|m| MemberChange {
            name: m.name.clone(),
            member: m.clone(),
        })
        .collect();

    let removed: Vec<MemberChange> = old
        .iter()
        .filter(|m| !new_names.contains(m.name.as_str()))
        .map(|m| MemberChange {
            name: m.name.clone(),
            member: m.clone(),
        })
        .collect();

    let unchanged: Vec<String> = old
        .iter()
        .filter(|m| new_names.contains(m.name.as_str()))
        .map(|m| m.name.clone())
        .collect();

    TopologyDiff {
        added,
        removed,
        unchanged,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{RoleDef, RoleType};

    fn minimal_config(engineer_instances: u32, manager_instances: u32) -> TeamConfig {
        TeamConfig {
            name: "test".into(),
            agent: None,
            workflow_mode: Default::default(),
            board: Default::default(),
            standup: Default::default(),
            automation: Default::default(),
            automation_sender: None,
            external_senders: vec![],
            orchestrator_pane: false,
            orchestrator_position: Default::default(),
            layout: None,
            workflow_policy: Default::default(),
            cost: Default::default(),
            grafana: Default::default(),
            use_shim: true,
            auto_respawn_on_crash: false,
            shim_health_check_interval_secs: 60,
            shim_health_timeout_secs: 120,
            shim_shutdown_timeout_secs: 30,
            shim_working_state_timeout_secs: 1800,
            pending_queue_max_age_secs: 600,
            event_log_max_bytes: 10 * 1024 * 1024,
            retro_min_duration_secs: 60,
            roles: vec![
                RoleDef {
                    name: "architect".into(),
                    role_type: RoleType::Architect,
                    agent: None,
                    instances: 1,
                    prompt: None,
                    talks_to: vec![],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: vec![],
                    use_worktrees: false,
                },
                RoleDef {
                    name: "manager".into(),
                    role_type: RoleType::Manager,
                    agent: None,
                    instances: manager_instances,
                    prompt: None,
                    talks_to: vec![],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: vec![],
                    use_worktrees: false,
                },
                RoleDef {
                    name: "engineer".into(),
                    role_type: RoleType::Engineer,
                    agent: None,
                    instances: engineer_instances,
                    prompt: None,
                    talks_to: vec![],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: vec![],
                    use_worktrees: true,
                },
            ],
        }
    }

    #[test]
    fn no_change_produces_empty_diff() {
        let config = minimal_config(3, 1);
        let diff = diff_configs(&config, &config).unwrap();
        assert!(diff.is_empty());
        assert_eq!(diff.change_count(), 0);
        // architect + manager + 3 engineers = 5 unchanged
        assert_eq!(diff.unchanged.len(), 5);
    }

    #[test]
    fn scale_up_engineers_shows_added() {
        let old = minimal_config(2, 1);
        let new = minimal_config(4, 1);
        let diff = diff_configs(&old, &new).unwrap();
        assert_eq!(diff.added.len(), 2);
        assert!(diff.removed.is_empty());
        // Check the added names
        let added_names: HashSet<&str> = diff.added.iter().map(|c| c.name.as_str()).collect();
        assert!(added_names.contains("eng-1-3"));
        assert!(added_names.contains("eng-1-4"));
    }

    #[test]
    fn scale_down_engineers_shows_removed() {
        let old = minimal_config(4, 1);
        let new = minimal_config(2, 1);
        let diff = diff_configs(&old, &new).unwrap();
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed.len(), 2);
        let removed_names: HashSet<&str> = diff.removed.iter().map(|c| c.name.as_str()).collect();
        assert!(removed_names.contains("eng-1-3"));
        assert!(removed_names.contains("eng-1-4"));
    }

    #[test]
    fn add_manager_shows_added_manager_and_engineers() {
        let old = minimal_config(2, 1);
        let new = minimal_config(2, 2);
        let diff = diff_configs(&old, &new).unwrap();
        // Adding a second manager creates manager-2 + eng-2-1, eng-2-2
        // And renames architect→architect, manager→manager-1, manager-2 new
        // Actually hierarchy naming: with instances=2, managers become manager-1, manager-2
        // With instances=1, manager stays as "manager"
        // So old has: architect, manager, eng-1-1, eng-1-2
        // New has: architect, manager-1, manager-2, eng-1-1, eng-1-2, eng-2-1, eng-2-2
        // Diff: removed manager, added manager-1, manager-2, eng-2-1, eng-2-2
        assert!(!diff.added.is_empty());
        let added_names: HashSet<&str> = diff.added.iter().map(|c| c.name.as_str()).collect();
        assert!(added_names.contains("manager-2"));
    }

    #[test]
    fn diff_members_direct() {
        let old = vec![
            MemberInstance {
                name: "architect".into(),
                role_name: "architect".into(),
                role_type: RoleType::Architect,
                agent: Some("claude".into()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1-1".into(),
                role_name: "engineer".into(),
                role_type: RoleType::Engineer,
                agent: Some("claude".into()),
                prompt: None,
                reports_to: Some("manager".into()),
                use_worktrees: true,
            },
        ];
        let new = vec![
            MemberInstance {
                name: "architect".into(),
                role_name: "architect".into(),
                role_type: RoleType::Architect,
                agent: Some("claude".into()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1-1".into(),
                role_name: "engineer".into(),
                role_type: RoleType::Engineer,
                agent: Some("claude".into()),
                prompt: None,
                reports_to: Some("manager".into()),
                use_worktrees: true,
            },
            MemberInstance {
                name: "eng-1-2".into(),
                role_name: "engineer".into(),
                role_type: RoleType::Engineer,
                agent: Some("claude".into()),
                prompt: None,
                reports_to: Some("manager".into()),
                use_worktrees: true,
            },
        ];
        let diff = diff_members(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "eng-1-2");
        assert!(diff.removed.is_empty());
        assert_eq!(diff.unchanged.len(), 2);
    }
}
