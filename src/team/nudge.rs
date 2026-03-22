#![cfg_attr(not(test), allow(dead_code))]

//! Dependency-aware nudge target selection.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;

use super::capability::{WorkflowCapability, resolve_member_capabilities};
use super::hierarchy::MemberInstance;
use super::resolver::{ResolutionStatus, resolve_board};
use super::standup::MemberState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NudgeTarget {
    pub member: String,
    pub reason: String,
    pub capability: WorkflowCapability,
}

pub fn compute_nudges(
    board_dir: &Path,
    members: &[MemberInstance],
    states: &HashMap<String, MemberState>,
    pending_inbox: &HashMap<String, usize>,
) -> Result<Vec<NudgeTarget>> {
    let resolutions = resolve_board(board_dir, members)?;
    let member_capabilities: HashMap<String, _> = members
        .iter()
        .map(|member| {
            (
                member.name.clone(),
                resolve_member_capabilities(member, members),
            )
        })
        .collect();

    let mut targets = BTreeMap::new();
    let mut has_runnable = false;
    let mut has_blocked = false;

    for resolution in &resolutions {
        match resolution.status {
            ResolutionStatus::Runnable => {
                has_runnable = true;
                if let Some(owner) = resolution.execution_owner.as_deref() {
                    let owner = resolve_member_reference(owner, members);
                    if member_is_eligible(
                        &owner,
                        WorkflowCapability::Executor,
                        states,
                        pending_inbox,
                        &member_capabilities,
                    ) {
                        record_target(
                            &mut targets,
                            &owner,
                            WorkflowCapability::Executor,
                            format!(
                                "resume runnable owned task #{}: {}",
                                resolution.task_id, resolution.title
                            ),
                        );
                    }
                } else {
                    for member in members {
                        if member_is_eligible(
                            &member.name,
                            WorkflowCapability::Dispatcher,
                            states,
                            pending_inbox,
                            &member_capabilities,
                        ) {
                            record_target(
                                &mut targets,
                                &member.name,
                                WorkflowCapability::Dispatcher,
                                format!(
                                    "dispatch unassigned runnable task #{}: {}",
                                    resolution.task_id, resolution.title
                                ),
                            );
                        }
                    }
                }
            }
            ResolutionStatus::NeedsReview => {
                if let Some(owner) = resolution.review_owner.as_deref() {
                    let owner = resolve_member_reference(owner, members);
                    if member_is_eligible(
                        &owner,
                        WorkflowCapability::Reviewer,
                        states,
                        pending_inbox,
                        &member_capabilities,
                    ) {
                        record_target(
                            &mut targets,
                            &owner,
                            WorkflowCapability::Reviewer,
                            format!(
                                "review backlog for task #{}: {}",
                                resolution.task_id, resolution.title
                            ),
                        );
                    }
                } else {
                    for member in members {
                        if member_is_eligible(
                            &member.name,
                            WorkflowCapability::Reviewer,
                            states,
                            pending_inbox,
                            &member_capabilities,
                        ) {
                            record_target(
                                &mut targets,
                                &member.name,
                                WorkflowCapability::Reviewer,
                                format!(
                                    "review backlog for task #{}: {}",
                                    resolution.task_id, resolution.title
                                ),
                            );
                        }
                    }
                }
            }
            ResolutionStatus::Blocked => {
                has_blocked = true;
            }
            ResolutionStatus::NeedsAction => {}
        }
    }

    if !has_runnable && has_blocked {
        for member in members {
            if member_is_eligible(
                &member.name,
                WorkflowCapability::Planner,
                states,
                pending_inbox,
                &member_capabilities,
            ) {
                record_target(
                    &mut targets,
                    &member.name,
                    WorkflowCapability::Planner,
                    "blocked frontier needs planning attention".to_string(),
                );
            }
        }
    }

    Ok(targets
        .into_iter()
        .map(|((member, capability), reason)| NudgeTarget {
            member,
            reason,
            capability,
        })
        .collect())
}

