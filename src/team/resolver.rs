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

pub fn dispatchable_tasks(board_dir: &Path) -> Result<Vec<Task>> {
    let tasks = load_tasks_from_dir(&board_dir.join("tasks"))?;
    let done: HashSet<u32> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "done" | "archived"))
        .map(|task| task.id)
        .collect();

    Ok(tasks
        .into_iter()
        .filter(|task| is_dispatchable_task(task, &done))
        .collect())
}

pub fn is_dispatchable_task(task: &Task, done: &HashSet<u32>) -> bool {
    if !matches!(task.status.as_str(), "todo" | "backlog" | "runnable") {
        return false;
    }
    if task.claimed_by.is_some() {
        return false;
    }
    let metadata = load_workflow_metadata(task).unwrap_or_default();
    blocking_reason(task, &metadata, done).is_none()
}

/// Subset of `dispatchable_tasks` that excludes tasks already committed to a
/// non-engineer member (architect/manager/writer digest) via `assignee:` or a
/// body `**Owner:** <role>` declaration. Mirrors the same filters the engineer
/// dispatcher applies in `available_dispatch_tasks` (#682 + #703).
///
/// #709: previously the dispatch-gap intervention + architect planning cycle
/// used `dispatchable_tasks()` directly and counted maya-owned strategy tasks
/// as "unassigned open tasks", firing the planning cycle on false premises
/// and nagging jordan-pm to dispatch work that could not be dispatched.
/// Observed in batty-marketing 2026-04-17: "third planning-cycle trigger
/// in ~90min on stale 'idle engineers' diagnosis" (jordan-pm's own note on
/// task #573). Each trigger burned one Maya turn (~5k+ tokens).
pub fn engineer_dispatchable_tasks(
    board_dir: &Path,
    members: &[MemberInstance],
) -> Result<Vec<Task>> {
    let non_engineer_names: HashSet<String> = members
        .iter()
        .filter(|member| member.role_type != RoleType::Engineer)
        .map(|member| member.name.clone())
        .collect();
    Ok(dispatchable_tasks(board_dir)?
        .into_iter()
        .filter(|task| is_engineer_dispatchable(task, &non_engineer_names))
        .collect())
}

