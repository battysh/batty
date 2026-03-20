use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

use crate::task;

use super::config::RoleType;
use super::hierarchy::MemberInstance;
use super::inbox;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowMetrics {
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub in_progress_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
}

pub fn compute_metrics(board_dir: &Path, members: &[MemberInstance]) -> Result<WorkflowMetrics> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(WorkflowMetrics::default());
    }

    let tasks = task::load_tasks_from_dir(&tasks_dir)?;
    if tasks.is_empty() {
        return Ok(WorkflowMetrics::default());
    }

    let task_status_by_id: HashMap<u32, String> = tasks
        .iter()
        .map(|task| (task.id, task.status.clone()))
        .collect();

    let now = SystemTime::now();
    let runnable_count = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
        .filter(|task| task.claimed_by.is_none())
        .filter(|task| task.blocked.is_none())
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| status == "done")
            })
        })
        .count() as u32;

    let blocked_count = tasks
        .iter()
        .filter(|task| task.status == "blocked" || task.blocked.is_some())
        .count() as u32;
    let in_review_count = tasks.iter().filter(|task| task.status == "review").count() as u32;
    let in_progress_count = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
        .count() as u32;

    let oldest_review_age_secs = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();
    let oldest_assignment_age_secs = tasks
        .iter()
        .filter(|task| task.claimed_by.is_some())
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();

    let idle_with_runnable = compute_idle_with_runnable(board_dir, members, &tasks, runnable_count);

    Ok(WorkflowMetrics {
        runnable_count,
        blocked_count,
        in_review_count,
        in_progress_count,
        idle_with_runnable,
        oldest_review_age_secs,
        oldest_assignment_age_secs,
    })
}

pub fn format_metrics(metrics: &WorkflowMetrics) -> String {
    let idle = if metrics.idle_with_runnable.is_empty() {
        "-".to_string()
    } else {
        metrics.idle_with_runnable.join(", ")
    };

    format!(
        "Workflow Metrics\n\
Runnable: {}\n\
Blocked: {}\n\
In Review: {}\n\
In Progress: {}\n\
Idle With Runnable: {}\n\
Oldest Review Age: {}\n\
Oldest Assignment Age: {}",
        metrics.runnable_count,
        metrics.blocked_count,
        metrics.in_review_count,
        metrics.in_progress_count,
        idle,
        format_age(metrics.oldest_review_age_secs),
        format_age(metrics.oldest_assignment_age_secs),
    )
}

fn compute_idle_with_runnable(
    board_dir: &Path,
    members: &[MemberInstance],
    tasks: &[task::Task],
    runnable_count: u32,
) -> Vec<String> {
    if runnable_count == 0 {
        return Vec::new();
    }

    let busy_engineers: HashSet<&str> = tasks
        .iter()
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| task.claimed_by.as_deref())
        .collect();

    let pending_root = project_root_from_board_dir(board_dir).map(inbox::inboxes_root);
    let mut idle = members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .filter(|member| !busy_engineers.contains(member.name.as_str()))
        .filter(|member| {
            pending_root
                .as_ref()
                .and_then(|root| inbox::pending_message_count(root, &member.name).ok())
                .unwrap_or(0)
                == 0
        })
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    idle.sort();
    idle
}

fn project_root_from_board_dir(board_dir: &Path) -> Option<&Path> {
    board_dir.parent()?.parent()?.parent()
}

fn file_age_secs(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified)
        .ok()
        .map(|duration| duration.as_secs())
}

fn format_age(age_secs: Option<u64>) -> String {
    age_secs
        .map(|secs| format!("{secs}s"))
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_member(name: &str, role_type: RoleType) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }
    }

    fn write_task(
        board_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        blocked: Option<&str>,
        depends_on: &[u32],
    ) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if let Some(blocked) = blocked {
            content.push_str(&format!("blocked: {blocked}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("  - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask body.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    #[test]
    fn compute_metrics_handles_empty_board() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();

        let metrics = compute_metrics(&board_dir, &[]).unwrap();
        assert_eq!(metrics, WorkflowMetrics::default());
    }

    #[test]
    fn compute_metrics_counts_mixed_workflow_states() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        write_task(&board_dir, 1, "done-dep", "done", None, None, &[]);
        write_task(&board_dir, 2, "runnable", "todo", None, None, &[1]);
        write_task(
            &board_dir,
            3,
            "blocked",
            "blocked",
            Some("eng-1"),
            Some("waiting"),
            &[],
        );
        write_task(&board_dir, 4, "review", "review", Some("eng-2"), None, &[]);
        write_task(
            &board_dir,
            5,
            "active",
            "in-progress",
            Some("eng-1"),
            None,
            &[],
        );

        let members = vec![
            make_member("eng-1", RoleType::Engineer),
            make_member("eng-2", RoleType::Engineer),
            make_member("eng-3", RoleType::Engineer),
        ];
        let metrics = compute_metrics(&board_dir, &members).unwrap();

        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
        assert_eq!(metrics.in_review_count, 1);
        assert_eq!(metrics.in_progress_count, 1);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-3"]);
        assert!(metrics.oldest_review_age_secs.is_some());
        assert!(metrics.oldest_assignment_age_secs.is_some());
    }

    #[test]
    fn format_metrics_produces_readable_summary() {
        let text = format_metrics(&WorkflowMetrics {
            runnable_count: 2,
            blocked_count: 1,
            in_review_count: 3,
            in_progress_count: 4,
            idle_with_runnable: vec!["eng-1".to_string(), "eng-2".to_string()],
            oldest_review_age_secs: Some(90),
            oldest_assignment_age_secs: None,
        });

        assert!(text.contains("Workflow Metrics"));
        assert!(text.contains("Runnable: 2"));
        assert!(text.contains("Idle With Runnable: eng-1, eng-2"));
        assert!(text.contains("Oldest Review Age: 90s"));
        assert!(text.contains("Oldest Assignment Age: n/a"));
    }
}