fn member_is_eligible(
    member_name: &str,
    capability: WorkflowCapability,
    states: &HashMap<String, MemberState>,
    pending_inbox: &HashMap<String, usize>,
    member_capabilities: &HashMap<String, super::capability::CapabilitySet>,
) -> bool {
    matches!(states.get(member_name), Some(MemberState::Idle))
        && pending_inbox.get(member_name).copied().unwrap_or(0) == 0
        && member_capabilities
            .get(member_name)
            .is_some_and(|capabilities| capabilities.contains(&capability))
}

fn record_target(
    targets: &mut BTreeMap<(String, WorkflowCapability), String>,
    member_name: &str,
    capability: WorkflowCapability,
    reason: String,
) {
    targets
        .entry((member_name.to_string(), capability))
        .or_insert(reason);
}

fn resolve_member_reference(member_name: &str, members: &[MemberInstance]) -> String {
    if members.iter().any(|member| member.name == member_name) {
        return member_name.to_string();
    }

    let mut matches = members
        .iter()
        .filter(|member| member.role_name == member_name)
        .map(|member| member.name.as_str());
    let Some(first) = matches.next() else {
        return member_name.to_string();
    };

    if matches.next().is_none() {
        first.to_string()
    } else {
        member_name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::TeamConfig;
    use crate::team::hierarchy::resolve_hierarchy;

    fn members(yaml: &str) -> Vec<MemberInstance> {
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        resolve_hierarchy(&config).unwrap()
    }

    fn write_task(tasks_dir: &Path, id: u32, extra_frontmatter: &str) {
        let path = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        std::fs::write(
            path,
            format!(
                "---\nid: {id}\ntitle: Task {id}\npriority: high\n{extra_frontmatter}class: standard\n---\n\nBody.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn zero_members_produces_no_nudges() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();

        let nudges = compute_nudges(tmp.path(), &[], &HashMap::new(), &HashMap::new()).unwrap();

        assert!(nudges.is_empty());
    }

    #[test]
    fn idle_executor_with_runnable_owned_task_gets_nudged() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Working),
            ("builder-1-1".to_string(), MemberState::Idle),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![NudgeTarget {
                member: "builder-1-1".to_string(),
                reason: "resume runnable owned task #1: Task 1".to_string(),
                capability: WorkflowCapability::Executor,
            }]
        );
    }

    #[test]
    fn all_members_busy_produces_no_nudges() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            2,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Working),
            ("builder-1-1".to_string(), MemberState::Working),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert!(nudges.is_empty());
    }

    #[test]
    fn idle_reviewer_with_review_backlog_gets_nudged() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 2, "status: review\nreview_owner: lead\n");
        let members = members(
            r#"
name: pair
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("builder-1-1".to_string(), MemberState::Working),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![NudgeTarget {
                member: "lead".to_string(),
                reason: "review backlog for task #2: Task 2".to_string(),
                capability: WorkflowCapability::Reviewer,
            }]
        );
    }

    #[test]
    fn pending_inbox_suppresses_nudge() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            3,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([("builder-1-1".to_string(), MemberState::Idle)]);
        let pending = HashMap::from([("builder-1-1".to_string(), 1usize)]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &pending).unwrap();

        assert!(nudges.is_empty());
    }

    #[test]
    fn pending_inbox_suppresses_planner_nudge() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            4,
            "status: todo\nblocked_on: waiting-on-decision\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("builder-1-1".to_string(), MemberState::Working),
        ]);
        let pending = HashMap::from([("lead".to_string(), 1usize)]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &pending).unwrap();

        assert!(nudges.is_empty());
    }

    #[test]
    fn blocked_frontier_without_runnable_work_nudges_planner() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            4,
            "status: todo\nblocked_on: waiting-on-decision\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("builder".to_string(), MemberState::Working),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![NudgeTarget {
                member: "lead".to_string(),
                reason: "blocked frontier needs planning attention".to_string(),
                capability: WorkflowCapability::Planner,
            }]
        );
    }

    #[test]
    fn multi_hop_blocked_dependency_chain_nudges_planner() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\ndepends_on:\n  - 2\n");
        write_task(&tasks_dir, 2, "status: todo\ndepends_on:\n  - 3\n");
        write_task(
            &tasks_dir,
            3,
            "status: todo\nblocked_on: waiting-on-decision\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("builder-1-1".to_string(), MemberState::Working),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![NudgeTarget {
                member: "architect".to_string(),
                reason: "blocked frontier needs planning attention".to_string(),
                capability: WorkflowCapability::Planner,
            }]
        );
    }

    #[test]
    fn single_architect_can_receive_dispatcher_and_reviewer_nudges() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: review\nreview_owner: architect\n");
        write_task(&tasks_dir, 2, "status: todo\n");
        let members = members(
            r#"
name: solo-architect
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        );
        let states = HashMap::from([("architect".to_string(), MemberState::Idle)]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![
                NudgeTarget {
                    member: "architect".to_string(),
                    reason: "dispatch unassigned runnable task #2: Task 2".to_string(),
                    capability: WorkflowCapability::Dispatcher,
                },
                NudgeTarget {
                    member: "architect".to_string(),
                    reason: "review backlog for task #1: Task 1".to_string(),
                    capability: WorkflowCapability::Reviewer,
                },
            ]
        );
    }

    #[test]
    fn renamed_roles_from_config_still_receive_role_type_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: review\nreview_owner: triage-lead\n");
        write_task(&tasks_dir, 2, "status: todo\n");
        let members = members(
            r#"
name: renamed
roles:
  - name: planner
    role_type: architect
    agent: claude
  - name: triage-lead
    role_type: manager
    agent: claude
  - name: implementer
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("planner".to_string(), MemberState::Working),
            ("triage-lead".to_string(), MemberState::Idle),
            ("implementer-1-1".to_string(), MemberState::Working),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![
                NudgeTarget {
                    member: "triage-lead".to_string(),
                    reason: "dispatch unassigned runnable task #2: Task 2".to_string(),
                    capability: WorkflowCapability::Dispatcher,
                },
                NudgeTarget {
                    member: "triage-lead".to_string(),
                    reason: "review backlog for task #1: Task 1".to_string(),
                    capability: WorkflowCapability::Reviewer,
                },
            ]
        );
    }

    #[test]
    fn deterministic_ordering_with_member_name_ties_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        write_task(
            &tasks_dir,
            2,
            "status: todo\nexecution_owner: builder-2-1\nclaimed_by: builder-2-1\n",
        );
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
    instances: 2
  - name: builder
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );
        let states = HashMap::from([
            ("lead-1".to_string(), MemberState::Working),
            ("lead-2".to_string(), MemberState::Working),
            ("builder-1-1".to_string(), MemberState::Idle),
            ("builder-2-1".to_string(), MemberState::Idle),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![
                NudgeTarget {
                    member: "builder-1-1".to_string(),
                    reason: "resume runnable owned task #1: Task 1".to_string(),
                    capability: WorkflowCapability::Executor,
                },
                NudgeTarget {
                    member: "builder-2-1".to_string(),
                    reason: "resume runnable owned task #2: Task 2".to_string(),
                    capability: WorkflowCapability::Executor,
                },
            ]
        );
    }

    #[test]
    fn multiple_targets_are_computed_deterministically() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        write_task(&tasks_dir, 2, "status: review\nreview_owner: lead\n");
        write_task(&tasks_dir, 3, "status: todo\n");
        let members = members(
            r#"
name: team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Idle),
            ("builder-1-1".to_string(), MemberState::Idle),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();

        assert_eq!(
            nudges,
            vec![
                NudgeTarget {
                    member: "builder-1-1".to_string(),
                    reason: "resume runnable owned task #1: Task 1".to_string(),
                    capability: WorkflowCapability::Executor,
                },
                NudgeTarget {
                    member: "lead".to_string(),
                    reason: "dispatch unassigned runnable task #3: Task 3".to_string(),
                    capability: WorkflowCapability::Dispatcher,
                },
                NudgeTarget {
                    member: "lead".to_string(),
                    reason: "review backlog for task #2: Task 2".to_string(),
                    capability: WorkflowCapability::Reviewer,
                },
            ]
        );
    }

    // --- resolve_member_reference ---

    #[test]
    fn resolve_member_reference_exact_name_match() {
        let members = members(
            "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n  - name: builder\n    role_type: engineer\n    agent: codex\n",
        );
        assert_eq!(resolve_member_reference("lead", &members), "lead");
        assert_eq!(
            resolve_member_reference("builder-1-1", &members),
            "builder-1-1"
        );
    }

    #[test]
    fn resolve_member_reference_role_name_fallback() {
        // With a manager present, engineer gets multiplicative name "builder-1-1"
        // while role_name stays "builder". But with instances:1, flat team (no manager)
        // gives name == role_name. So we need a manager to create the suffix.
        let members = members(
            "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n  - name: builder\n    role_type: engineer\n    agent: codex\n",
        );
        // "builder" is the role_name, "builder-1-1" is the instance name
        // Since there's only one instance with that role_name, it resolves to the instance
        assert_eq!(resolve_member_reference("builder", &members), "builder-1-1");
    }

    #[test]
    fn resolve_member_reference_unknown_returns_as_is() {
        let members = members(
            "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n",
        );
        assert_eq!(
            resolve_member_reference("unknown-member", &members),
            "unknown-member"
        );
    }

    // --- empty board ---

    #[test]
    fn empty_board_produces_no_nudges() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();
        let members = members(
            "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n",
        );
        let states = HashMap::from([("lead".to_string(), MemberState::Idle)]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();
        assert!(nudges.is_empty());
    }

    // --- review without owner nudges all eligible ---

    #[test]
    fn review_without_owner_nudges_all_eligible_reviewers() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: review\n"); // no review_owner
        // Use two managers so both have reviewer capability
        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
    instances: 2
