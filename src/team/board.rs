//! Board management — kanban.md rotation of done items to archive.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use tracing::info;

/// Workflow metadata stored in task frontmatter.
///
/// All fields are optional and default to empty so existing kanban-md task
/// files remain valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkflowMetadata {
    pub depends_on: Vec<u32>,
    pub review_owner: Option<String>,
    pub blocked_on: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub artifacts: Vec<String>,
    pub next_action: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WorkflowFrontmatter {
    #[serde(default)]
    depends_on: Vec<u32>,
    #[serde(default)]
    review_owner: Option<String>,
    #[serde(default)]
    blocked_on: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    next_action: Option<String>,
}

impl From<WorkflowFrontmatter> for WorkflowMetadata {
    fn from(frontmatter: WorkflowFrontmatter) -> Self {
        Self {
            depends_on: frontmatter.depends_on,
            review_owner: frontmatter.review_owner,
            blocked_on: frontmatter.blocked_on,
            worktree_path: frontmatter.worktree_path,
            branch: frontmatter.branch,
            commit: frontmatter.commit,
            artifacts: frontmatter.artifacts,
            next_action: frontmatter.next_action,
        }
    }
}

/// Read workflow metadata from a task file frontmatter block.
pub(crate) fn read_workflow_metadata(task_path: &Path) -> Result<WorkflowMetadata> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, _) = split_task_frontmatter(&content)?;
    let parsed: WorkflowFrontmatter =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;
    Ok(parsed.into())
}

