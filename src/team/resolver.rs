#![cfg_attr(not(test), allow(dead_code))]

//! Resolve board tasks into runnable workflow states.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::task::{Task, load_tasks_from_dir};

use super::capability::WorkflowCapability;
use super::config::RoleType;
use super::hierarchy::MemberInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionStatus {
    Runnable,
    Blocked,
    NeedsReview,
    NeedsAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResolution {
    pub task_id: u32,
    pub title: String,
    pub status: ResolutionStatus,
    pub execution_owner: Option<String>,
    pub review_owner: Option<String>,
    pub blocking_reason: Option<String>,
    pub acting_capability: Option<WorkflowCapability>,
}

#[derive(Debug, Default, Deserialize)]
struct WorkflowMetadata {
    #[serde(default)]
    execution_owner: Option<String>,
    #[serde(default)]
    blocked_on: Option<String>,
    #[serde(default)]
    review_owner: Option<String>,
}

pub fn resolve_board(board_dir: &Path, members: &[MemberInstance]) -> Result<Vec<TaskResolution>> {
    let tasks = load_tasks_from_dir(&board_dir.join("tasks"))?;
    let done: HashSet<u32> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "done" | "archived"))
        .map(|task| task.id)
        .collect();

    let mut resolutions = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
    {
        let metadata = load_workflow_metadata(task)?;
        let execution_owner = metadata.execution_owner.clone().or(task.claimed_by.clone());
        let blocking_reason = blocking_reason(task, &metadata, &done);
        let status = if blocking_reason.is_some() {
            ResolutionStatus::Blocked
        } else if task.status == "review" {
            ResolutionStatus::NeedsReview
        } else if matches!(
            task.status.as_str(),
            "todo" | "backlog" | "in-progress" | "runnable"
        ) {
            ResolutionStatus::Runnable
        } else {
            ResolutionStatus::NeedsAction
        };
        resolutions.push(TaskResolution {
            task_id: task.id,
            title: task.title.clone(),
            status,
            execution_owner: execution_owner.clone(),
            review_owner: metadata.review_owner.clone(),
            blocking_reason,
            acting_capability: acting_capability(task, &metadata, status, members, execution_owner),
        });
    }

    resolutions.sort_by_key(|resolution| resolution.task_id);
    Ok(resolutions)
}

pub fn runnable_tasks(resolutions: &[TaskResolution]) -> Vec<TaskResolution> {
    resolutions
        .iter()
        .filter(|resolution| resolution.status == ResolutionStatus::Runnable)
        .cloned()
        .collect()
}

fn acting_capability(
    task: &Task,
    metadata: &WorkflowMetadata,
    status: ResolutionStatus,
    members: &[MemberInstance],
    execution_owner: Option<String>,
) -> Option<WorkflowCapability> {
    match status {
        ResolutionStatus::Blocked => None,
        ResolutionStatus::NeedsReview => {
            if metadata.review_owner.is_some() || has_reviewer(members) {
                Some(WorkflowCapability::Reviewer)
            } else {
                None
            }
        }
        ResolutionStatus::Runnable => {
            if execution_owner.is_some() {
                Some(WorkflowCapability::Executor)
            } else if has_dispatcher(members) {
                Some(WorkflowCapability::Dispatcher)
            } else if has_executor(members) {
                Some(WorkflowCapability::Executor)
            } else {
                None
            }
        }
        ResolutionStatus::NeedsAction => {
            if task.status == "in-progress" && has_executor(members) {
                Some(WorkflowCapability::Executor)
            } else if task.status == "backlog" && has_planner(members) {
                Some(WorkflowCapability::Planner)
            } else if has_dispatcher(members) {
                Some(WorkflowCapability::Dispatcher)
            } else {
                None
            }
        }
    }
}

fn blocking_reason(
    task: &Task,
    metadata: &WorkflowMetadata,
    done: &HashSet<u32>,
) -> Option<String> {
    if let Some(reason) = task.blocked.as_ref() {
        return Some(reason.clone());
    }
    if let Some(reason) = metadata.blocked_on.as_ref() {
        return Some(reason.clone());
    }
    task.depends_on
        .iter()
        .find(|dep_id| !done.contains(dep_id))
        .map(|dep_id| format!("unmet dependency #{dep_id}"))
}

