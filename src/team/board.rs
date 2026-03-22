//! Board management — kanban.md rotation of done items to archive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use tracing::info;

use super::errors::BoardError;
use crate::task::{Task, load_tasks_from_dir};

/// Workflow metadata stored in task frontmatter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkflowMetadata {
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub commit: Option<String>,
    pub changed_paths: Vec<String>,
    pub tests_run: Option<bool>,
    pub tests_passed: Option<bool>,
    pub artifacts: Vec<String>,
    pub outcome: Option<String>,
    pub review_blockers: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WorkflowFrontmatter {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    changed_paths: Vec<String>,
    #[serde(default)]
    tests_run: Option<bool>,
    #[serde(default)]
    tests_passed: Option<bool>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    review_blockers: Vec<String>,
}

impl From<WorkflowFrontmatter> for WorkflowMetadata {
    fn from(frontmatter: WorkflowFrontmatter) -> Self {
        Self {
            branch: frontmatter.branch,
            worktree_path: frontmatter.worktree_path,
            commit: frontmatter.commit,
            changed_paths: frontmatter.changed_paths,
            tests_run: frontmatter.tests_run,
            tests_passed: frontmatter.tests_passed,
            artifacts: frontmatter.artifacts,
            outcome: frontmatter.outcome,
            review_blockers: frontmatter.review_blockers,
        }
    }
}

pub(crate) fn read_workflow_metadata(task_path: &Path) -> Result<WorkflowMetadata> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, _) = split_task_frontmatter(&content)?;
    let parsed: WorkflowFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;
    Ok(parsed.into())
}

pub(crate) fn write_workflow_metadata(task_path: &Path, metadata: &WorkflowMetadata) -> Result<()> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, body) = split_task_frontmatter(&content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;

    set_optional_string(&mut mapping, "branch", metadata.branch.as_deref());
    set_optional_string(
        &mut mapping,
        "worktree_path",
        metadata.worktree_path.as_deref(),
    );
    set_optional_string(&mut mapping, "commit", metadata.commit.as_deref());
    set_string_list(&mut mapping, "changed_paths", &metadata.changed_paths);
    set_optional_bool(&mut mapping, "tests_run", metadata.tests_run);
    set_optional_bool(&mut mapping, "tests_passed", metadata.tests_passed);
    set_string_list(&mut mapping, "artifacts", &metadata.artifacts);
    set_optional_string(&mut mapping, "outcome", metadata.outcome.as_deref());
    set_string_list(&mut mapping, "review_blockers", &metadata.review_blockers);

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

/// Summary returned by [`archive_tasks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSummary {
    pub archived_count: usize,
    pub skipped_count: usize,
    pub archive_dir: PathBuf,
}

/// Parse an age threshold string ("7d", "24h", "2w", "0s") into a [`Duration`].
pub fn parse_age_threshold(threshold: &str) -> Result<Duration> {
    let threshold = threshold.trim();
    if threshold.is_empty() {
        bail!("empty age threshold");
    }

    let split_pos = threshold
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(threshold.len());
    let (digits, suffix) = threshold.split_at(split_pos);

    if digits.is_empty() {
        bail!("invalid age threshold: {threshold}");
    }

    let value: u64 = digits
        .parse()
        .with_context(|| format!("invalid age threshold: {threshold}"))?;

    let seconds = match suffix {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86400,
        "w" => value * 86400 * 7,
        _ => bail!("invalid age threshold suffix: {threshold} (expected s, m, h, d, or w)"),
    };

    Ok(Duration::from_secs(seconds))
}

/// List done tasks older than the given age threshold.
pub fn done_tasks_older_than(board_dir: &Path, max_age: Duration) -> Result<Vec<Task>> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        bail!("no tasks directory found at {}", tasks_dir.display());
    }

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let now = Utc::now();
    let cutoff = now - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::zero());

    let matching: Vec<Task> = tasks
        .into_iter()
        .filter(|t| t.status == "done")
        .filter(|t| {
            if max_age.is_zero() {
                return true;
            }
            match &t.completed {
                Some(completed_str) => parse_completed_date(completed_str)
                    .map(|completed| completed < cutoff)
                    .unwrap_or(false),
                None => {
                    // Fall back to filesystem mtime
                    std::fs::metadata(&t.source_path)
                        .and_then(|m| m.modified())
                        .ok()
                        .map(|mtime| {
                            let mtime_dt: DateTime<Utc> = mtime.into();
                            mtime_dt < cutoff
                        })
                        .unwrap_or(false)
                }
            }
        })
        .collect();

    Ok(matching)
}

