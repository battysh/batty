use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use crate::task::{Task, load_tasks_from_dir};

use super::board::{read_workflow_metadata, write_workflow_metadata};
use super::workflow::{ReviewDisposition, TaskState, can_transition};

pub fn cmd_transition(board_dir: &Path, task_id: u32, target: &str) -> Result<()> {
    transition_task(board_dir, task_id, target)?;
    println!("Task #{task_id} transitioned to {}.", target.trim());
    Ok(())
}

pub(crate) fn transition_task(board_dir: &Path, task_id: u32, target: &str) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let task = Task::from_file(&task_path)?;
    let current = parse_task_state(&task.status)?;
    let target = parse_task_state(target)?;

    can_transition(current, target).map_err(anyhow::Error::msg)?;

    update_task_frontmatter(&task_path, |mapping| {
        set_status(mapping, target);
        if target != TaskState::Blocked {
            clear_blocked(mapping);
        }
    })?;
    Ok(())
}

pub fn cmd_assign(
    board_dir: &Path,
    task_id: u32,
    exec_owner: Option<&str>,
    review_owner: Option<&str>,
) -> Result<()> {
    if exec_owner.is_none() && review_owner.is_none() {
        bail!("at least one owner must be provided");
    }

    assign_task_owners(board_dir, task_id, exec_owner, review_owner)?;

    println!("Task #{task_id} ownership updated.");
    Ok(())
}

pub(crate) fn assign_task_owners(
    board_dir: &Path,
    task_id: u32,
    exec_owner: Option<&str>,
    review_owner: Option<&str>,
) -> Result<()> {
    if exec_owner.is_none() && review_owner.is_none() {
        bail!("at least one owner must be provided");
    }

    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        if let Some(owner) = exec_owner {
            set_optional_string(mapping, "claimed_by", normalize_optional(owner));
        }
        if let Some(owner) = review_owner {
            set_optional_string(mapping, "review_owner", normalize_optional(owner));
        }
    })?;
    Ok(())
}

pub fn cmd_review(board_dir: &Path, task_id: u32, disposition: &str) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let task = Task::from_file(&task_path)?;
    let current = parse_task_state(&task.status)?;
    let disposition = parse_review_disposition(disposition)?;
    let target = match disposition {
        ReviewDisposition::Approved => TaskState::Done,
        ReviewDisposition::ChangesRequested => TaskState::InProgress,
        ReviewDisposition::Rejected => TaskState::Archived,
    };

    can_transition(current, target).map_err(anyhow::Error::msg)?;

    update_task_frontmatter(&task_path, |mapping| {
        set_status(mapping, target);
        clear_blocked(mapping);
    })?;

    let mut metadata = read_workflow_metadata(&task_path)?;
    metadata.outcome = Some(review_disposition_name(disposition).to_string());
    if disposition == ReviewDisposition::Approved {
        metadata.review_blockers.clear();
    }
    write_workflow_metadata(&task_path, &metadata)?;

    println!(
        "Task #{task_id} review recorded as {}.",
        review_disposition_name(disposition)
    );
    Ok(())
}

pub fn cmd_update(board_dir: &Path, task_id: u32, fields: HashMap<String, String>) -> Result<()> {
    if fields.is_empty() {
        bail!("no workflow fields provided");
    }

    let task_path = find_task_path(board_dir, task_id)?;
    let mut metadata = read_workflow_metadata(&task_path)?;
    let mut metadata_changed = false;

    if let Some(branch) = fields.get("branch") {
        metadata.branch = normalize_optional(branch).map(str::to_string);
        metadata_changed = true;
    }
    if let Some(commit) = fields.get("commit") {
        metadata.commit = normalize_optional(commit).map(str::to_string);
        metadata_changed = true;
    }
    if metadata_changed {
        write_workflow_metadata(&task_path, &metadata)?;
    }

    let blocked_on = fields.get("blocked_on").cloned();
    let should_clear_blocked = fields.contains_key("clear_blocked");
    if blocked_on.is_some() || should_clear_blocked {
        update_task_frontmatter(&task_path, |mapping| {
            if should_clear_blocked {
                clear_blocked(mapping);
            }
            if let Some(reason) = blocked_on.as_deref() {
                let reason = normalize_optional(reason);
                set_optional_string(mapping, "blocked", reason);
                set_optional_string(mapping, "blocked_on", reason);
            }
        })?;
    }

    println!("Task #{task_id} metadata updated.");
    Ok(())
}

fn find_task_path(board_dir: &Path, task_id: u32) -> Result<PathBuf> {
    let tasks_dir = board_dir.join("tasks");
    let tasks = load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
    tasks
        .into_iter()
        .find(|task| task.id == task_id)
        .map(|task| task.source_path)
        .with_context(|| format!("task #{task_id} not found in {}", tasks_dir.display()))
}

fn parse_task_state(value: &str) -> Result<TaskState> {
    match value.trim().replace('-', "_").as_str() {
        "backlog" => Ok(TaskState::Backlog),
        "todo" => Ok(TaskState::Todo),
        "in_progress" => Ok(TaskState::InProgress),
        "review" => Ok(TaskState::Review),
        "blocked" => Ok(TaskState::Blocked),
        "done" => Ok(TaskState::Done),
        "archived" => Ok(TaskState::Archived),
        other => bail!("unknown task state `{other}`"),
    }
}