fn load_workflow_metadata(task: &Task) -> Result<WorkflowMetadata> {
    if task.source_path.as_os_str().is_empty() {
        return Ok(WorkflowMetadata::default());
    }

    let content = std::fs::read_to_string(&task.source_path)
        .with_context(|| format!("failed to read task file: {}", task.source_path.display()))?;
    let Some(frontmatter) = content
        .trim_start()
        .strip_prefix("---")
        .and_then(|rest| rest.strip_prefix('\n'))
        .and_then(|rest| rest.split_once("\n---").map(|(frontmatter, _)| frontmatter))
    else {
        return Ok(WorkflowMetadata::default());
    };

    serde_yaml::from_str(frontmatter).context("failed to parse workflow metadata")
}

fn has_planner(members: &[MemberInstance]) -> bool {
    members
        .iter()
        .any(|member| matches!(member.role_type, RoleType::Architect | RoleType::Manager))
        || has_executor(members)
}

fn has_dispatcher(members: &[MemberInstance]) -> bool {
    members
        .iter()
        .any(|member| matches!(member.role_type, RoleType::Manager | RoleType::Architect))
        || has_executor(members)
}

fn has_executor(members: &[MemberInstance]) -> bool {
    members
        .iter()
        .any(|member| member.role_type == RoleType::Engineer)
        || members
            .iter()
            .any(|member| matches!(member.role_type, RoleType::Manager | RoleType::Architect))
}

fn has_reviewer(members: &[MemberInstance]) -> bool {
    members
        .iter()
        .any(|member| matches!(member.role_type, RoleType::Manager | RoleType::Architect))
        || has_executor(members)
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
    fn todo_without_deps_is_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\n");

        let resolutions = resolve_board(
            tmp.path(),
            &members(
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
            ),
        )
        .unwrap();

        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
        assert_eq!(
            resolutions[0].acting_capability,
            Some(WorkflowCapability::Dispatcher)
        );
    }

    #[test]
    fn unmet_dependency_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\n");
        write_task(&tasks_dir, 2, "status: todo\ndepends_on:\n  - 1\n");

        let resolutions = resolve_board(tmp.path(), &members("name: solo\nroles:\n  - name: builder\n    role_type: engineer\n    agent: codex\n")).unwrap();

        assert_eq!(resolutions[1].status, ResolutionStatus::Blocked);
        assert_eq!(
            resolutions[1].blocking_reason.as_deref(),
            Some("unmet dependency #1")
        );
    }

    #[test]
    fn blocked_on_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nblocked_on: waiting-for-review\n",
        );

        let resolutions = resolve_board(tmp.path(), &members("name: solo\nroles:\n  - name: builder\n    role_type: engineer\n    agent: codex\n")).unwrap();

        assert_eq!(resolutions[0].status, ResolutionStatus::Blocked);
        assert_eq!(
            resolutions[0].blocking_reason.as_deref(),
            Some("waiting-for-review")
        );
    }

    #[test]
    fn review_without_disposition_needs_review() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: review\nreview_owner: lead\n");

        let resolutions = resolve_board(
            tmp.path(),
            &members(
                r#"
name: pair
roles:
  - name: lead
    role_type: architect
    agent: claude
  - name: builder
    role_type: engineer
    agent: codex
"#,
            ),
        )
        .unwrap();

        assert_eq!(resolutions[0].status, ResolutionStatus::NeedsReview);
        assert_eq!(
            resolutions[0].acting_capability,
            Some(WorkflowCapability::Reviewer)
        );
    }

    #[test]
    fn runnable_tasks_filters_only_runnable_items() {
        let resolutions = vec![
            TaskResolution {
                task_id: 1,
                title: "Task 1".to_string(),
                status: ResolutionStatus::Runnable,
                execution_owner: None,
                review_owner: None,
                blocking_reason: None,
                acting_capability: Some(WorkflowCapability::Dispatcher),
            },
            TaskResolution {
                task_id: 2,
                title: "Task 2".to_string(),
                status: ResolutionStatus::Blocked,
                execution_owner: None,
                review_owner: None,
                blocking_reason: Some("waiting".to_string()),
                acting_capability: None,
            },
            TaskResolution {
                task_id: 3,
                title: "Task 3".to_string(),
                status: ResolutionStatus::NeedsReview,
                execution_owner: None,
                review_owner: None,
                blocking_reason: None,
                acting_capability: Some(WorkflowCapability::Reviewer),
            },
        ];

        let runnable = runnable_tasks(&resolutions);
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].task_id, 1);
    }
}