/// Move task files to archive subdirectory, preserving content unchanged.
pub fn archive_tasks(board_dir: &Path, tasks: &[Task], dry_run: bool) -> Result<ArchiveSummary> {
    let archive_dir = board_dir.join("archive");

    if tasks.is_empty() {
        return Ok(ArchiveSummary {
            archived_count: 0,
            skipped_count: 0,
            archive_dir,
        });
    }

    if !dry_run {
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("failed to create archive dir: {}", archive_dir.display()))?;
    }

    let mut archived = 0usize;
    let skipped = 0usize;

    for task in tasks {
        let source = &task.source_path;
        let file_name = source.file_name().context("task file has no file name")?;
        let dest = archive_dir.join(file_name);

        if dry_run {
            let completed_display = task.completed.as_deref().unwrap_or("unknown date");
            println!(
                "  - {} (done {})",
                file_name.to_string_lossy(),
                completed_display
            );
            archived += 1;
            continue;
        }

        std::fs::rename(source, &dest).with_context(|| {
            format!("failed to move {} to {}", source.display(), dest.display())
        })?;
        archived += 1;
        info!(task_id = task.id, "archived task");
    }

    info!(archived, "archived done tasks");
    Ok(ArchiveSummary {
        archived_count: archived,
        skipped_count: skipped,
        archive_dir,
    })
}

/// Archive done tasks by moving their files from `tasks/` to `archive/`.
///
/// Returns the number of tasks archived. If `older_than` is provided, only
/// tasks completed before that date are archived.
pub fn archive_done_tasks(board_dir: &Path, older_than: Option<&str>) -> Result<u32> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        bail!("no tasks directory found at {}", tasks_dir.display());
    }

    let cutoff = older_than.map(parse_cutoff_date).transpose()?;

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let to_archive: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == "done")
        .filter(|t| match (&cutoff, &t.completed) {
            (Some(cutoff_dt), Some(completed_str)) => parse_completed_date(completed_str)
                .map(|completed| completed < *cutoff_dt)
                .unwrap_or(false),
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect();

    if to_archive.is_empty() {
        return Ok(0);
    }

    let archive_dir = board_dir.join("archive");
    std::fs::create_dir_all(&archive_dir)
        .with_context(|| format!("failed to create archive dir: {}", archive_dir.display()))?;

    let mut count = 0u32;
    for task in &to_archive {
        let source = &task.source_path;
        let file_name = source.file_name().context("task file has no file name")?;
        let dest = archive_dir.join(file_name);

        // Update status to "archived" before moving
        update_task_status(source, "archived")?;

        std::fs::rename(source, &dest).with_context(|| {
            format!("failed to move {} to {}", source.display(), dest.display())
        })?;
        count += 1;
        info!(task_id = task.id, "archived task");
    }

    info!(count, "archived done tasks");
    Ok(count)
}

fn parse_cutoff_date(date_str: &str) -> Result<DateTime<FixedOffset>> {
    // Try YYYY-MM-DD first, treating it as start of day UTC
    if let Ok(naive) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        let dt = naive.and_hms_opt(0, 0, 0).context("invalid date")?;
        return Ok(DateTime::<FixedOffset>::from_naive_utc_and_offset(
            dt,
            FixedOffset::east_opt(0).unwrap(),
        ));
    }
    // Try RFC3339
    DateTime::parse_from_rfc3339(date_str).with_context(|| {
        format!("invalid date format: {date_str} (expected YYYY-MM-DD or RFC3339)")
    })
}

fn parse_completed_date(completed_str: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(completed_str).ok()
}

