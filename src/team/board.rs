//! Board management — kanban.md rotation of done items to archive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use tracing::info;

use super::errors::BoardError;
use super::test_results::TestResults;
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
    pub test_results: Option<TestResults>,
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
    test_results: Option<TestResults>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    review_blockers: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TaskTimestampFrontmatter {
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    started: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgingThresholds {
    pub stale_in_progress_hours: u64,
    pub aged_todo_hours: u64,
    pub stale_review_hours: u64,
}

impl Default for AgingThresholds {
    fn default() -> Self {
        Self {
            stale_in_progress_hours: 4,
            aged_todo_hours: 48,
            stale_review_hours: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgedTask {
    pub task_id: u32,
    pub title: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskAgingReport {
    pub stale_in_progress: Vec<AgedTask>,
    pub aged_todo: Vec<AgedTask>,
    pub stale_review: Vec<AgedTask>,
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
            test_results: frontmatter.test_results,
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

pub(crate) fn compute_task_aging(
    board_dir: &Path,
    project_root: &Path,
    thresholds: AgingThresholds,
) -> Result<TaskAgingReport> {
    compute_task_aging_at(board_dir, project_root, thresholds, Utc::now())
}

pub(crate) fn compute_task_aging_at(
    board_dir: &Path,
    project_root: &Path,
    thresholds: AgingThresholds,
    now: DateTime<Utc>,
) -> Result<TaskAgingReport> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(TaskAgingReport::default());
    }

    let mut report = TaskAgingReport::default();
    for task in load_tasks_from_dir(&tasks_dir)? {
        match task.status.as_str() {
            "in-progress" | "in_progress" => {
                let age_secs = task_age_from_frontmatter(&task, now, AgeAnchor::Started)?;
                if age_secs >= thresholds.stale_in_progress_hours.saturating_mul(3600)
                    && commits_ahead_of_main(project_root, &task)? == 0
                {
                    report.stale_in_progress.push(AgedTask {
                        task_id: task.id,
                        title: task.title,
                        status: task.status,
                        claimed_by: task.claimed_by,
                        age_secs,
                    });
                }
            }
            "todo" => {
                let age_secs = task_age_from_frontmatter(&task, now, AgeAnchor::Updated)?;
                if age_secs >= thresholds.aged_todo_hours.saturating_mul(3600) {
                    report.aged_todo.push(AgedTask {
                        task_id: task.id,
                        title: task.title,
                        status: task.status,
                        claimed_by: task.claimed_by,
                        age_secs,
                    });
                }
            }
            "review" => {
                let age_secs = task_age_from_frontmatter(&task, now, AgeAnchor::Updated)?;
                if age_secs >= thresholds.stale_review_hours.saturating_mul(3600) {
                    report.stale_review.push(AgedTask {
                        task_id: task.id,
                        title: task.title,
                        status: task.status,
                        claimed_by: task.claimed_by,
                        age_secs,
                    });
                }
            }
            _ => {}
        }
    }

    report
        .stale_in_progress
        .sort_by_key(|entry| (entry.task_id, entry.age_secs));
    report
        .aged_todo
        .sort_by_key(|entry| (entry.task_id, entry.age_secs));
    report
        .stale_review
        .sort_by_key(|entry| (entry.task_id, entry.age_secs));
    Ok(report)
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
    set_optional_value(&mut mapping, "test_results", metadata.test_results.as_ref())?;
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

/// Lifecycle timestamps stored in task frontmatter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskLifecycleTimestamps {
    pub created: Option<DateTime<FixedOffset>>,
    pub started: Option<DateTime<FixedOffset>>,
    pub completed: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Deserialize, Default)]
struct TaskLifecycleFrontmatter {
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    started: Option<String>,
    #[serde(default)]
    completed: Option<String>,
}

pub(crate) fn read_task_lifecycle_timestamps(task_path: &Path) -> Result<TaskLifecycleTimestamps> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, _) = split_task_frontmatter(&content)?;
    let parsed: TaskLifecycleFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;

    Ok(TaskLifecycleTimestamps {
        created: parsed
            .created
            .as_deref()
            .and_then(parse_frontmatter_timestamp),
        started: parsed
            .started
            .as_deref()
            .and_then(parse_frontmatter_timestamp),
        completed: parsed
            .completed
            .as_deref()
            .and_then(parse_frontmatter_timestamp),
    })
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

fn parse_frontmatter_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
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

#[derive(Debug, Clone, Copy)]
enum AgeAnchor {
    Started,
    Updated,
}

fn task_age_from_frontmatter(task: &Task, now: DateTime<Utc>, anchor: AgeAnchor) -> Result<u64> {
    let content = std::fs::read_to_string(&task.source_path)
        .with_context(|| format!("failed to read {}", task.source_path.display()))?;
    let (frontmatter, _) = split_task_frontmatter(&content)?;
    let parsed: TaskTimestampFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse task timestamp frontmatter")?;

    let timestamp = match anchor {
        AgeAnchor::Started => parsed.started.or(parsed.updated).or(parsed.created),
        AgeAnchor::Updated => parsed.updated.or(parsed.started).or(parsed.created),
    };

    Ok(timestamp
        .as_deref()
        .and_then(parse_task_timestamp)
        .map(|value| now.signed_duration_since(value).num_seconds().max(0) as u64)
        .unwrap_or(0))
}

fn parse_task_timestamp(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn commits_ahead_of_main(project_root: &Path, task: &Task) -> Result<u32> {
    if let Some(worktree_path) = task.worktree_path.as_deref() {
        let worktree_dir = resolve_task_path(project_root, worktree_path);
        if worktree_dir.is_dir() {
            return crate::team::git_cmd::rev_list_count(&worktree_dir, "main..HEAD")
                .map_err(Into::into);
        }
    }

    if let Some(owner) = task.claimed_by.as_deref() {
        let worktree_dir = project_root.join(".batty").join("worktrees").join(owner);
        if worktree_dir.is_dir() {
            return crate::team::git_cmd::rev_list_count(&worktree_dir, "main..HEAD")
                .map_err(Into::into);
        }
    }

    if let Some(branch) = task.branch.as_deref()
        && !branch.is_empty()
    {
        return crate::team::git_cmd::rev_list_count(project_root, &format!("main..{branch}"))
            .map_err(Into::into);
    }

    Ok(0)
}

fn resolve_task_path(project_root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
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

fn set_optional_value<T>(mapping: &mut Mapping, key: &str, value: Option<&T>) -> Result<()>
where
    T: serde::Serialize,
{
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(
                key,
                serde_yaml::to_value(value)
                    .context("failed to serialize workflow metadata value")?,
            );
        }
        None => {
            mapping.remove(&key);
        }
    }
    Ok(())
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
    fn read_task_lifecycle_timestamps_parses_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let task_path = tmp.path().join("001-lifecycle.md");
        std::fs::write(
            &task_path,
            "---\nid: 1\ntitle: lifecycle\nstatus: done\npriority: high\ncreated: 2026-04-05T10:00:00-04:00\nstarted: 2026-04-05T11:00:00-04:00\ncompleted: 2026-04-05T12:30:00-04:00\n---\n\nBody.\n",
        )
        .unwrap();

        let timestamps = read_task_lifecycle_timestamps(&task_path).unwrap();
        assert_eq!(
            timestamps.created.unwrap().to_rfc3339(),
            "2026-04-05T10:00:00-04:00"
        );
        assert_eq!(
            timestamps.started.unwrap().to_rfc3339(),
            "2026-04-05T11:00:00-04:00"
        );
        assert_eq!(
            timestamps.completed.unwrap().to_rfc3339(),
            "2026-04-05T12:30:00-04:00"
        );
    }

    #[test]
    fn read_task_lifecycle_timestamps_ignores_invalid_values() {
        let tmp = tempfile::tempdir().unwrap();
        let task_path = tmp.path().join("002-invalid.md");
        std::fs::write(
            &task_path,
            "---\nid: 2\ntitle: lifecycle\nstatus: in-progress\npriority: medium\ncreated: not-a-timestamp\nstarted: 2026-04-05T11:00:00-04:00\n---\n\nBody.\n",
        )
        .unwrap();

        let timestamps = read_task_lifecycle_timestamps(&task_path).unwrap();
        assert!(timestamps.created.is_none());
        assert_eq!(
            timestamps.started.unwrap().to_rfc3339(),
            "2026-04-05T11:00:00-04:00"
        );
        assert!(timestamps.completed.is_none());
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
        assert_eq!(
            metadata.test_results.as_ref().map(|results| results.failed),
            None
        );
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
            test_results: Some(TestResults {
                framework: "cargo".to_string(),
                total: Some(3),
                passed: 2,
                failed: 1,
                ignored: 0,
                failures: vec![super::super::test_results::TestFailure {
                    test_name: "tests::fails".to_string(),
                    message: Some("assertion failed".to_string()),
                    location: Some("src/team/completion.rs:10".to_string()),
                }],
                summary: Some("test result: FAILED. 2 passed; 1 failed; 0 ignored;".to_string()),
            }),
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
        assert!(content.contains("test_results:"));
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
        assert!(!content.contains("test_results:"));
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

    #[test]
    fn archive_preserves_file_content() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "042-done.md",
            42,
            "done",
            Some("2026-03-15T08:00:00+00:00"),
        );

        let original_bytes = std::fs::read(tasks_dir.join("042-done.md")).unwrap();

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        archive_tasks(&board_dir, &tasks, false).unwrap();

        let archived_bytes = std::fs::read(board_dir.join("archive").join("042-done.md")).unwrap();
        assert_eq!(
            original_bytes, archived_bytes,
            "archived file bytes must match original exactly"
        );
    }

    #[test]
    fn archive_summary_counts_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        write_task_file(
            &tasks_dir,
            "010-done.md",
            10,
            "done",
            Some("2026-03-01T00:00:00+00:00"),
        );
        write_task_file(
            &tasks_dir,
            "011-done.md",
            11,
            "done",
            Some("2026-03-02T00:00:00+00:00"),
        );
        write_task_file(
            &tasks_dir,
            "012-done.md",
            12,
            "done",
            Some("2026-03-03T00:00:00+00:00"),
        );

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();
        let done_tasks: Vec<_> = tasks.into_iter().filter(|t| t.status == "done").collect();
        assert_eq!(done_tasks.len(), 3);

        let summary = archive_tasks(&board_dir, &done_tasks, false).unwrap();
        assert_eq!(summary.archived_count, 3);
        assert_eq!(summary.skipped_count, 0);
        assert_eq!(summary.archive_dir, board_dir.join("archive"));
    }

    #[test]
    fn archive_handles_empty_board() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        // Create board dir but no tasks dir — simulates an empty board
        std::fs::create_dir_all(&board_dir).unwrap();

        let empty: Vec<Task> = vec![];
        let summary = archive_tasks(&board_dir, &empty, false).unwrap();
        assert_eq!(summary.archived_count, 0);
        assert_eq!(summary.skipped_count, 0);
        // Archive dir should not be created when there's nothing to archive
        assert!(!board_dir.join("archive").exists());
    }

    #[allow(clippy::too_many_arguments)]
    fn write_timed_task(
        board_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        created: &str,
        started: Option<&str>,
        updated: Option<&str>,
    ) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\ncreated: {created}\n"
        );
        if let Some(started) = started {
            content.push_str(&format!("started: {started}\n"));
        }
        if let Some(updated) = updated {
            content.push_str(&format!("updated: {updated}\n"));
        }
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        content.push_str("class: standard\n---\n\nTask body.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    #[test]
    fn aging_flags_tasks_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let now = DateTime::parse_from_rfc3339("2026-04-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write_timed_task(
            &board_dir,
            1,
            "stale-progress",
            "in-progress",
            Some("eng-1"),
            "2026-04-06T08:00:00Z",
            Some("2026-04-06T08:00:00Z"),
            Some("2026-04-06T08:00:00Z"),
        );
        write_timed_task(
            &board_dir,
            2,
            "aged-todo",
            "todo",
            None,
            "2026-04-04T12:00:00Z",
            None,
            Some("2026-04-04T12:00:00Z"),
        );
        write_timed_task(
            &board_dir,
            3,
            "stale-review",
            "review",
            Some("eng-2"),
            "2026-04-06T11:00:00Z",
            None,
            Some("2026-04-06T11:00:00Z"),
        );

        let report =
            compute_task_aging_at(&board_dir, tmp.path(), AgingThresholds::default(), now).unwrap();

        assert_eq!(report.stale_in_progress.len(), 1);
        assert_eq!(report.stale_in_progress[0].task_id, 1);
        assert_eq!(report.aged_todo.len(), 1);
        assert_eq!(report.aged_todo[0].task_id, 2);
        assert_eq!(report.stale_review.len(), 1);
        assert_eq!(report.stale_review[0].task_id, 3);
    }

    #[test]
    fn aging_ignores_fresh_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let now = DateTime::parse_from_rfc3339("2026-04-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write_timed_task(
            &board_dir,
            1,
            "fresh-progress",
            "in-progress",
            Some("eng-1"),
            "2026-04-06T08:00:01Z",
            Some("2026-04-06T08:00:01Z"),
            Some("2026-04-06T08:00:01Z"),
        );
        write_timed_task(
            &board_dir,
            2,
            "fresh-todo",
            "todo",
            None,
            "2026-04-04T12:00:01Z",
            None,
            Some("2026-04-04T12:00:01Z"),
        );
        write_timed_task(
            &board_dir,
            3,
            "fresh-review",
            "review",
            Some("eng-2"),
            "2026-04-06T11:00:01Z",
            None,
            Some("2026-04-06T11:00:01Z"),
        );

        let report =
            compute_task_aging_at(&board_dir, tmp.path(), AgingThresholds::default(), now).unwrap();

        assert!(report.stale_in_progress.is_empty());
        assert!(report.aged_todo.is_empty());
        assert!(report.stale_review.is_empty());
    }

    #[test]
    fn aging_respects_threshold_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let now = DateTime::parse_from_rfc3339("2026-04-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write_timed_task(
            &board_dir,
            1,
            "progress",
            "in-progress",
            Some("eng-1"),
            "2026-04-06T10:30:00Z",
            Some("2026-04-06T10:30:00Z"),
            Some("2026-04-06T10:30:00Z"),
        );
        write_timed_task(
            &board_dir,
            2,
            "todo",
            "todo",
            None,
            "2026-04-05T12:00:00Z",
            None,
            Some("2026-04-05T12:00:00Z"),
        );
        write_timed_task(
            &board_dir,
            3,
            "review",
            "review",
            Some("eng-2"),
            "2026-04-06T10:30:00Z",
            None,
            Some("2026-04-06T10:30:00Z"),
        );

        let report = compute_task_aging_at(
            &board_dir,
            tmp.path(),
            AgingThresholds {
                stale_in_progress_hours: 1,
                aged_todo_hours: 24,
                stale_review_hours: 1,
            },
            now,
        )
        .unwrap();

        assert_eq!(report.stale_in_progress.len(), 1);
        assert_eq!(report.aged_todo.len(), 1);
        assert_eq!(report.stale_review.len(), 1);
    }
}
