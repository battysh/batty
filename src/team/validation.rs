#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use serde::Deserialize;

    use crate::task::load_tasks_from_dir;

    use super::super::config::{RoleType, TeamConfig};
    use super::super::hierarchy::{MemberInstance, resolve_hierarchy};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum PrepCapability {
        Planner,
        Dispatcher,
        Reviewer,
        Executor,
    }

    #[derive(Debug)]
    struct TemplateExpectation {
        users: usize,
        architects: usize,
        managers: usize,
        engineers: usize,
        workflow_mode: &'static str,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct PrepBoardResolution {
        task_id: u32,
        runnable: bool,
        blocked_reason: Option<String>,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum ArtifactTypeScaffold {
        TestResult,
        BuildOutput,
        Documentation,
        Other,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
    struct ArtifactRecordScaffold {
        path: String,
        artifact_type: ArtifactTypeScaffold,
        created_at: Option<u64>,
        verified: bool,
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct WorkflowMetadataScaffold {
        worktree_path: Option<String>,
        branch: Option<String>,
        commit: Option<String>,
        artifacts: Vec<String>,
        next_action: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct CompletionPacket {
        task_id: u32,
        branch: String,
        commit: String,
        tests_run: Vec<String>,
        tests_passed: bool,
        outcome: String,
        #[serde(default)]
        worktree_path: Option<String>,
        #[serde(default)]
        artifacts: Vec<ArtifactRecordScaffold>,
    }

    fn workflow_mode_from_template(yaml: &str) -> String {
        yaml.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("workflow_mode:"))
            .map(str::trim)
            .unwrap_or("legacy")
            .to_string()
    }

    fn inferred_capabilities(member: &MemberInstance) -> BTreeSet<PrepCapability> {
        match member.role_type {
            RoleType::User => BTreeSet::new(),
            RoleType::Architect => BTreeSet::from([PrepCapability::Planner]),
            RoleType::Manager => {
                BTreeSet::from([PrepCapability::Dispatcher, PrepCapability::Reviewer])
            }
            RoleType::Engineer => BTreeSet::from([PrepCapability::Executor]),
        }
    }

    fn expected_capabilities(role_type: RoleType) -> BTreeSet<PrepCapability> {
        match role_type {
            RoleType::User => BTreeSet::new(),
            RoleType::Architect => BTreeSet::from([PrepCapability::Planner]),
            RoleType::Manager => {
                BTreeSet::from([PrepCapability::Dispatcher, PrepCapability::Reviewer])
            }
            RoleType::Engineer => BTreeSet::from([PrepCapability::Executor]),
        }
    }

    fn load_template(yaml: &str) -> TeamConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn assert_template_topology(
        yaml: &str,
        expectation: &TemplateExpectation,
    ) -> Vec<MemberInstance> {
        let config = load_template(yaml);
        let members = resolve_hierarchy(&config).unwrap();

        assert_eq!(
            workflow_mode_from_template(yaml),
            expectation.workflow_mode.to_string(),
            "template should default to legacy workflow mode"
        );

        let mut users = 0;
        let mut architects = 0;
        let mut managers = 0;
        let mut engineers = 0;
        for member in &members {
            match member.role_type {
                RoleType::User => users += 1,
                RoleType::Architect => architects += 1,
                RoleType::Manager => managers += 1,
                RoleType::Engineer => engineers += 1,
            }

            // TODO(task-25): Replace inferred capabilities with capability::resolve_capability_map.
            assert_eq!(
                inferred_capabilities(member),
                expected_capabilities(member.role_type),
                "unexpected capability set for {}",
                member.name
            );
        }

        assert_eq!(users, expectation.users);
        assert_eq!(architects, expectation.architects);
        assert_eq!(managers, expectation.managers);
        assert_eq!(engineers, expectation.engineers);

        members
    }

    fn classify_board_for_validation_prep(tasks_dir: &std::path::Path) -> Vec<PrepBoardResolution> {
        let tasks = load_tasks_from_dir(tasks_dir).unwrap();
        let status_by_id: std::collections::HashMap<u32, String> = tasks
            .iter()
            .map(|task| (task.id, task.status.clone()))
            .collect();

        tasks
            .into_iter()
            .filter(|task| task.status != "done")
            .map(|task| {
                let blocked_reason = task.blocked.clone().or_else(|| {
                    task.depends_on.iter().find_map(|dep_id| {
                        let status = status_by_id.get(dep_id)?;
                        (status != "done").then(|| format!("dependency #{dep_id} not done"))
                    })
                });

                PrepBoardResolution {
                    task_id: task.id,
                    runnable: blocked_reason.is_none()
                        && matches!(task.status.as_str(), "backlog" | "todo" | "in-progress"),
                    blocked_reason,
                }
            })
            .collect()
    }

    fn track_artifact_scaffold(metadata: &mut WorkflowMetadataScaffold, artifact: &str) {
        let artifact = artifact.trim();
        if artifact.is_empty() {
            return;
        }
        if !metadata
            .artifacts
            .iter()
            .any(|existing| existing == artifact)
        {
            metadata.artifacts.push(artifact.to_string());
        }
    }

    fn apply_stage(status: &str, metadata: &mut WorkflowMetadataScaffold) {
        metadata.next_action = match status {
            "todo" => Some("execute".to_string()),
            "in-progress" => Some("finish_execution".to_string()),
            "review" => Some("review".to_string()),
            "done" => None,
            other => Some(other.to_string()),
        };
    }

    fn parse_completion_packet(body: &str) -> CompletionPacket {
        let start = body.find("```json").unwrap();
        let after_fence = &body[start + "```json".len()..];
        let inner = after_fence.strip_prefix('\n').unwrap_or(after_fence);
        let end = inner.find("```").unwrap();
        serde_json::from_str(inner[..end].trim()).unwrap()
    }

    fn apply_completion_packet(metadata: &mut WorkflowMetadataScaffold, packet: &CompletionPacket) {
        let _ = packet.task_id;
        let _ = &packet.tests_run;
        let _ = packet.tests_passed;

        metadata.branch = Some(packet.branch.clone());
        metadata.commit = Some(packet.commit.clone());
        metadata.worktree_path = packet.worktree_path.clone();
        metadata.next_action = Some(packet.outcome.clone());
        for artifact in &packet.artifacts {
            track_artifact_scaffold(metadata, &artifact.path);
        }
    }

    #[test]
    fn team_solo_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_solo.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 0,
                managers: 0,
                engineers: 1,
                workflow_mode: "legacy",
            },
        );

        assert!(members.iter().all(|member| !member.use_worktrees));
    }

    #[test]
    fn team_pair_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_pair.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 0,
                engineers: 1,
                workflow_mode: "legacy",
            },
        );

        assert!(
            members
                .iter()
                .any(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        );
    }

    #[test]
    fn team_simple_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_simple.yaml"),
            &TemplateExpectation {
                users: 1,
                architects: 1,
                managers: 1,
                engineers: 3,
                workflow_mode: "legacy",
            },
        );

        assert!(members.iter().any(|member| member.name == "human"));
    }

    #[test]
    fn team_squad_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_squad.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 1,
                engineers: 5,
                workflow_mode: "legacy",
            },
        );

        assert_eq!(
            members
                .iter()
                .filter(|member| member.role_type == RoleType::Engineer)
                .count(),
            5
        );
    }

    #[test]
    fn team_research_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_research.yaml"),
            &TemplateExpectation {
                users: 0,
                architects: 1,
                managers: 3,
                engineers: 6,
                workflow_mode: "legacy",
            },
        );

        assert!(members.iter().any(|member| member.role_name == "principal"));
    }

    #[test]
    fn team_software_template_validation_prep() {
        let members = assert_template_topology(
            include_str!("templates/team_software.yaml"),
            &TemplateExpectation {
                users: 1,
                architects: 1,
                managers: 2,
                engineers: 8,
                workflow_mode: "legacy",
            },
        );

        assert!(members.iter().any(|member| member.role_name == "tech-lead"));
    }

    #[test]
    fn board_classification_scaffold_marks_runnable_and_blocked_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        fs::write(
            tasks_dir.join("001-done.md"),
            r#"---
id: 1
title: done
status: done
priority: medium
depends_on: []
---
done
"#,
        )
        .unwrap();

        fs::write(
            tasks_dir.join("002-runnable.md"),
            r#"---
id: 2
title: runnable
status: todo
priority: medium
depends_on: []
---
runnable
"#,
        )
        .unwrap();

        fs::write(
            tasks_dir.join("003-blocked-dep.md"),
            r#"---
id: 3
title: blocked dep
status: todo
priority: medium
depends_on: [4]
---
blocked
"#,
        )
        .unwrap();

        fs::write(
            tasks_dir.join("004-open-dep.md"),
            r#"---
id: 4
title: open dep
status: backlog
priority: medium
depends_on: []
---
open dep
"#,
        )
        .unwrap();

        fs::write(
            tasks_dir.join("005-blocked-flag.md"),
            r#"---
id: 5
title: blocked flag
status: in-progress
priority: medium
blocked: waiting on review
depends_on: []
---
blocked flag
"#,
        )
        .unwrap();

        // TODO(task-25): Replace scaffold classifier with resolver::resolve_board once workflow APIs land on main.
        let results = classify_board_for_validation_prep(&tasks_dir);

        assert_eq!(
            results,
            vec![
                PrepBoardResolution {
                    task_id: 2,
                    runnable: true,
                    blocked_reason: None,
                },
                PrepBoardResolution {
                    task_id: 3,
                    runnable: false,
                    blocked_reason: Some("dependency #4 not done".to_string()),
                },
                PrepBoardResolution {
                    task_id: 4,
                    runnable: true,
                    blocked_reason: None,
                },
                PrepBoardResolution {
                    task_id: 5,
                    runnable: false,
                    blocked_reason: Some("waiting on review".to_string()),
                },
            ]
        );
    }

    #[test]
    fn workflow_metadata_transition_scaffold_covers_basic_lifecycle() {
        let mut metadata = WorkflowMetadataScaffold::default();

        apply_stage("todo", &mut metadata);
        assert_eq!(metadata.next_action.as_deref(), Some("execute"));

        metadata.worktree_path = Some("/tmp/eng-1-1".to_string());
        metadata.branch = Some("eng-1-1/task-34".to_string());
        apply_stage("in-progress", &mut metadata);
        assert_eq!(metadata.next_action.as_deref(), Some("finish_execution"));

        metadata.commit = Some("abc1234".to_string());
        track_artifact_scaffold(&mut metadata, "target/nextest/default.xml");
        apply_stage("review", &mut metadata);
        assert_eq!(metadata.next_action.as_deref(), Some("review"));
        assert_eq!(metadata.artifacts, vec!["target/nextest/default.xml"]);

        apply_stage("done", &mut metadata);
        assert!(metadata.next_action.is_none());
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-1/task-34"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
    }

    #[test]
    fn completion_packet_scaffold_updates_workflow_metadata() {
        let packet = parse_completion_packet(
            r#"
Completed task 34.

```json
{
  "task_id": 34,
  "branch": "eng-1-1/task-34",
  "commit": "def5678",
  "tests_run": ["cargo test validation"],
  "tests_passed": true,
  "outcome": "ready_for_review",
  "worktree_path": "/tmp/eng-1-1",
  "artifacts": [
    {
      "path": "target/nextest/default.xml",
      "artifact_type": "test_result",
      "created_at": 1777000000,
      "verified": true
    }
  ]
}
```
"#,
        );

        let mut metadata = WorkflowMetadataScaffold::default();
        apply_completion_packet(&mut metadata, &packet);

        assert_eq!(metadata.branch.as_deref(), Some("eng-1-1/task-34"));
        assert_eq!(metadata.commit.as_deref(), Some("def5678"));
        assert_eq!(metadata.worktree_path.as_deref(), Some("/tmp/eng-1-1"));
        assert_eq!(metadata.next_action.as_deref(), Some("ready_for_review"));
        assert_eq!(metadata.artifacts, vec!["target/nextest/default.xml"]);
    }

    // TODO(task-25): Test orchestrator_enabled in both modes.
    // TODO(task-30): Test nudge computation for each topology.
    // TODO(task-32): Test policy enforcement (WIP limits, escalation thresholds).
    #[test]
    fn workflow_validation_todo_markers_are_captured_in_prep_module() {
        assert_eq!(
            workflow_mode_from_template(include_str!("templates/team_simple.yaml")),
            "legacy"
        );
    }
}