"#,
        );
        let states = HashMap::from([
            ("lead-1".to_string(), MemberState::Idle),
            ("lead-2".to_string(), MemberState::Idle),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();
        let reviewer_names: Vec<&str> = nudges.iter().map(|n| n.member.as_str()).collect();
        assert!(reviewer_names.contains(&"lead-1"));
        assert!(reviewer_names.contains(&"lead-2"));
        assert!(
            nudges
                .iter()
                .all(|n| n.capability == WorkflowCapability::Reviewer)
        );
    }

    // --- mixed blocked and runnable suppresses planner ---

    #[test]
    fn mixed_blocked_and_runnable_no_planner_nudge() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\nblocked_on: waiting\n");
        write_task(&tasks_dir, 2, "status: todo\n"); // runnable

        let members = members(
            r#"
name: team
roles:
  - name: lead
    role_type: manager
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
        );
        let states = HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("builder-1-1".to_string(), MemberState::Idle),
        ]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();
        // Planner nudge only fires when ALL tasks are blocked and none are runnable
        assert!(
            !nudges
                .iter()
                .any(|n| n.capability == WorkflowCapability::Planner)
        );
    }

    // --- unknown member state ---

    #[test]
    fn unknown_member_state_not_nudged() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nexecution_owner: builder-1-1\nclaimed_by: builder-1-1\n",
        );
        let members = members(
            "name: team\nroles:\n  - name: builder\n    role_type: engineer\n    agent: codex\n",
        );
        // No state entry for builder-1-1 at all
        let states = HashMap::new();

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();
        assert!(nudges.is_empty());
    }

    // --- working member not nudged ---

    #[test]
    fn working_member_not_nudged_for_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\n"); // unassigned
        let members = members(
            "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n",
        );
        let states = HashMap::from([("lead".to_string(), MemberState::Working)]);

        let nudges = compute_nudges(tmp.path(), &members, &states, &HashMap::new()).unwrap();
        assert!(nudges.is_empty());
    }
}
