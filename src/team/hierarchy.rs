//! Instance naming and manager↔engineer partitioning.
//!
//! With `instances: N`, the daemon creates named instances:
//! - `architect-1` (just 1)
//! - `manager-1`, `manager-2`, `manager-3`
//! - Engineers partitioned across managers: `eng-1-1..eng-1-5` (under manager-1), etc.

use anyhow::{Result, bail};

use super::config::{RoleType, TeamConfig};

/// A resolved team member instance with its name, role, and hierarchy position.
#[derive(Debug, Clone)]
pub struct MemberInstance {
    /// Unique instance name (e.g., "architect-1", "manager-2", "eng-1-3").
    pub name: String,
    /// The role definition name from team.yaml.
    pub role_name: String,
    /// The role type.
    pub role_type: RoleType,
    /// Agent to use (None for user roles).
    pub agent: Option<String>,
    /// Prompt template filename (relative to team_config dir).
    pub prompt: Option<String>,
    /// Instance name this member reports to (None for top-level/user roles).
    pub reports_to: Option<String>,
    /// Whether this member uses git worktrees.
    pub use_worktrees: bool,
}

/// Resolve the team hierarchy into a flat list of member instances.
///
/// Engineer instances are multiplicative: each manager gets `engineer.instances`
/// engineers assigned to it. Total engineers = manager.instances × engineer.instances.
pub fn resolve_hierarchy(config: &TeamConfig) -> Result<Vec<MemberInstance>> {
    let mut members = Vec::new();

    // Collect role defs by type for hierarchy resolution
    let managers: Vec<_> = config
        .roles
        .iter()
        .filter(|r| r.role_type == RoleType::Manager)
        .collect();
    let engineers: Vec<_> = config
        .roles
        .iter()
        .filter(|r| r.role_type == RoleType::Engineer)
        .collect();

    // Phase 1: Add user roles (no pane, no instances beyond routing)
    for role in config
        .roles
        .iter()
        .filter(|r| r.role_type == RoleType::User)
    {
        members.push(MemberInstance {
            name: role.name.clone(),
            role_name: role.name.clone(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        });
    }

    // Phase 2: Add architect instances
    for role in config
        .roles
        .iter()
        .filter(|r| r.role_type == RoleType::Architect)
    {
        for i in 1..=role.instances {
            let name = if role.instances == 1 {
                role.name.clone()
            } else {
                format!("{}-{i}", role.name)
            };
            members.push(MemberInstance {
                name,
                role_name: role.name.clone(),
                role_type: RoleType::Architect,
                agent: role.agent.clone(),
                prompt: role.prompt.clone(),
                reports_to: None,
                use_worktrees: role.use_worktrees,
            });
        }
    }

    // Phase 3: Add manager instances
    let mut manager_names = Vec::new();
    for role in &managers {
        for i in 1..=role.instances {
            let name = if role.instances == 1 {
                role.name.clone()
            } else {
                format!("{}-{i}", role.name)
            };
            manager_names.push(name.clone());

            // Find architect to report to (first architect role, instance 1)
            let reports_to = config
                .roles
                .iter()
                .find(|r| r.role_type == RoleType::Architect)
                .map(|a| {
                    if a.instances == 1 {
                        a.name.clone()
                    } else {
                        format!("{}-1", a.name)
                    }
                });

            members.push(MemberInstance {
                name,
                role_name: role.name.clone(),
                role_type: RoleType::Manager,
                agent: role.agent.clone(),
                prompt: role.prompt.clone(),
                reports_to,
                use_worktrees: role.use_worktrees,
            });
        }
    }

    // Phase 4: Add engineer instances, partitioned across managers
    for role in &engineers {
        if manager_names.is_empty() {
            // Engineers without managers report to nobody (flat team)
            for i in 1..=role.instances {
                let name = if role.instances == 1 {
                    role.name.clone()
                } else {
                    format!("{}-{i}", role.name)
                };
                members.push(MemberInstance {
                    name,
                    role_name: role.name.clone(),
                    role_type: RoleType::Engineer,
                    agent: role.agent.clone(),
                    prompt: role.prompt.clone(),
                    reports_to: None,
                    use_worktrees: role.use_worktrees,
                });
            }
        } else {
            // Multiplicative: each manager gets `instances` engineers
            for (mgr_idx, mgr_name) in manager_names.iter().enumerate() {
                for eng_idx in 1..=role.instances {
                    let name = format!("eng-{}-{eng_idx}", mgr_idx + 1);
                    members.push(MemberInstance {
                        name,
                        role_name: role.name.clone(),
                        role_type: RoleType::Engineer,
                        agent: role.agent.clone(),
                        prompt: role.prompt.clone(),
                        reports_to: Some(mgr_name.clone()),
                        use_worktrees: role.use_worktrees,
                    });
                }
            }
        }
    }

    if members
        .iter()
        .filter(|m| m.role_type != RoleType::User)
        .count()
        == 0
    {
        bail!("team has no agent members (only user roles)");
    }

    Ok(members)
}

/// Count total panes needed (excludes user roles which have no pane).
pub fn pane_count(members: &[MemberInstance]) -> usize {
    members
        .iter()
        .filter(|m| m.role_type != RoleType::User)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(yaml: &str) -> TeamConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn simple_team_3_engineers() {
        let config = make_config(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        // 1 architect + 1 manager + 3 engineers = 5
        assert_eq!(members.len(), 5);
        assert_eq!(pane_count(&members), 5);

        let engineers: Vec<_> = members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .collect();
        assert_eq!(engineers.len(), 3);
        assert_eq!(engineers[0].name, "eng-1-1");
        assert_eq!(engineers[1].name, "eng-1-2");
        assert_eq!(engineers[2].name, "eng-1-3");
        // All report to manager
        assert_eq!(engineers[0].reports_to.as_deref(), Some("manager"));
    }

    #[test]
    fn large_team_multiplicative() {
        let config = make_config(
            r#"
name: large
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
  - name: manager
    role_type: manager
    agent: claude
    instances: 3
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 5
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        // 1 architect + 3 managers + 15 engineers = 19
        assert_eq!(members.len(), 19);
        assert_eq!(pane_count(&members), 19);

        let engineers: Vec<_> = members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .collect();
        assert_eq!(engineers.len(), 15);
        // First manager's engineers
        assert_eq!(engineers[0].name, "eng-1-1");
        assert_eq!(engineers[0].reports_to.as_deref(), Some("manager-1"));
        assert_eq!(engineers[4].name, "eng-1-5");
        // Second manager's engineers
        assert_eq!(engineers[5].name, "eng-2-1");
        assert_eq!(engineers[5].reports_to.as_deref(), Some("manager-2"));
        // Third manager's engineers
        assert_eq!(engineers[10].name, "eng-3-1");
        assert_eq!(engineers[10].reports_to.as_deref(), Some("manager-3"));
    }

    #[test]
    fn user_role_excluded_from_pane_count() {
        let config = make_config(
            r#"
name: with-user
roles:
  - name: human
    role_type: user
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(pane_count(&members), 1);
    }

    #[test]
    fn manager_reports_to_architect() {
        let config = make_config(
            r#"
name: test
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: mgr
    role_type: manager
    agent: claude
    instances: 2
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        let mgr1 = members.iter().find(|m| m.name == "mgr-1").unwrap();
        assert_eq!(mgr1.reports_to.as_deref(), Some("arch"));
    }

    #[test]
    fn single_instance_no_number_suffix() {
        let config = make_config(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        assert_eq!(members[0].name, "architect");
    }

    #[test]
    fn multi_instance_has_number_suffix() {
        let config = make_config(
            r#"
name: test
roles:
  - name: manager
    role_type: manager
    agent: claude
    instances: 2
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        assert_eq!(members[0].name, "manager-1");
        assert_eq!(members[1].name, "manager-2");
    }

    #[test]
    fn engineers_without_managers_report_to_nobody() {
        let config = make_config(
            r#"
name: flat
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: 3
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        assert_eq!(members.len(), 3);
        for m in &members {
            assert!(m.reports_to.is_none());
        }
        assert_eq!(members[0].name, "worker-1");
    }

    #[test]
    fn rejects_user_only_team() {
        let config = make_config(
            r#"
name: empty
roles:
  - name: human
    role_type: user
"#,
        );
        let err = resolve_hierarchy(&config).unwrap_err().to_string();
        assert!(err.contains("no agent members"));
    }
}
