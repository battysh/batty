#![cfg_attr(not(test), allow(dead_code))]

//! Workflow capability resolution for topology-independent team behavior.
//!
//! The rules in this module intentionally resolve responsibilities from
//! `RoleType` plus hierarchy position rather than from literal role names.
//! That keeps the workflow model stable across:
//! - default architect/manager/engineer teams
//! - renamed roles such as tech leads and developers
//! - reduced topologies such as solo and pair setups
//!
//! Fallback rules:
//! - `Dispatcher` belongs to managers by default, falls back to architects
//!   when no manager layer exists, and finally to a lone top-level engineer.
//! - `Reviewer` belongs to supervisory agents by default. If the topology has
//!   no non-executor reviewer, the operator becomes the review fallback.
//! - `Orchestrator` and `Operator` are control-plane capabilities and are kept
//!   separate from agent member responsibilities.

use std::collections::{BTreeMap, BTreeSet};

use super::config::RoleType;
use super::hierarchy::MemberInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkflowCapability {
    Planner,
    Dispatcher,
    Executor,
    Reviewer,
    Orchestrator,
    Operator,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilitySubject {
    Member(String),
    Orchestrator,
    Operator,
}

pub type CapabilitySet = BTreeSet<WorkflowCapability>;
pub type CapabilityMap = BTreeMap<CapabilitySubject, CapabilitySet>;

/// Resolve the workflow capabilities for a single team member.
///
/// This function only returns agent-side capabilities. Operator and
/// orchestrator responsibilities are exposed by `resolve_capability_map`.
pub fn resolve_member_capabilities(
    member: &MemberInstance,
    members: &[MemberInstance],
) -> CapabilitySet {
    let has_architect = members
        .iter()
        .any(|candidate| candidate.role_type == RoleType::Architect);
    let has_manager = members
        .iter()
        .any(|candidate| candidate.role_type == RoleType::Manager);
    let is_top_level = member.reports_to.is_none();

    let mut capabilities = CapabilitySet::new();
    match member.role_type {
        RoleType::User => {}
        RoleType::Architect => {
            capabilities.insert(WorkflowCapability::Planner);
            capabilities.insert(WorkflowCapability::Reviewer);
            if !has_manager {
                capabilities.insert(WorkflowCapability::Dispatcher);
            }
        }
        RoleType::Manager => {
            capabilities.insert(WorkflowCapability::Dispatcher);
            capabilities.insert(WorkflowCapability::Reviewer);
            if !has_architect && is_top_level {
                capabilities.insert(WorkflowCapability::Planner);
            }
        }
        RoleType::Engineer => {
            capabilities.insert(WorkflowCapability::Executor);
            if !has_architect && !has_manager && is_top_level {
                capabilities.insert(WorkflowCapability::Planner);
                capabilities.insert(WorkflowCapability::Dispatcher);
            }
        }
    }

    capabilities
}

/// Resolve the full workflow capability map for a team topology.
///
/// The returned map includes both agent members and control-plane subjects.
/// This keeps orchestrator/operator duties explicit rather than quietly
/// attaching them to a regular agent.
pub fn resolve_capability_map(members: &[MemberInstance]) -> CapabilityMap {
    let mut map = CapabilityMap::new();

    for member in members {
        if member.role_type == RoleType::User {
            continue;
        }
        let capabilities = resolve_member_capabilities(member, members);
        if !capabilities.is_empty() {
            map.insert(CapabilitySubject::Member(member.name.clone()), capabilities);
        }
    }

    let mut operator_capabilities = CapabilitySet::from([WorkflowCapability::Operator]);
    if !has_agent_reviewer(members) {
        operator_capabilities.insert(WorkflowCapability::Reviewer);
    }
    map.insert(CapabilitySubject::Operator, operator_capabilities);
    map.insert(
        CapabilitySubject::Orchestrator,
        CapabilitySet::from([WorkflowCapability::Orchestrator]),
    );

    map
}

fn has_agent_reviewer(members: &[MemberInstance]) -> bool {
    members.iter().any(|member| {
        matches!(member.role_type, RoleType::Architect | RoleType::Manager)
            && !resolve_member_capabilities(member, members).is_empty()
            && resolve_member_capabilities(member, members).contains(&WorkflowCapability::Reviewer)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::TeamConfig;
    use crate::team::hierarchy;

    fn members_from_yaml(yaml: &str) -> Vec<MemberInstance> {
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        hierarchy::resolve_hierarchy(&config).unwrap()
    }

    fn member_capabilities(map: &CapabilityMap, member_name: &str) -> CapabilitySet {
        map.get(&CapabilitySubject::Member(member_name.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn capability_set(capabilities: &[WorkflowCapability]) -> CapabilitySet {
        capabilities.iter().copied().collect()
    }

    #[test]
    fn solo_topology_falls_back_to_operator_review() {
        let members = members_from_yaml(
            r#"
name: solo
roles:
  - name: builder
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert_eq!(
            member_capabilities(&capability_map, "builder"),
            capability_set(&[
                WorkflowCapability::Planner,
                WorkflowCapability::Dispatcher,
                WorkflowCapability::Executor,
            ])
        );
        assert_eq!(
            capability_map
                .get(&CapabilitySubject::Operator)
                .cloned()
                .unwrap(),
            capability_set(&[WorkflowCapability::Operator, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            capability_map
                .get(&CapabilitySubject::Orchestrator)
                .cloned()
                .unwrap(),
            capability_set(&[WorkflowCapability::Orchestrator])
        );
    }

    #[test]
    fn pair_topology_assigns_architect_planning_dispatch_and_review() {
        let members = members_from_yaml(
            r#"
name: pair
roles:
  - name: planner
    role_type: architect
    agent: claude
    instances: 1
  - name: builder
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert_eq!(
            member_capabilities(&capability_map, "planner"),
            capability_set(&[
                WorkflowCapability::Planner,
                WorkflowCapability::Dispatcher,
                WorkflowCapability::Reviewer,
            ])
        );
        assert_eq!(
            member_capabilities(&capability_map, "builder"),
            capability_set(&[WorkflowCapability::Executor])
        );
        assert_eq!(
            capability_map
                .get(&CapabilitySubject::Operator)
                .cloned()
                .unwrap(),
            capability_set(&[WorkflowCapability::Operator])
        );
    }

    #[test]
    fn manager_led_topology_promotes_top_level_manager_to_planner() {
        let members = members_from_yaml(
            r#"
name: manager-led
roles:
  - name: lead
    role_type: manager
    agent: claude
    instances: 1
  - name: implementer
    role_type: engineer
    agent: codex
    instances: 2
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert_eq!(
            member_capabilities(&capability_map, "lead"),
            capability_set(&[
                WorkflowCapability::Planner,
                WorkflowCapability::Dispatcher,
                WorkflowCapability::Reviewer,
            ])
        );
        assert_eq!(
            member_capabilities(&capability_map, "implementer-1-1"),
            capability_set(&[WorkflowCapability::Executor])
        );
        assert_eq!(
            member_capabilities(&capability_map, "implementer-1-2"),
            capability_set(&[WorkflowCapability::Executor])
        );
    }

    #[test]
    fn multi_manager_topology_keeps_architect_planning_and_managers_dispatching() {
        let members = members_from_yaml(
            r#"
name: squad
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
  - name: manager
    role_type: manager
    agent: claude
    instances: 2
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 2
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert_eq!(
            member_capabilities(&capability_map, "architect"),
            capability_set(&[WorkflowCapability::Planner, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "manager-1"),
            capability_set(&[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "manager-2"),
            capability_set(&[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "eng-1-1"),
            capability_set(&[WorkflowCapability::Executor])
        );
        assert_eq!(
            member_capabilities(&capability_map, "eng-2-2"),
            capability_set(&[WorkflowCapability::Executor])
        );
    }

    #[test]
    fn renamed_roles_resolve_from_role_type_instead_of_role_name() {
        let members = members_from_yaml(
            r#"
name: renamed
roles:
  - name: human
    role_type: user
  - name: tech-lead
    role_type: architect
    agent: claude
    instances: 1
  - name: backend-mgr
    role_type: manager
    agent: claude
    instances: 1
  - name: frontend-mgr
    role_type: manager
    agent: claude
    instances: 1
  - name: developer
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert_eq!(
            member_capabilities(&capability_map, "tech-lead"),
            capability_set(&[WorkflowCapability::Planner, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "backend-mgr"),
            capability_set(&[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "frontend-mgr"),
            capability_set(&[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            member_capabilities(&capability_map, "developer-1-1"),
            capability_set(&[WorkflowCapability::Executor])
        );
    }

    // --- New tests for task #261 ---

    #[test]
    fn user_role_gets_no_capabilities_and_is_excluded_from_map() {
        let members = members_from_yaml(
            r#"
name: with-user
roles:
  - name: human
    role_type: user
  - name: builder
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        // User role should not appear in the capability map
        assert!(!capability_map.contains_key(&CapabilitySubject::Member("human".to_string())));
        // Engineer should still get full solo capabilities
        assert_eq!(
            member_capabilities(&capability_map, "builder"),
            capability_set(&[
                WorkflowCapability::Planner,
                WorkflowCapability::Dispatcher,
                WorkflowCapability::Executor,
            ])
        );
    }

    #[test]
    fn resolve_member_capabilities_returns_empty_for_user() {
        let member = MemberInstance {
            name: "human".to_string(),
            role_name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let members = vec![member.clone()];
        let caps = resolve_member_capabilities(&member, &members);
        assert!(caps.is_empty());
    }

    #[test]
    fn empty_member_list_produces_operator_and_orchestrator_only() {
        let capability_map = resolve_capability_map(&[]);

        // No agent members
        assert!(
            !capability_map
                .keys()
                .any(|k| matches!(k, CapabilitySubject::Member(_)))
        );

        // Operator should have reviewer fallback since no agent reviewers
        assert_eq!(
            capability_map
                .get(&CapabilitySubject::Operator)
                .cloned()
                .unwrap(),
            capability_set(&[WorkflowCapability::Operator, WorkflowCapability::Reviewer])
        );
        assert_eq!(
            capability_map
                .get(&CapabilitySubject::Orchestrator)
                .cloned()
                .unwrap(),
            capability_set(&[WorkflowCapability::Orchestrator])
        );
    }

    #[test]
    fn architect_gets_dispatch_when_no_manager_exists() {
        let members = members_from_yaml(
            r#"
name: no-manager
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        // Architect picks up Dispatcher because no manager
        assert!(
            member_capabilities(&capability_map, "arch").contains(&WorkflowCapability::Dispatcher)
        );
    }

    #[test]
    fn architect_loses_dispatch_when_manager_present() {
        let members = members_from_yaml(
            r#"
name: full-team
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: mgr
    role_type: manager
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        // Architect should NOT have Dispatcher when manager exists
        assert!(
            !member_capabilities(&capability_map, "arch").contains(&WorkflowCapability::Dispatcher)
        );
        // Manager should have Dispatcher
        assert!(
            member_capabilities(&capability_map, "mgr").contains(&WorkflowCapability::Dispatcher)
        );
    }

    #[test]
    fn subordinate_engineer_never_gets_planner_or_dispatcher() {
        let members = members_from_yaml(
            r#"
name: hierarchy
roles:
  - name: lead
    role_type: manager
    agent: claude
    instances: 1
  - name: worker
    role_type: engineer
    agent: codex
    instances: 3
"#,
        );

        let capability_map = resolve_capability_map(&members);

        for i in 1..=3 {
            let name = format!("worker-1-{}", i);
            let caps = member_capabilities(&capability_map, &name);
            assert_eq!(caps, capability_set(&[WorkflowCapability::Executor]));
            assert!(!caps.contains(&WorkflowCapability::Planner));
            assert!(!caps.contains(&WorkflowCapability::Dispatcher));
        }
    }

    #[test]
    fn manager_without_architect_and_top_level_gets_planner() {
        let members = members_from_yaml(
            r#"
name: manager-only
roles:
  - name: mgr
    role_type: manager
    agent: claude
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert!(member_capabilities(&capability_map, "mgr").contains(&WorkflowCapability::Planner));
    }

    #[test]
    fn manager_with_architect_does_not_get_planner() {
        let members = members_from_yaml(
            r#"
name: with-arch
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: mgr
    role_type: manager
    agent: claude
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        assert!(
            !member_capabilities(&capability_map, "mgr").contains(&WorkflowCapability::Planner)
        );
    }

    #[test]
    fn operator_gets_reviewer_when_no_architect_or_manager() {
        let members = members_from_yaml(
            r#"
name: engineers-only
roles:
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#,
        );

        let capability_map = resolve_capability_map(&members);

        // No supervisory roles → operator is review fallback
        let operator_caps = capability_map
            .get(&CapabilitySubject::Operator)
            .cloned()
            .unwrap();
        assert!(operator_caps.contains(&WorkflowCapability::Reviewer));
    }

    #[test]
    fn operator_does_not_get_reviewer_when_architect_exists() {
        let members = members_from_yaml(
            r#"
name: with-arch
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        let operator_caps = capability_map
            .get(&CapabilitySubject::Operator)
            .cloned()
            .unwrap();
        assert!(!operator_caps.contains(&WorkflowCapability::Reviewer));
    }

    #[test]
    fn operator_does_not_get_reviewer_when_manager_exists() {
        let members = members_from_yaml(
            r#"
name: with-mgr
roles:
  - name: mgr
    role_type: manager
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);

        let operator_caps = capability_map
            .get(&CapabilitySubject::Operator)
            .cloned()
            .unwrap();
        assert!(!operator_caps.contains(&WorkflowCapability::Reviewer));
    }

    #[test]
    fn orchestrator_always_has_only_orchestrator_capability() {
        // Test across multiple topologies
        let topologies = vec![
            // Solo
            r#"
name: solo
roles:
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
            // Full team
            r#"
name: full
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: mgr
    role_type: manager
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        ];

        for yaml in topologies {
            let members = members_from_yaml(yaml);
            let capability_map = resolve_capability_map(&members);
            assert_eq!(
                capability_map
                    .get(&CapabilitySubject::Orchestrator)
                    .cloned()
                    .unwrap(),
                capability_set(&[WorkflowCapability::Orchestrator])
            );
        }
    }

    #[test]
    fn has_agent_reviewer_returns_true_with_architect() {
        let members = members_from_yaml(
            r#"
name: test
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        assert!(has_agent_reviewer(&members));
    }

    #[test]
    fn has_agent_reviewer_returns_false_with_only_engineers() {
        let members = members_from_yaml(
            r#"
name: test
roles:
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#,
        );

        assert!(!has_agent_reviewer(&members));
    }

    #[test]
    fn has_agent_reviewer_returns_false_with_only_user_roles() {
        let member = MemberInstance {
            name: "human".to_string(),
            role_name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        assert!(!has_agent_reviewer(&[member]));
    }

    #[test]
    fn capability_map_member_count_matches_non_user_members() {
        let members = members_from_yaml(
            r#"
name: mixed
roles:
  - name: human
    role_type: user
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#,
        );

        let capability_map = resolve_capability_map(&members);

        let member_entries = capability_map
            .keys()
            .filter(|k| matches!(k, CapabilitySubject::Member(_)))
            .count();
        // 1 architect + 2 engineers = 3 members (user excluded)
        assert_eq!(member_entries, 3);
    }

    #[test]
    fn solo_engineer_gets_all_three_agent_capabilities() {
        let members = members_from_yaml(
            r#"
name: solo
roles:
  - name: lone-wolf
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        let capability_map = resolve_capability_map(&members);
        let caps = member_capabilities(&capability_map, "lone-wolf");

        assert!(caps.contains(&WorkflowCapability::Planner));
        assert!(caps.contains(&WorkflowCapability::Dispatcher));
        assert!(caps.contains(&WorkflowCapability::Executor));
        assert!(!caps.contains(&WorkflowCapability::Reviewer));
        assert!(!caps.contains(&WorkflowCapability::Orchestrator));
        assert!(!caps.contains(&WorkflowCapability::Operator));
    }

    #[test]
    fn multi_instance_engineers_all_get_same_capabilities() {
        let members = members_from_yaml(
            r#"
name: team
roles:
  - name: arch
    role_type: architect
    agent: claude
    instances: 1
  - name: eng
    role_type: engineer
    agent: codex
    instances: 4
"#,
        );

        let capability_map = resolve_capability_map(&members);
        let expected = capability_set(&[WorkflowCapability::Executor]);

        // No manager → flat naming: eng-1, eng-2, eng-3, eng-4
        for i in 1..=4 {
            let name = format!("eng-{}", i);
            assert_eq!(member_capabilities(&capability_map, &name), expected);
        }
    }

    #[test]
    fn capability_subject_ordering_is_deterministic() {
        // CapabilitySubject derives Ord — variant order: Member, Orchestrator, Operator
        let a = CapabilitySubject::Member("aaa".to_string());
        let b = CapabilitySubject::Member("zzz".to_string());
        let orch = CapabilitySubject::Orchestrator;
        let op = CapabilitySubject::Operator;

        assert!(a < b);
        assert!(a < orch);
        assert!(orch < op);
    }
}