/// Update workflow metadata in a task file while preserving other frontmatter keys.
pub(crate) fn write_workflow_metadata(task_path: &Path, metadata: &WorkflowMetadata) -> Result<()> {
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, body) = split_task_frontmatter(&content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;

    set_u32_list(&mut mapping, "depends_on", &metadata.depends_on);
    set_optional_string(
        &mut mapping,
        "review_owner",
        metadata.review_owner.as_deref(),
    );
    set_optional_string(&mut mapping, "blocked_on", metadata.blocked_on.as_deref());
    set_optional_string(
        &mut mapping,
        "worktree_path",
        metadata.worktree_path.as_deref(),
    );
    set_optional_string(&mut mapping, "branch", metadata.branch.as_deref());
    set_optional_string(&mut mapping, "commit", metadata.commit.as_deref());
    set_string_list(&mut mapping, "artifacts", &metadata.artifacts);
    set_optional_string(&mut mapping, "next_action", metadata.next_action.as_deref());

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

    // Move excess items (oldest = first in list) to archive
    let keep_count = threshold as usize;
    let to_archive = &done_items[..done_items.len() - keep_count];
    let to_keep = &done_items[done_items.len() - keep_count..];
    let rotated = to_archive.len() as u32;

    // Rebuild kanban
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

    // Append to archive
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

/// Split kanban content into (before_done, done_items, after_done).
fn split_done_section(content: &str) -> (&str, Vec<&str>, &str) {
    let done_marker = "## Done";
    let Some(done_start) = content.find(done_marker) else {
        return (content, Vec::new(), "");
    };

    let before_done = &content[..done_start];
    let after_marker = &content[done_start + done_marker.len()..];

    // Skip the newline after "## Done"
    let items_start = after_marker
        .find('\n')
        .map(|i| i + 1)
        .unwrap_or(after_marker.len());
    let items_section = &after_marker[items_start..];

    // Find the next section header (## Something)
    let mut done_items = Vec::new();
    let mut remaining_start = items_section.len();

    for (i, line) in items_section.lines().enumerate() {
        if line.starts_with("## ") && i > 0 {
            // Found next section — compute byte offset
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
        anyhow::bail!("task file missing YAML frontmatter (no opening ---)");
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

fn set_u32_list(mapping: &mut Mapping, key: &str, values: &[u32]) {
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
                .map(|value| Value::Number((*value).into()))
                .collect(),
        ),
    );
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
        let task = tmp.path().join("001-task.md");
        std::fs::write(
            &task,
            "---\nid: 1\ntitle: Task\nstatus: backlog\npriority: high\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let metadata = read_workflow_metadata(&task).unwrap();
        assert_eq!(metadata, WorkflowMetadata::default());
    }

    #[test]
    fn read_workflow_metadata_parses_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("020-task.md");
        std::fs::write(
            &task,
            "---\nid: 20\ntitle: Workflow\nstatus: review\npriority: critical\nclass: standard\ndepends_on:\n  - 18\n  - 19\nreview_owner: manager\nblocked_on: waiting-for-tests\nworktree_path: .batty/worktrees/eng-1-3\nbranch: eng-1-3/task-20\ncommit: abc1234\nartifacts:\n  - target/debug/batty\n  - docs/workflow.md\nnext_action: Hand off to manager\n---\n\nTask body.\n",
        )
        .unwrap();

        let metadata = read_workflow_metadata(&task).unwrap();
        assert_eq!(metadata.depends_on, vec![18, 19]);
        assert_eq!(metadata.review_owner.as_deref(), Some("manager"));
        assert_eq!(metadata.blocked_on.as_deref(), Some("waiting-for-tests"));
        assert_eq!(
            metadata.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-3")
        );
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-3/task-20"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(
            metadata.artifacts,
            vec!["target/debug/batty", "docs/workflow.md"]
        );
        assert_eq!(metadata.next_action.as_deref(), Some("Hand off to manager"));
    }

    #[test]
    fn write_workflow_metadata_preserves_body_and_other_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("020-task.md");
        std::fs::write(
            &task,
            "---\nid: 20\ntitle: Workflow\nstatus: review\npriority: critical\nclass: standard\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
        )
        .unwrap();

        let metadata = WorkflowMetadata {
            depends_on: vec![18, 19],
            review_owner: Some("manager".to_string()),
            blocked_on: Some("waiting-for-tests".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1-3".to_string()),
            branch: Some("eng-1-3/task-20".to_string()),
            commit: Some("abc1234".to_string()),
            artifacts: vec!["target/debug/batty".to_string()],
            next_action: Some("Hand off to manager".to_string()),
        };

        write_workflow_metadata(&task, &metadata).unwrap();

        let content = std::fs::read_to_string(&task).unwrap();
        assert!(content.contains("class: standard"));
        assert!(content.contains("claimed_by: eng-1-3"));
        assert!(content.contains("review_owner: manager"));
        assert!(content.contains("blocked_on: waiting-for-tests"));
        assert!(content.contains("Task body."));
        assert_eq!(read_workflow_metadata(&task).unwrap(), metadata);
    }

    #[test]
    fn write_workflow_metadata_removes_empty_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let task = tmp.path().join("020-task.md");
        std::fs::write(
            &task,
            "---\nid: 20\ntitle: Workflow\nstatus: review\npriority: critical\nclass: standard\ndepends_on:\n  - 18\nreview_owner: manager\nblocked_on: waiting-for-tests\nworktree_path: .batty/worktrees/eng-1-3\nbranch: eng-1-3/task-20\ncommit: abc1234\nartifacts:\n  - target/debug/batty\nnext_action: Hand off to manager\n---\n\nTask body.\n",
        )
        .unwrap();

        write_workflow_metadata(&task, &WorkflowMetadata::default()).unwrap();

        let content = std::fs::read_to_string(&task).unwrap();
        assert!(!content.contains("depends_on:"));
        assert!(!content.contains("review_owner:"));
        assert!(!content.contains("blocked_on:"));
        assert!(!content.contains("worktree_path:"));
        assert!(!content.contains("branch:"));
        assert!(!content.contains("commit:"));
        assert!(!content.contains("artifacts:"));
        assert!(!content.contains("next_action:"));
        assert!(content.contains("class: standard"));
        assert_eq!(
            read_workflow_metadata(&task).unwrap(),
            WorkflowMetadata::default()
        );
    }
}
