#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::fs;
    use std::path::Path;

    use super::super::board::read_workflow_metadata;
    use super::super::capability::{
        CapabilityMap, CapabilitySubject, WorkflowCapability, resolve_capability_map,
    };
    use super::super::completion::ingest_completion_message;
    use super::super::config::{RoleType, TeamConfig, WorkflowMode, WorkflowPolicy};
    use super::super::hierarchy::{MemberInstance, resolve_hierarchy};
    use super::super::nudge::compute_nudges;
    use super::super::policy::{check_wip_limit, is_review_stale, should_escalate};
    use super::super::resolver::{ResolutionStatus, resolve_board, runnable_tasks};
    use super::super::review::{MergeDisposition, ReviewState, apply_review};
    use super::super::standup::MemberState;
    use super::super::team_config_dir;
    use super::super::workflow::{TaskState, WorkflowMeta};

    #[derive(Debug)]
    struct TemplateExpectation {
        users: usize,
        architects: usize,
        managers: usize,
        engineers: usize,
        role_capabilities: Vec<(&'static str, &'static [WorkflowCapability])>,
        operator_caps: &'static [WorkflowCapability],
    }

    fn capability_set(values: &[WorkflowCapability]) -> BTreeSet<WorkflowCapability> {
        values.iter().copied().collect()
    }

    fn capability_subject_set(
        map: &CapabilityMap,
        subject: CapabilitySubject,
    ) -> BTreeSet<WorkflowCapability> {
        map.get(&subject).cloned().unwrap_or_default()
    }

    fn member_capabilities(map: &CapabilityMap, member_name: &str) -> BTreeSet<WorkflowCapability> {
        capability_subject_set(map, CapabilitySubject::Member(member_name.to_string()))
    }

    fn load_template(yaml: &str) -> TeamConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn assert_template_topology(
        yaml: &str,
        expectation: &TemplateExpectation,
    ) -> Vec<MemberInstance> {
        let config = load_template(yaml);
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(!config.orchestrator_enabled());

        let members = resolve_hierarchy(&config).unwrap();
        let capability_map = resolve_capability_map(&members);

        let users = members
            .iter()
            .filter(|member| member.role_type == RoleType::User)
            .count();
        let architects = members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .count();
        let managers = members
            .iter()
            .filter(|member| member.role_type == RoleType::Manager)
            .count();
        let engineers = members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .count();

        assert_eq!(users, expectation.users);
        assert_eq!(architects, expectation.architects);
        assert_eq!(managers, expectation.managers);
        assert_eq!(engineers, expectation.engineers);

        for (role_name, expected_caps) in &expectation.role_capabilities {
            let expected = capability_set(expected_caps);
            let role_members: Vec<_> = members
                .iter()
                .filter(|member| member.role_name == *role_name)
                .collect();
            assert!(
                !role_members.is_empty(),
                "expected at least one member for role `{role_name}`"
            );
            for member in role_members {
                assert_eq!(
                    member_capabilities(&capability_map, &member.name),
                    expected,
                    "unexpected capabilities for {}",
                    member.name
                );
            }
        }

        assert_eq!(
            capability_subject_set(&capability_map, CapabilitySubject::Operator),
            capability_set(expectation.operator_caps)
        );
        assert_eq!(
            capability_subject_set(&capability_map, CapabilitySubject::Orchestrator),
            capability_set(&[WorkflowCapability::Orchestrator])
        );

        members
    }

    fn idle_states(members: &[MemberInstance]) -> HashMap<String, MemberState> {
        members
            .iter()
            .filter(|member| member.role_type != RoleType::User)
            .map(|member| (member.name.clone(), MemberState::Idle))
            .collect()
    }

    fn write_task(tasks_dir: &Path, id: u32, extra_frontmatter: &str) {
        fs::write(
            tasks_dir.join(format!("{id:03}-task-{id}.md")),
            format!(
                "---\nid: {id}\ntitle: Task {id}\npriority: medium\n{extra_frontmatter}class: standard\n---\n\nBody.\n"
            ),
        )
        .unwrap();
    }

    fn create_task(project_root: &Path, id: u32, extra_frontmatter: &str) -> std::path::PathBuf {
        let tasks_dir = team_config_dir(project_root).join("board").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        fs::write(
            &task_path,
            format!(
                "---\nid: {id}\ntitle: Task {id}\nstatus: review\npriority: medium\n{extra_frontmatter}class: standard\n---\n\nBody.\n"
            ),
        )
        .unwrap();
        task_path
    }

    #[test]
    fn team_solo_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_solo.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 0,
                managers: 0,
                engineers: 1,
                role_capabilities: vec![(
                    "engineer",
                    &[
                        WorkflowCapability::Planner,
                        WorkflowCapability::Dispatcher,
                        WorkflowCapability::Executor,
                    ],
                )],
                operator_caps: &[WorkflowCapability::Operator, WorkflowCapability::Reviewer],
            },
        );

        assert_eq!(members[0].name, "engineer");
        assert!(!members[0].use_worktrees);
    }

    #[test]
    fn team_pair_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_pair.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 0,
                engineers: 1,
                role_capabilities: vec![
                    (
                        "architect",
                        &[
                            WorkflowCapability::Planner,
                            WorkflowCapability::Dispatcher,
                            WorkflowCapability::Reviewer,
                        ],
                    ),
                    ("engineer", &[WorkflowCapability::Executor]),
                ],
                operator_caps: &[WorkflowCapability::Operator],
            },
        );

        assert!(members.iter().any(|member| member.name == "architect"));
        assert!(members.iter().any(|member| member.name == "engineer"));
    }

    #[test]
    fn team_simple_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_simple.yaml"),
            &TemplateExpectation {
                users: 1,
                architects: 1,
                managers: 1,
                engineers: 3,
                role_capabilities: vec![
                    (
                        "architect",
                        &[WorkflowCapability::Planner, WorkflowCapability::Reviewer],
                    ),
                    (
                        "manager",
                        &[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer],
                    ),
                    ("engineer", &[WorkflowCapability::Executor]),
                ],
                operator_caps: &[WorkflowCapability::Operator],
            },
        );

        assert!(members.iter().any(|member| member.name == "human"));
        assert_eq!(
            members
                .iter()
                .filter(|member| member.role_name == "engineer")
                .count(),
            3
        );
    }

    #[test]
    fn team_squad_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_squad.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 1,
                engineers: 5,
                role_capabilities: vec![
                    (
                        "architect",
                        &[WorkflowCapability::Planner, WorkflowCapability::Reviewer],
                    ),
                    (
                        "manager",
                        &[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer],
                    ),
                    ("engineer", &[WorkflowCapability::Executor]),
                ],
                operator_caps: &[WorkflowCapability::Operator],
            },
        );

        assert_eq!(
            members
                .iter()
                .filter(|member| member.role_name == "engineer")
                .count(),
            5
        );
    }

    #[test]
    fn team_research_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_research.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 3,
                engineers: 6,
                role_capabilities: vec![
                    (
                        "principal",
                        &[WorkflowCapability::Planner, WorkflowCapability::Reviewer],
                    ),
                    (
                        "sub-lead",
                        &[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer],
                    ),
                    ("researcher", &[WorkflowCapability::Executor]),
                ],
                operator_caps: &[WorkflowCapability::Operator],
            },
        );

        assert!(members.iter().any(|member| member.name == "principal"));
        assert_eq!(
            members
                .iter()
                .filter(|member| member.role_name == "researcher")
                .count(),
            6
        );
    }

    #[test]
    fn team_software_template_validation_uses_real_capabilities() {
        let members = assert_template_topology(
            include_str!("templates/team_software.yaml"),
            &TemplateExpectation {
                users: 1,
                architects: 1,
                managers: 2,
                engineers: 8,
                role_capabilities: vec![
                    (
                        "tech-lead",
                        &[WorkflowCapability::Planner, WorkflowCapability::Reviewer],
                    ),
                    (
                        "backend-mgr",
                        &[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer],
                    ),
                    (
                        "frontend-mgr",
                        &[WorkflowCapability::Dispatcher, WorkflowCapability::Reviewer],
                    ),
                    ("developer", &[WorkflowCapability::Executor]),
                ],
                operator_caps: &[WorkflowCapability::Operator],
            },
        );

        assert!(members.iter().any(|member| member.name == "human"));
        assert_eq!(
            members
                .iter()
                .filter(|member| member.role_name == "developer")
                .count(),
            8
        );
    }

    #[test]
    fn orchestrator_enabled_uses_real_workflow_modes() {
        let legacy: TeamConfig = serde_yaml::from_str(
            r#"
name: legacy
workflow_mode: legacy
orchestrator_pane: true
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();
        let hybrid: TeamConfig = serde_yaml::from_str(
            r#"
name: hybrid
workflow_mode: hybrid
orchestrator_pane: true
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();
        let workflow_first: TeamConfig = serde_yaml::from_str(
            r#"
name: wf
workflow_mode: workflow_first
orchestrator_pane: true
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();
        let workflow_first_hidden: TeamConfig = serde_yaml::from_str(
            r#"
name: wf-hidden
workflow_mode: workflow_first
orchestrator_pane: false
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();

        assert!(!legacy.orchestrator_enabled());
        assert!(hybrid.orchestrator_enabled());
        assert!(workflow_first.orchestrator_enabled());
        assert!(!workflow_first_hidden.orchestrator_enabled());
    }

    #[test]
    fn resolve_board_and_runnable_tasks_use_real_workflow_resolver() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        write_task(&tasks_dir, 1, "status: done\n");
        write_task(
            &tasks_dir,
            2,
            "status: todo\nexecution_owner: eng-1-1\nclaimed_by: eng-1-1\n",
        );
        write_task(&tasks_dir, 3, "status: review\nreview_owner: manager\n");
        write_task(&tasks_dir, 4, "status: todo\nblocked_on: waiting for api\n");
        write_task(&tasks_dir, 5, "status: todo\ndepends_on: [6]\n");
        write_task(&tasks_dir, 6, "status: backlog\n");

        let members = resolve_hierarchy(
            &serde_yaml::from_str::<TeamConfig>(
                r#"
name: team
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
            )
            .unwrap(),
        )
        .unwrap();

        let resolutions = resolve_board(tmp.path(), &members).unwrap();
        let runnable = runnable_tasks(&resolutions);

        assert_eq!(
            runnable.iter().map(|task| task.task_id).collect::<Vec<_>>(),
            vec![2, 6]
        );
        assert_eq!(
            resolutions
                .iter()
                .find(|task| task.task_id == 2)
                .map(|task| task.status),
            Some(ResolutionStatus::Runnable)
        );
        assert_eq!(
            resolutions
                .iter()
                .find(|task| task.task_id == 3)
                .map(|task| task.status),
            Some(ResolutionStatus::NeedsReview)
        );
        assert_eq!(
            resolutions
                .iter()
                .find(|task| task.task_id == 4)
                .and_then(|task| task.blocking_reason.as_deref()),
            Some("waiting for api")
        );
        assert_eq!(
            resolutions
                .iter()
                .find(|task| task.task_id == 5)
                .and_then(|task| task.blocking_reason.as_deref()),
            Some("unmet dependency #6")
        );
    }

    #[test]
    fn compute_nudges_uses_real_planner_path_for_each_shipped_topology() {
        let templates = [
            include_str!("templates/team_solo.yaml"),
            include_str!("templates/team_pair.yaml"),
            include_str!("templates/team_simple.yaml"),
            include_str!("templates/team_squad.yaml"),
            include_str!("templates/team_research.yaml"),
            include_str!("templates/team_software.yaml"),
        ];

        for yaml in templates {
            let config = load_template(yaml);
            let members = resolve_hierarchy(&config).unwrap();
            let capability_map = resolve_capability_map(&members);

            let tmp = tempfile::tempdir().unwrap();
            let tasks_dir = tmp.path().join("tasks");
            fs::create_dir_all(&tasks_dir).unwrap();
            write_task(
                &tasks_dir,
                1,
                "status: blocked\nblocked_on: waiting on dependency\n",
            );

            let nudges = compute_nudges(
                tmp.path(),
                &members,
                &idle_states(&members),
                &HashMap::new(),
            )
            .unwrap();

            assert!(
                nudges
                    .iter()
                    .any(|target| target.capability == WorkflowCapability::Planner),
                "expected planner nudge for topology {:?}",
                config.name
            );

            for planner in nudges
                .iter()
                .filter(|target| target.capability == WorkflowCapability::Planner)
            {
                assert!(
                    member_capabilities(&capability_map, &planner.member)
                        .contains(&WorkflowCapability::Planner),
                    "planner nudge targeted non-planner {}",
                    planner.member
                );
            }
        }
    }

    #[test]
    fn compute_nudges_uses_real_dispatch_path_for_each_shipped_topology() {
        let templates = [
            include_str!("templates/team_solo.yaml"),
            include_str!("templates/team_pair.yaml"),
            include_str!("templates/team_simple.yaml"),
            include_str!("templates/team_squad.yaml"),
            include_str!("templates/team_research.yaml"),
            include_str!("templates/team_software.yaml"),
        ];

        for yaml in templates {
            let config = load_template(yaml);
            let members = resolve_hierarchy(&config).unwrap();
            let capability_map = resolve_capability_map(&members);

            let tmp = tempfile::tempdir().unwrap();
            let tasks_dir = tmp.path().join("tasks");
            fs::create_dir_all(&tasks_dir).unwrap();
            write_task(&tasks_dir, 1, "status: todo\n");

            let nudges = compute_nudges(
                tmp.path(),
                &members,
                &idle_states(&members),
                &HashMap::new(),
            )
            .unwrap();

            assert!(
                nudges
                    .iter()
                    .any(|target| target.capability == WorkflowCapability::Dispatcher),
                "expected dispatcher nudge for topology {:?}",
                config.name
            );

            for dispatcher in nudges
                .iter()
                .filter(|target| target.capability == WorkflowCapability::Dispatcher)
            {
                assert!(
                    member_capabilities(&capability_map, &dispatcher.member)
                        .contains(&WorkflowCapability::Dispatcher),
                    "dispatcher nudge targeted non-dispatcher {}",
                    dispatcher.member
                );
            }
        }
    }

    #[test]
    fn workflow_meta_transitions_end_to_end_with_real_review_flow() {
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            execution_owner: Some("eng-1-1".to_string()),
            review_owner: Some("manager".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1-1".to_string()),
            branch: Some("eng-1-1/task-34".to_string()),
            commit: Some("abc1234".to_string()),
            artifacts: vec!["target/nextest/default.xml".to_string()],
            ..WorkflowMeta::default()
        };

        meta.transition(TaskState::InProgress).unwrap();
        meta.next_action = Some("run tests".to_string());
        meta.transition(TaskState::Review).unwrap();
        meta.review = Some(ReviewState {
            reviewer: "manager".to_string(),
            packet_ref: Some("review/packet-34.json".to_string()),
            disposition: MergeDisposition::MergeReady,
            notes: Some("ready for merge".to_string()),
            reviewed_at: None,
            nudge_sent: false,
        });

        apply_review(&mut meta, MergeDisposition::MergeReady, "manager").unwrap();

        assert_eq!(meta.state, TaskState::Done);
        assert_eq!(meta.review_owner.as_deref(), Some("manager"));
        assert_eq!(
            meta.review_disposition,
            Some(super::super::workflow::ReviewDisposition::Approved)
        );
        assert_eq!(
            meta.review
                .as_ref()
                .and_then(|review| review.packet_ref.as_deref()),
            Some("review/packet-34.json")
        );
        assert_eq!(meta.branch.as_deref(), Some("eng-1-1/task-34"));
        assert_eq!(meta.commit.as_deref(), Some("abc1234"));
        assert_eq!(meta.artifacts, vec!["target/nextest/default.xml"]);
    }

    #[test]
    fn completion_packet_ingestion_uses_real_parser_and_metadata_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let task_path = create_task(tmp.path(), 34, "");

        let message = r#"Done.

## Completion Packet

```json
{
  "task_id": 34,
  "branch": "eng-1-1/task-34",
  "worktree_path": ".batty/worktrees/eng-1-1",
  "commit": "def5678",
  "changed_paths": ["src/team/validation.rs"],
  "tests_run": true,
  "tests_passed": true,
  "artifacts": ["target/nextest/default.xml"],
  "outcome": "ready_for_review"
}
```"#;

        let task_id = ingest_completion_message(tmp.path(), message).unwrap();
        let metadata = read_workflow_metadata(&task_path).unwrap();

        assert_eq!(task_id, Some(34));
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-1/task-34"));
        assert_eq!(
            metadata.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-1")
        );
        assert_eq!(metadata.commit.as_deref(), Some("def5678"));
        assert_eq!(metadata.changed_paths, vec!["src/team/validation.rs"]);
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(true));
        assert_eq!(metadata.artifacts, vec!["target/nextest/default.xml"]);
        assert_eq!(metadata.outcome.as_deref(), Some("ready_for_review"));
        assert!(metadata.review_blockers.is_empty());
    }

    #[test]
    fn workflow_policy_enforcement_uses_real_policy_helpers() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: policy-team
workflow_policy:
  wip_limit_per_engineer: 2
  wip_limit_per_reviewer: 1
  escalation_threshold_secs: 120
  review_timeout_secs: 300
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();

        let policy: &WorkflowPolicy = &config.workflow_policy;

        assert!(check_wip_limit(policy, RoleType::Engineer, 1));
        assert!(!check_wip_limit(policy, RoleType::Engineer, 2));
        assert!(check_wip_limit(policy, RoleType::Manager, 0));
        assert!(!check_wip_limit(policy, RoleType::Manager, 1));
        assert!(!should_escalate(policy, 119));
        assert!(should_escalate(policy, 120));
        assert!(!is_review_stale(policy, 299));
        assert!(is_review_stale(policy, 300));
    }
}
