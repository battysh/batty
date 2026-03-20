//! Board management — kanban.md rotation of done items to archive.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use tracing::info;

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
}
