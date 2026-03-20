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
}