/// Update the `status` field in a task file's YAML frontmatter.
fn update_task_status(task_path: &Path, new_status: &str) -> Result<()> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, body) = split_task_frontmatter(&content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;

    mapping.insert(
        Value::String("status".to_string()),
        Value::String(new_status.to_string()),
    );

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

/// Rotate done items from kanban.md to kanban-archive.md when the count
/// exceeds `threshold`.
///
/// Done items are lines under the `## Done` section. When the count exceeds
/// the threshold, the oldest items (first in the list) are moved to the
/// archive file.
pub fn rotate_done_items(kanban_path: &Path, archive_path: &Path, threshold: u32) -> Result<u32> {
    let content = std::fs::read_to_string(kanban_path)
        .with_context(|| format!("failed to read {}", kanban_path.display()))?;

    let (before_done, done_items, after_done) = split_done_section(&content);

    if done_items.len() <= threshold as usize {
        return Ok(0);
    }

    let keep_count = threshold as usize;
    let to_archive = &done_items[..done_items.len() - keep_count];
    let to_keep = &done_items[done_items.len() - keep_count..];
    let rotated = to_archive.len() as u32;

    let mut new_kanban = before_done.to_string();
    new_kanban.push_str("## Done\n");
    for item in to_keep {
        new_kanban.push_str(item);
        new_kanban.push('\n');
    }
    if !after_done.is_empty() {
        new_kanban.push_str(after_done);
    }

    std::fs::write(kanban_path, &new_kanban)
        .with_context(|| format!("failed to write {}", kanban_path.display()))?;

    let mut archive_content = if archive_path.exists() {
        std::fs::read_to_string(archive_path)
            .with_context(|| format!("failed to read {}", archive_path.display()))?
    } else {
        "# Kanban Archive\n".to_string()
    };

    if !archive_content.ends_with('\n') {
        archive_content.push('\n');
    }
    for item in to_archive {
        archive_content.push_str(item);
        archive_content.push('\n');
    }

    std::fs::write(archive_path, &archive_content)
        .with_context(|| format!("failed to write {}", archive_path.display()))?;

    info!(rotated, threshold, "rotated done items to archive");
    Ok(rotated)
}

fn split_done_section(content: &str) -> (&str, Vec<&str>, &str) {
    let done_marker = "## Done";
    let Some(done_start) = content.find(done_marker) else {
        return (content, Vec::new(), "");
    };

    let before_done = &content[..done_start];
    let after_marker = &content[done_start + done_marker.len()..];
    let items_start = after_marker
        .find('\n')
        .map(|i| i + 1)
        .unwrap_or(after_marker.len());
    let items_section = &after_marker[items_start..];

    let mut done_items = Vec::new();
    let mut remaining_start = items_section.len();

    for (i, line) in items_section.lines().enumerate() {
        if line.starts_with("## ") && i > 0 {
            remaining_start = items_section
                .find(&format!("\n{line}"))
                .map(|pos| pos + 1)
                .unwrap_or(items_section.len());
            break;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            done_items.push(line);
        }
    }

    let after_done = &items_section[remaining_start..];
    (before_done, done_items, after_done)
}

fn split_task_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(BoardError::InvalidFrontmatter {
            detail: "no opening ---".to_string(),
        }
        .into());
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

fn yaml_key(name: &str) -> Value {
    Value::String(name.to_string())
}

fn set_optional_string(mapping: &mut Mapping, key: &str, value: Option<&str>) {
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(key, Value::String(value.to_string()));
        }
        None => {
            mapping.remove(&key);
        }
    }
}

fn set_optional_bool(mapping: &mut Mapping, key: &str, value: Option<bool>) {
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(key, Value::Bool(value));
        }
        None => {
            mapping.remove(&key);
        }
    }
}