fn parse_review_disposition(value: &str) -> Result<ReviewDisposition> {
    match value.trim().replace('-', "_").as_str() {
        "approved" => Ok(ReviewDisposition::Approved),
        "changes_requested" => Ok(ReviewDisposition::ChangesRequested),
        "rejected" => Ok(ReviewDisposition::Rejected),
        other => bail!("unknown review disposition `{other}`"),
    }
}

fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Backlog => "backlog",
        TaskState::Todo => "todo",
        TaskState::InProgress => "in-progress",
        TaskState::Review => "review",
        TaskState::Blocked => "blocked",
        TaskState::Done => "done",
        TaskState::Archived => "archived",
    }
}

fn review_disposition_name(disposition: ReviewDisposition) -> &'static str {
    match disposition {
        ReviewDisposition::Approved => "approved",
        ReviewDisposition::ChangesRequested => "changes_requested",
        ReviewDisposition::Rejected => "rejected",
    }
}

fn update_task_frontmatter<F>(task_path: &Path, mutator: F) -> Result<()>
where
    F: FnOnce(&mut Mapping),
{
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, body) = split_task_frontmatter(&content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;
    mutator(&mut mapping);

    let mut rendered =
        serde_yaml::to_string(&mapping).context("failed to serialize task frontmatter")?;
    if let Some(stripped) = rendered.strip_prefix("---\n") {
        rendered = stripped.to_string();
    }

    let mut updated = String::from("---\n");
    updated.push_str(&rendered);
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("---\n");
    updated.push_str(body);

    std::fs::write(task_path, updated)
        .with_context(|| format!("failed to write {}", task_path.display()))?;
    Ok(())
}

fn split_task_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        bail!("task file missing YAML frontmatter (no opening ---)");
    }

    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let close_pos = after_open
        .find("\n---")
        .context("task file missing closing --- for frontmatter")?;

    let frontmatter = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];
    Ok((frontmatter, body.strip_prefix('\n').unwrap_or(body)))
}

fn set_status(mapping: &mut Mapping, state: TaskState) {
    mapping.insert(
        yaml_key("status"),
        Value::String(state_name(state).to_string()),
    );
}

fn clear_blocked(mapping: &mut Mapping) {
    mapping.remove(yaml_key("blocked"));
    mapping.remove(yaml_key("blocked_on"));
}

fn set_optional_string(mapping: &mut Mapping, key: &str, value: Option<&str>) {
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(key, Value::String(value.to_string()));
        }
        None => {
            mapping.remove(key);
        }
    }
}

fn yaml_key(name: &str) -> Value {
    Value::String(name.to_string())
}

fn normalize_optional(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task_file(dir: &Path, id: u32, status: &str) -> PathBuf {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: high\nclass: standard\n---\n\nTask body.\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn transition_updates_task_status() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 7, "todo");

        cmd_transition(board_dir, 7, "in-progress").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "in-progress");
    }

    #[test]
    fn illegal_transition_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 8, "backlog");

        let error = cmd_transition(board_dir, 8, "done")
            .unwrap_err()
            .to_string();
        assert!(error.contains("illegal task state transition"));
    }

    #[test]
    fn assign_updates_execution_and_review_owners() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 9, "todo");

        cmd_assign(board_dir, 9, Some("eng-1-2"), Some("manager-1")).unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1-2"));
        assert_eq!(task.review_owner.as_deref(), Some("manager-1"));
    }

    #[test]
    fn review_updates_status_and_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 10, "review");

        cmd_review(board_dir, 10, "approved").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "done");
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.outcome.as_deref(), Some("approved"));
    }

    #[test]
    fn update_writes_board_metadata_and_block_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 11, "blocked");

        let fields = HashMap::from([
            ("branch".to_string(), "eng-1-2/task-11".to_string()),
            ("commit".to_string(), "abc1234".to_string()),
            ("blocked_on".to_string(), "waiting for review".to_string()),
        ]);
        cmd_update(board_dir, 11, fields).unwrap();

        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-2/task-11"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.blocked.as_deref(), Some("waiting for review"));
        assert_eq!(task.blocked_on.as_deref(), Some("waiting for review"));

        cmd_update(
            board_dir,
            11,
            HashMap::from([("clear_blocked".to_string(), "true".to_string())]),
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert!(task.blocked.is_none());
        assert!(task.blocked_on.is_none());
    }

    #[test]
    fn update_requires_at_least_one_field() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 12, "todo");

        let error = cmd_update(board_dir, 12, HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no workflow fields provided"));
    }

    #[test]
    fn task_commands_work_without_orchestrator_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 13, "todo");

        cmd_assign(board_dir, 13, Some("eng-1-2"), Some("manager-1")).unwrap();
        cmd_transition(board_dir, 13, "in-progress").unwrap();
        cmd_transition(board_dir, 13, "review").unwrap();
        cmd_update(
            board_dir,
            13,
            HashMap::from([
                ("branch".to_string(), "eng-1-2/task-13".to_string()),
                ("commit".to_string(), "deadbeef".to_string()),
            ]),
        )
        .unwrap();
        cmd_review(board_dir, 13, "approved").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(task.status, "done");
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1-2"));
        assert_eq!(task.review_owner.as_deref(), Some("manager-1"));
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-2/task-13"));
        assert_eq!(metadata.commit.as_deref(), Some("deadbeef"));
        assert_eq!(metadata.outcome.as_deref(), Some("approved"));
    }
}