pub fn is_engineer_dispatchable(task: &Task, non_engineer_names: &HashSet<String>) -> bool {
    if let Some(ref assignee) = task.assignee {
        return !non_engineer_names.contains(assignee);
    }
    match crate::team::daemon::dispatch::queue::parse_body_owner_role(&task.description) {
        Some(owner) => !non_engineer_names.contains(&owner),
        None => true,
    }
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
    if task.is_schedule_blocked() {
        return Some(format!(
            "scheduled for {}",
            task.scheduled_for.as_deref().unwrap_or("unknown")
        ));
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

    fn solo_members() -> Vec<MemberInstance> {
        members(
            "name: solo\nroles:\n  - name: builder\n    role_type: engineer\n    agent: codex\n",
        )
    }

    #[test]
    fn scheduled_future_is_blocked() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            &format!("status: todo\nscheduled_for: \"{future}\"\n"),
        );

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Blocked);
        assert!(
            resolutions[0]
                .blocking_reason
                .as_ref()
                .unwrap()
                .contains("scheduled for")
        );
    }

    #[test]
    fn scheduled_past_is_runnable() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            &format!("status: todo\nscheduled_for: \"{past}\"\n"),
        );

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
        assert!(resolutions[0].blocking_reason.is_none());
    }

    #[test]
    fn no_scheduled_for_is_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
        assert!(resolutions[0].blocking_reason.is_none());
    }

    #[test]
    fn scheduled_just_passed_is_runnable() {
        let just_passed = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            &format!("status: todo\nscheduled_for: \"{just_passed}\"\n"),
        );

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
        assert!(resolutions[0].blocking_reason.is_none());
    }

    // --- done tasks are excluded ---

    #[test]
    fn done_tasks_excluded_from_resolutions() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: done\n");
        write_task(&tasks_dir, 2, "status: todo\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].task_id, 2);
    }

    #[test]
    fn archived_tasks_excluded_from_resolutions() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: archived\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert!(resolutions.is_empty());
    }

    // --- dependency resolution ---

    #[test]
    fn all_deps_met_makes_task_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: done\n");
        write_task(&tasks_dir, 2, "status: done\n");
        write_task(&tasks_dir, 3, "status: todo\ndepends_on:\n  - 1\n  - 2\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].task_id, 3);
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
        assert!(resolutions[0].blocking_reason.is_none());
    }

    #[test]
    fn partial_deps_met_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: done\n");
        write_task(&tasks_dir, 2, "status: todo\n");
        write_task(&tasks_dir, 3, "status: todo\ndepends_on:\n  - 1\n  - 2\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        let task3 = resolutions.iter().find(|r| r.task_id == 3).unwrap();
        assert_eq!(task3.status, ResolutionStatus::Blocked);
        assert_eq!(
            task3.blocking_reason.as_deref(),
            Some("unmet dependency #2")
        );
    }

    #[test]
    fn diamond_dependency_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        // Diamond: 4 depends on 2,3; both 2,3 depend on 1
        write_task(&tasks_dir, 1, "status: done\n");
        write_task(&tasks_dir, 2, "status: done\ndepends_on:\n  - 1\n");
        write_task(&tasks_dir, 3, "status: done\ndepends_on:\n  - 1\n");
        write_task(&tasks_dir, 4, "status: todo\ndepends_on:\n  - 2\n  - 3\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].task_id, 4);
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
    }

    // --- empty board ---

    #[test]
    fn empty_board_returns_no_resolutions() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert!(resolutions.is_empty());
    }

    // --- execution_owner fallback ---

    #[test]
    fn execution_owner_falls_back_to_claimed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\nclaimed_by: eng-1-1\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].execution_owner.as_deref(), Some("eng-1-1"));
    }

    // --- blocked field ---

    #[test]
    fn task_with_blocked_field_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\nblocked: waiting-for-api\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Blocked);
        assert_eq!(
            resolutions[0].blocking_reason.as_deref(),
            Some("waiting-for-api")
        );
    }

    // --- status variations ---

    #[test]
    fn backlog_status_is_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: backlog\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
    }

    #[test]
    fn in_progress_is_runnable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: in-progress\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::Runnable);
    }

    #[test]
    fn unknown_status_is_needs_action() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: custom-status\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        assert_eq!(resolutions[0].status, ResolutionStatus::NeedsAction);
    }

    // --- capability resolution ---

    #[test]
    fn runnable_with_owner_gets_executor_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: todo\nclaimed_by: builder-1-1\n");

        let resolutions = resolve_board(
            tmp.path(),
            &members(
                "name: team\nroles:\n  - name: lead\n    role_type: manager\n    agent: claude\n  - name: builder\n    role_type: engineer\n    agent: codex\n",
            ),
        )
        .unwrap();
        assert_eq!(
            resolutions[0].acting_capability,
            Some(WorkflowCapability::Executor)
        );
    }

    #[test]
    fn resolutions_sorted_by_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 5, "status: todo\n");
        write_task(&tasks_dir, 2, "status: todo\n");
        write_task(&tasks_dir, 8, "status: todo\n");

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        let ids: Vec<u32> = resolutions.iter().map(|r| r.task_id).collect();
        assert_eq!(ids, vec![2, 5, 8]);
    }

    // --- blocked takes priority over deps ---

    #[test]
    fn blocked_field_takes_priority_over_dependency_check() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "status: done\n");
        write_task(
            &tasks_dir,
            2,
            "status: todo\nblocked: manual-hold\ndepends_on:\n  - 1\n",
        );

        let resolutions = resolve_board(tmp.path(), &solo_members()).unwrap();
        let task2 = resolutions.iter().find(|r| r.task_id == 2).unwrap();
        assert_eq!(task2.status, ResolutionStatus::Blocked);
        assert_eq!(task2.blocking_reason.as_deref(), Some("manual-hold"));
    }

    #[test]
    fn blocked_todo_is_not_dispatchable() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(
            &tasks_dir,
            1,
            "status: todo\nblocked: waiting on manual token rotation\n",
        );

        let tasks = dispatchable_tasks(tmp.path()).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn engineer_dispatchable_filters_out_maya_owned_body_tasks() {
        // Regression for #709: Maya's planner used to count planner/manager-owned
        // tasks as engineer-dispatchable, triggering spurious planning cycles.
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let team = members(
            r#"
name: mixed
roles:
  - name: maya-planner
    role_type: architect
    agent: codex
    instances: 1
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        std::fs::write(
            tasks_dir.join("001-eng.md"),
            "---\nid: 1\ntitle: engineer work\nstatus: todo\npriority: medium\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("002-maya.md"),
            "---\nid: 2\ntitle: planning task\nstatus: todo\npriority: medium\nclass: standard\n---\n\nOwner: maya-planner handles this.\n",
        )
        .unwrap();

        let all = dispatchable_tasks(tmp.path()).unwrap();
        assert_eq!(all.len(), 2);
        let engineer_only = engineer_dispatchable_tasks(tmp.path(), &team).unwrap();
        assert_eq!(engineer_only.len(), 1);
        assert_eq!(engineer_only[0].id, 1);
    }

    #[test]
    fn engineer_dispatchable_filters_explicit_non_engineer_assignee() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let team = members(
            r#"
name: mixed
roles:
  - name: maya-planner
    role_type: architect
    agent: codex
    instances: 1
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 1
"#,
        );

        std::fs::write(
            tasks_dir.join("001-eng.md"),
            "---\nid: 1\ntitle: engineer work\nstatus: todo\npriority: medium\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("002-maya.md"),
            "---\nid: 2\ntitle: planning task\nstatus: todo\npriority: medium\nassignee: maya-planner\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();

        let engineer_only = engineer_dispatchable_tasks(tmp.path(), &team).unwrap();
        assert_eq!(engineer_only.len(), 1);
        assert_eq!(engineer_only[0].id, 1);
    }
}