fn set_string_list(mapping: &mut Mapping, key: &str, values: &[String]) {
    let key = yaml_key(key);
    if values.is_empty() {
        mapping.remove(&key);
        return;
    }

    mapping.insert(
        key,
        Value::Sequence(
            values
                .iter()
                .map(|value| Value::String(value.clone()))
                .collect(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_done_section_basic() {
        let content =
            "# Board\n\n## Backlog\n\n## In Progress\n\n## Done\n- item 1\n- item 2\n- item 3\n";
        let (before, items, after) = split_done_section(content);
        assert!(before.contains("## In Progress"));
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "- item 1");
        assert!(after.is_empty());
    }

    #[test]
    fn split_done_section_with_following_section() {
        let content = "## Done\n- a\n- b\n## Archive\nstuff\n";
        let (_, items, after) = split_done_section(content);
        assert_eq!(items.len(), 2);
        assert!(after.contains("## Archive"));
    }

    #[test]
    fn split_done_section_empty() {
        let content = "## Done\n\n## Other\n";
        let (_, items, _) = split_done_section(content);
        assert!(items.is_empty());
    }

    #[test]
    fn split_done_section_no_done_header() {
        let content = "# Board\n## Backlog\n- task\n";
        let (before, items, _) = split_done_section(content);
        assert_eq!(before, content);
        assert!(items.is_empty());
    }

    #[test]
    fn rotate_moves_excess_items() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(
            &kanban,
            "## Backlog\n\n## In Progress\n\n## Done\n- old 1\n- old 2\n- old 3\n- new 1\n- new 2\n",
        )
        .unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 2).unwrap();
        assert_eq!(rotated, 3);

        let kanban_content = std::fs::read_to_string(&kanban).unwrap();
        assert!(kanban_content.contains("- new 1"));
        assert!(kanban_content.contains("- new 2"));
        assert!(!kanban_content.contains("- old 1"));

        let archive_content = std::fs::read_to_string(&archive).unwrap();
        assert!(archive_content.contains("- old 1"));
        assert!(archive_content.contains("- old 2"));
        assert!(archive_content.contains("- old 3"));
    }

    #[test]
    fn rotate_does_nothing_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(&kanban, "## Done\n- item 1\n- item 2\n").unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 5).unwrap();
        assert_eq!(rotated, 0);
        assert!(!archive.exists());
    }

    #[test]
    fn rotate_appends_to_existing_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(&archive, "# Kanban Archive\n- previous\n").unwrap();
        std::fs::write(&kanban, "## Done\n- a\n- b\n- c\n").unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 1).unwrap();
        assert_eq!(rotated, 2);

        let archive_content = std::fs::read_to_string(&archive).unwrap();
        assert!(archive_content.contains("- previous"));
        assert!(archive_content.contains("- a"));
        assert!(archive_content.contains("- b"));
    }

    #[test]
    fn read_workflow_metadata_defaults_when_fields_are_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("027-task.md");
        std::fs::write(
            &task,
            "---\nid: 27\ntitle: Completion packets\nstatus: in-progress\npriority: medium\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        assert_eq!(
            read_workflow_metadata(&task).unwrap(),
            WorkflowMetadata::default()
        );
    }

    #[test]
    fn read_workflow_metadata_parses_all_completion_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("027-task.md");
        std::fs::write(
            &task,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclass: standard\nbranch: eng-1-4/task-27\nworktree_path: .batty/worktrees/eng-1-4\ncommit: abc1234\nchanged_paths:\n  - src/team/completion.rs\ntests_run: true\ntests_passed: false\nartifacts:\n  - docs/workflow.md\noutcome: ready_for_review\nreview_blockers:\n  - missing screenshots\n---\n\nTask body.\n",
        )
        .unwrap();

        let metadata = read_workflow_metadata(&task).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-4/task-27"));
        assert_eq!(
            metadata.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-4")
        );
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(metadata.changed_paths, vec!["src/team/completion.rs"]);
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(false));
        assert_eq!(metadata.artifacts, vec!["docs/workflow.md"]);
        assert_eq!(metadata.outcome.as_deref(), Some("ready_for_review"));
        assert_eq!(metadata.review_blockers, vec!["missing screenshots"]);
    }

    #[test]
    fn write_workflow_metadata_preserves_body_and_other_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("027-task.md");
        std::fs::write(
            &task,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: eng-1-4\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let metadata = WorkflowMetadata {
            branch: Some("eng-1-4/task-27".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1-4".to_string()),
            commit: Some("abc1234".to_string()),
            changed_paths: vec!["src/team/completion.rs".to_string()],
            tests_run: Some(true),
            tests_passed: Some(true),
            artifacts: vec!["docs/workflow.md".to_string()],
            outcome: Some("ready_for_review".to_string()),
            review_blockers: vec!["missing screenshots".to_string()],
        };

        write_workflow_metadata(&task, &metadata).unwrap();

        let content = std::fs::read_to_string(&task).unwrap();
        assert!(content.contains("claimed_by: eng-1-4"));
        assert!(content.contains("branch: eng-1-4/task-27"));
        assert!(content.contains("tests_run: true"));
        assert!(content.contains("tests_passed: true"));
        assert!(content.contains("review_blockers:"));
        assert!(content.contains("Task body."));
        assert_eq!(read_workflow_metadata(&task).unwrap(), metadata);
    }

    #[test]
    fn write_workflow_metadata_removes_empty_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("027-task.md");
        std::fs::write(
            &task,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclass: standard\nbranch: eng-1-4/task-27\nworktree_path: .batty/worktrees/eng-1-4\ncommit: abc1234\nchanged_paths:\n  - src/team/completion.rs\ntests_run: true\ntests_passed: true\nartifacts:\n  - docs/workflow.md\noutcome: ready_for_review\nreview_blockers:\n  - missing screenshots\n---\n\nTask body.\n",
        )
        .unwrap();

        write_workflow_metadata(&task, &WorkflowMetadata::default()).unwrap();

        let content = std::fs::read_to_string(&task).unwrap();
        assert!(!content.contains("branch:"));
        assert!(!content.contains("worktree_path:"));
        assert!(!content.contains("commit:"));
        assert!(!content.contains("changed_paths:"));
        assert!(!content.contains("tests_run:"));
        assert!(!content.contains("tests_passed:"));
        assert!(!content.contains("artifacts:"));
        assert!(!content.contains("outcome:"));
        assert!(!content.contains("review_blockers:"));
        assert!(content.contains("class: standard"));
    }

    fn write_task_file(dir: &Path, filename: &str, id: u32, status: &str, completed: Option<&str>) {
        let completed_line = completed
            .map(|c| format!("completed: {c}\n"))
            .unwrap_or_default();
        let content = format!(
            "---\nid: {id}\ntitle: task {id}\nstatus: {status}\npriority: medium\n{completed_line}class: standard\n---\n\nTask body.\n"
        );
        std::fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn archive_moves_all_done_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00-04:00"),
        );
        write_task_file(&tasks_dir, "002-progress.md", 2, "in-progress", None);
        write_task_file(
            &tasks_dir,
            "003-done.md",
            3,
            "done",
            Some("2026-03-21T10:00:00-04:00"),
        );

        let count = archive_done_tasks(&board_dir, None).unwrap();
        assert_eq!(count, 2);

        // Files moved to archive
        let archive_dir = board_dir.join("archive");
        assert!(archive_dir.join("001-done.md").exists());
        assert!(archive_dir.join("003-done.md").exists());

        // In-progress task stays
        assert!(tasks_dir.join("002-progress.md").exists());
        assert!(!tasks_dir.join("001-done.md").exists());
        assert!(!tasks_dir.join("003-done.md").exists());

        // Archived file has status updated
        let archived = std::fs::read_to_string(archive_dir.join("001-done.md")).unwrap();
        assert!(archived.contains("status: archived"));
    }

    #[test]
    fn archive_with_older_than_filters_by_date() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-old.md",
            1,
            "done",
            Some("2026-03-10T10:00:00-04:00"),
        );
        write_task_file(
            &tasks_dir,
            "002-recent.md",
            2,
            "done",
            Some("2026-03-21T10:00:00-04:00"),
        );

        let count = archive_done_tasks(&board_dir, Some("2026-03-15")).unwrap();
        assert_eq!(count, 1);

        let archive_dir = board_dir.join("archive");
        assert!(archive_dir.join("001-old.md").exists());
        assert!(!archive_dir.join("002-recent.md").exists());
        assert!(tasks_dir.join("002-recent.md").exists());
    }

    #[test]
    fn archive_creates_directory_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00-04:00"),
        );

        let archive_dir = board_dir.join("archive");
        assert!(!archive_dir.exists());

        let count = archive_done_tasks(&board_dir, None).unwrap();
        assert_eq!(count, 1);
        assert!(archive_dir.is_dir());
    }

    #[test]
    fn archive_returns_zero_when_no_done_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(&tasks_dir, "001-progress.md", 1, "in-progress", None);
        write_task_file(&tasks_dir, "002-todo.md", 2, "todo", None);

        let count = archive_done_tasks(&board_dir, None).unwrap();
        assert_eq!(count, 0);
        assert!(!board_dir.join("archive").exists());
    }

    #[test]
    fn archive_skips_done_tasks_without_completed_date_when_older_than_set() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        // Done task with no completed date — should be skipped when --older-than is set
        write_task_file(&tasks_dir, "001-no-date.md", 1, "done", None);
        write_task_file(
            &tasks_dir,
            "002-old.md",
            2,
            "done",
            Some("2026-01-01T00:00:00+00:00"),
        );

        let count = archive_done_tasks(&board_dir, Some("2026-03-01")).unwrap();
        assert_eq!(count, 1);

        assert!(tasks_dir.join("001-no-date.md").exists());
        assert!(board_dir.join("archive/002-old.md").exists());
    }

    #[test]
    fn archive_excludes_tasks_from_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00-04:00"),
        );
        write_task_file(&tasks_dir, "002-todo.md", 2, "todo", None);

        archive_done_tasks(&board_dir, None).unwrap();

        // load_tasks_from_dir only reads from tasks/, not archive/
        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 2);
    }

    #[test]
    fn parse_cutoff_date_accepts_yyyy_mm_dd() {
        let dt = parse_cutoff_date("2026-03-15").unwrap();
        assert_eq!(
            dt.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
        );
    }

    #[test]
    fn parse_cutoff_date_accepts_rfc3339() {
        let dt = parse_cutoff_date("2026-03-15T10:30:00-04:00").unwrap();
        assert_eq!(
            dt.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
        );
    }

    #[test]
    fn parse_cutoff_date_rejects_invalid() {
        assert!(parse_cutoff_date("not-a-date").is_err());
    }

    #[test]
    fn update_task_status_changes_status_field() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("001-task.md");
        std::fs::write(
            &task,
            "---\nid: 1\ntitle: test task\nstatus: done\npriority: medium\n---\n\nBody.\n",
        )
        .unwrap();

        update_task_status(&task, "archived").unwrap();

        let content = std::fs::read_to_string(&task).unwrap();
        assert!(content.contains("status: archived"));
        assert!(!content.contains("status: done"));
        assert!(content.contains("Body."));
    }

    // --- parse_age_threshold tests ---

    #[test]
    fn parse_age_threshold_days() {
        let dur = parse_age_threshold("7d").unwrap();
        assert_eq!(dur, Duration::from_secs(7 * 86400));
    }

    #[test]
    fn parse_age_threshold_hours() {
        let dur = parse_age_threshold("24h").unwrap();
        assert_eq!(dur, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn parse_age_threshold_weeks() {
        let dur = parse_age_threshold("2w").unwrap();
        assert_eq!(dur, Duration::from_secs(14 * 86400));
    }

    #[test]
    fn parse_age_threshold_zero() {
        let dur = parse_age_threshold("0s").unwrap();
        assert_eq!(dur, Duration::from_secs(0));
    }

    #[test]
    fn parse_age_threshold_invalid() {
        assert!(parse_age_threshold("abc").is_err());
    }

    // --- done_tasks_older_than tests ---

    #[test]
    fn done_tasks_older_than_filters_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        // Task completed long ago — should be included
        write_task_file(
            &tasks_dir,
            "001-old.md",
            1,
            "done",
            Some("2020-01-01T00:00:00+00:00"),
        );
        // Task completed very recently — should be excluded with 7d threshold
        let now = Utc::now();
        let recent = now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        write_task_file(&tasks_dir, "002-recent.md", 2, "done", Some(&recent));

        let tasks = done_tasks_older_than(&board_dir, Duration::from_secs(7 * 86400)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
    }

    // --- archive_tasks tests ---

    #[test]
    fn archive_tasks_moves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00+00:00"),
        );

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        let summary = archive_tasks(&board_dir, &tasks, false).unwrap();

        assert_eq!(summary.archived_count, 1);
        assert_eq!(summary.skipped_count, 0);

        // File moved to archive
        let archive_dir = board_dir.join("archive");
        assert!(archive_dir.join("001-done.md").exists());
        assert!(!tasks_dir.join("001-done.md").exists());

        // Content preserved unchanged
        let content = std::fs::read_to_string(archive_dir.join("001-done.md")).unwrap();
        assert!(content.contains("status: done"));
        assert!(content.contains("Task body."));
    }

    #[test]
    fn archive_tasks_creates_archive_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00+00:00"),
        );

        let archive_dir = board_dir.join("archive");
        assert!(!archive_dir.exists());

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        archive_tasks(&board_dir, &tasks, false).unwrap();

        assert!(archive_dir.is_dir());
    }

    #[test]
    fn archive_tasks_dry_run_does_not_move() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "001-done.md",
            1,
            "done",
            Some("2026-03-20T10:00:00+00:00"),
        );

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        let summary = archive_tasks(&board_dir, &tasks, true).unwrap();

        assert_eq!(summary.archived_count, 1);
        // File still in original location
        assert!(tasks_dir.join("001-done.md").exists());
        // Archive dir not created
        assert!(!board_dir.join("archive").exists());
    }

    #[test]
    fn archive_tasks_skips_non_done() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(&tasks_dir, "001-progress.md", 1, "in-progress", None);
        write_task_file(&tasks_dir, "002-todo.md", 2, "todo", None);

        // done_tasks_older_than filters to done only
        let tasks = done_tasks_older_than(&board_dir, Duration::from_secs(0)).unwrap();
        assert!(tasks.is_empty());

        let summary = archive_tasks(&board_dir, &tasks, false).unwrap();
        assert_eq!(summary.archived_count, 0);
        assert!(!board_dir.join("archive").exists());
    }
}
