//! Instance naming and manager↔engineer partitioning.
//!
//! With `instances: N`, the daemon creates named instances:
//! - `architect-1` (just 1)
//! - `manager-1`, `manager-2`, `manager-3`
//! - Engineers partitioned across compatible managers: `eng-1-1..eng-1-5`
//!   (under manager-1), etc.

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
/// Engineer instances are multiplicative across compatible managers: each
/// compatible manager gets `engineer.instances` engineers assigned to it.
///
/// Compatibility rule:
/// - if an engineer role's `talks_to` lists specific manager role names, only
///   those manager instances receive engineers from that role
/// - otherwise, the engineer role is assigned across all managers
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
        let resolved_agent = config.resolve_agent(role);
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
                agent: resolved_agent.clone(),
                prompt: role.prompt.clone(),
                reports_to: None,
                use_worktrees: role.use_worktrees,
            });
        }
    }

    // Phase 3: Add manager instances
    let mut manager_instances = Vec::new();
    for role in &managers {
        let resolved_agent = config.resolve_agent(role);
        for i in 1..=role.instances {
            let name = if role.instances == 1 {
                role.name.clone()
            } else {
                format!("{}-{i}", role.name)
            };
            manager_instances.push((name.clone(), role.name.clone()));

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
                agent: resolved_agent.clone(),
                prompt: role.prompt.clone(),
                reports_to,
                use_worktrees: role.use_worktrees,
            });
        }
    }

    let multiple_engineer_roles = engineers.len() > 1;

    // Phase 4: Add engineer instances, partitioned across compatible managers
    for role in &engineers {
        let resolved_agent = config.resolve_agent(role);
        let compatible_managers: Vec<_> = if manager_instances.is_empty() {
            Vec::new()
        } else if role.talks_to.is_empty() {
            manager_instances.iter().collect()
        } else {
            manager_instances
                .iter()
                .filter(|(member_name, role_name)| {
                    role.talks_to
                        .iter()
                        .any(|target| target == role_name || target == member_name)
                })
                .collect()
        };

        if compatible_managers.is_empty() {
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
                    agent: resolved_agent.clone(),
                    prompt: role.prompt.clone(),
                    reports_to: None,
                    use_worktrees: role.use_worktrees,
                });
            }
        } else {
            // Multiplicative: each compatible manager gets `instances` engineers
            for (mgr_idx, (mgr_name, _mgr_role_name)) in compatible_managers.iter().enumerate() {
                for eng_idx in 1..=role.instances {
                    let name = engineer_instance_name(
                        role.name.as_str(),
                        multiple_engineer_roles,
                        mgr_idx + 1,
                        eng_idx,
                    );
                    members.push(MemberInstance {
                        name,
                        role_name: role.name.clone(),
                        role_type: RoleType::Engineer,
                        agent: resolved_agent.clone(),
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

fn engineer_instance_name(
    role_name: &str,
    multiple_engineer_roles: bool,
    manager_index: usize,
    engineer_index: u32,
) -> String {
    if !multiple_engineer_roles && role_name == "engineer" {
        format!("eng-{manager_index}-{engineer_index}")
    } else {
        format!("{role_name}-{manager_index}-{engineer_index}")
    }
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

    #[test]
    fn engineer_roles_can_target_specific_manager_roles() {
        let config = make_config(
            r#"
name: split-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: black-lead
    role_type: manager
    agent: claude
    talks_to: [architect, black-eng]
  - name: red-lead
    role_type: manager
    agent: claude
    talks_to: [architect, red-eng]
  - name: black-eng
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [black-lead]
  - name: red-eng
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [red-lead]
"#,
        );

        let members = resolve_hierarchy(&config).unwrap();
        let engineers: Vec<_> = members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .collect();

        assert_eq!(engineers.len(), 6);
        assert_eq!(
            engineers
                .iter()
                .filter(|m| m.role_name == "black-eng")
                .count(),
            3
        );
        assert_eq!(
            engineers
                .iter()
                .filter(|m| m.role_name == "red-eng")
                .count(),
            3
        );
        assert!(engineers.iter().all(|m| {
            if m.role_name == "black-eng" {
                m.reports_to.as_deref() == Some("black-lead")
            } else {
                m.reports_to.as_deref() == Some("red-lead")
            }
        }));

        let unique_names: std::collections::HashSet<_> =
            engineers.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(unique_names.len(), engineers.len());
        assert!(unique_names.contains("black-eng-1-1"));
        assert!(unique_names.contains("red-eng-1-1"));
    }

    #[test]
    fn engineer_role_without_matching_manager_talks_to_stays_flat() {
        let config = make_config(
            r#"
name: unmatched
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: specialist
    role_type: engineer
    agent: codex
    instances: 2
    talks_to: [architect]
"#,
        );

        let members = resolve_hierarchy(&config).unwrap();
        let engineers: Vec<_> = members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .collect();

        assert_eq!(engineers.len(), 2);
        assert!(engineers.iter().all(|m| m.reports_to.is_none()));
        assert_eq!(engineers[0].name, "specialist-1");
        assert_eq!(engineers[1].name, "specialist-2");
    }

    #[test]
    fn team_level_agent_propagates_to_members() {
        let config = make_config(
            r#"
name: team-default
agent: codex
roles:
  - name: architect
    role_type: architect
  - name: manager
    role_type: manager
  - name: engineer
    role_type: engineer
    instances: 2
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        // All non-user members should have the team default agent
        for m in &members {
            assert_eq!(
                m.agent.as_deref(),
                Some("codex"),
                "member {} should have team default agent 'codex'",
                m.name
            );
        }
    }

    #[test]
    fn role_agent_overrides_team_default() {
        let config = make_config(
            r#"
name: mixed
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
  - name: engineer
    role_type: engineer
    instances: 2
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        let architect = members.iter().find(|m| m.name == "architect").unwrap();
        assert_eq!(
            architect.agent.as_deref(),
            Some("claude"),
            "architect should use role-level override"
        );
        let manager = members.iter().find(|m| m.name == "manager").unwrap();
        assert_eq!(
            manager.agent.as_deref(),
            Some("codex"),
            "manager should use team default"
        );
    }

    #[test]
    fn mixed_backend_engineers_under_same_manager() {
        let config = make_config(
            r#"
name: mixed-eng
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: claude-eng
    role_type: engineer
    agent: claude
    instances: 2
    talks_to: [manager]
  - name: codex-eng
    role_type: engineer
    instances: 2
    talks_to: [manager]
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        let claude_engs: Vec<_> = members
            .iter()
            .filter(|m| m.role_name == "claude-eng")
            .collect();
        let codex_engs: Vec<_> = members
            .iter()
            .filter(|m| m.role_name == "codex-eng")
            .collect();

        assert_eq!(claude_engs.len(), 2);
        assert_eq!(codex_engs.len(), 2);

        for m in &claude_engs {
            assert_eq!(m.agent.as_deref(), Some("claude"));
            assert_eq!(m.reports_to.as_deref(), Some("manager"));
        }
        for m in &codex_engs {
            assert_eq!(m.agent.as_deref(), Some("codex"));
            assert_eq!(m.reports_to.as_deref(), Some("manager"));
        }
    }

    #[test]
    fn no_team_agent_defaults_to_claude() {
        let config = make_config(
            r#"
name: default-fallback
roles:
  - name: worker
    role_type: engineer
    agent: claude
    instances: 1
"#,
        );
        let members = resolve_hierarchy(&config).unwrap();
        assert_eq!(members[0].agent.as_deref(), Some("claude"));
    }
}
