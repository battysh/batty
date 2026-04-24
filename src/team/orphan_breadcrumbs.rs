//! Best-effort recovery breadcrumbs for orphan task demotion.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_yaml::{Mapping, Value};
use tracing::warn;

use crate::task::Task;

use super::git_cmd;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrphanBreadcrumb {
    pub(crate) artifact_path: PathBuf,
    pub(crate) artifact_ref: String,
}

pub(crate) fn capture_orphan_demotion_breadcrumb_best_effort(
    project_root: &Path,
    board_dir: &Path,
    task: &Task,
    source: &str,
) -> Option<OrphanBreadcrumb> {
    match capture_orphan_demotion_breadcrumb(project_root, board_dir, task, source) {
        Ok(breadcrumb) => Some(breadcrumb),
        Err(error) => {
            warn!(
                task_id = task.id,
                source,
                error = %error,
                "failed to capture orphan demotion breadcrumbs; continuing demotion"
            );
            None
        }
    }
}

pub(crate) fn capture_orphan_demotion_breadcrumb(
    project_root: &Path,
    _board_dir: &Path,
    task: &Task,
    source: &str,
) -> Result<OrphanBreadcrumb> {
    let artifact_dir = project_root
        .join(".batty")
        .join("recovery")
        .join("orphan-breadcrumbs");
    std::fs::create_dir_all(&artifact_dir)
        .with_context(|| format!("failed to create {}", artifact_dir.display()))?;

    let timestamp = Utc::now();
    let stamp = timestamp.format("%Y%m%dT%H%M%SZ");
    let artifact_path = artifact_dir.join(format!("task-{}-{stamp}.md", task.id));
    let artifact_ref = artifact_path
        .strip_prefix(project_root)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| artifact_path.to_string_lossy().to_string());

    let worktree_path = infer_worktree_path(project_root, task);
    let snapshot = WorktreeSnapshot::capture(worktree_path.as_deref(), task);
    let content = render_breadcrumb(task, source, &timestamp.to_rfc3339(), &snapshot);
    std::fs::write(&artifact_path, content)
        .with_context(|| format!("failed to write {}", artifact_path.display()))?;

    append_artifact_ref(&task.source_path, &artifact_ref)?;

    Ok(OrphanBreadcrumb {
        artifact_path,
        artifact_ref,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeSnapshot {
    path: Option<PathBuf>,
    exists: bool,
    branch: Option<String>,
    last_commit: Option<String>,
    status_short: String,
    diff_stat: String,
    error: Option<String>,
}

impl WorktreeSnapshot {
    fn capture(path: Option<&Path>, task: &Task) -> Self {
        let Some(path) = path else {
            return Self {
                path: None,
                exists: false,
                branch: task.branch.clone(),
                last_commit: task.commit.clone(),
                status_short: "worktree path unavailable".to_string(),
                diff_stat: String::new(),
                error: Some("no worktree path could be inferred".to_string()),
            };
        };

        if !path.exists() {
            return Self {
                path: Some(path.to_path_buf()),
                exists: false,
                branch: task.branch.clone(),
                last_commit: task.commit.clone(),
                status_short: "worktree missing".to_string(),
                diff_stat: String::new(),
                error: Some(format!("{} does not exist", path.display())),
            };
        }

        let branch = git_stdout(path, &["branch", "--show-current"])
            .filter(|value| !value.is_empty())
            .or_else(|| task.branch.clone());
        let last_commit = git_stdout(path, &["rev-parse", "--short", "HEAD"])
            .filter(|value| !value.is_empty())
            .or_else(|| task.commit.clone());
        let status_short = git_stdout(path, &["status", "--short"])
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "clean".to_string());
        let mut diff_parts = Vec::new();
        if let Some(value) = git_stdout(path, &["diff", "--stat"]).filter(|value| !value.is_empty())
        {
            diff_parts.push(value);
        }
        if let Some(value) =
            git_stdout(path, &["diff", "--cached", "--stat"]).filter(|value| !value.is_empty())
        {
            diff_parts.push(format!("staged:\n{value}"));
        }
        let diff_stat = if diff_parts.is_empty() {
            "clean".to_string()
        } else {
            diff_parts.join("\n")
        };

        Self {
            path: Some(path.to_path_buf()),
            exists: true,
            branch,
            last_commit,
            status_short,
            diff_stat,
            error: None,
        }
    }
}

fn git_stdout(path: &Path, args: &[&str]) -> Option<String> {
    git_cmd::run_git(path, args)
        .ok()
        .map(|output| output.stdout.trim().to_string())
}

fn infer_worktree_path(project_root: &Path, task: &Task) -> Option<PathBuf> {
    if let Some(path) = task.worktree_path.as_deref().filter(|value| !value.trim().is_empty()) {
        let path = PathBuf::from(path);
        return Some(if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        });
    }

    let engineer = task
        .claimed_by
        .as_deref()
        .or(task.review_owner.as_deref())
        .or_else(|| task.branch.as_deref().and_then(|branch| branch.split('/').next()))
        .filter(|value| !value.trim().is_empty())?;

    Some(project_root.join(".batty").join("worktrees").join(engineer))
}

fn render_breadcrumb(
    task: &Task,
    source: &str,
    captured_at: &str,
    snapshot: &WorktreeSnapshot,
) -> String {
    let path = snapshot
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let branch = snapshot.branch.as_deref().unwrap_or("unknown");
    let last_commit = snapshot.last_commit.as_deref().unwrap_or("unknown");
    let error = snapshot.error.as_deref().unwrap_or("none");

    format!(
        concat!(
            "# Orphan Demotion Breadcrumbs\n\n",
            "- task: #{} {}\n",
            "- previous_status: {}\n",
            "- captured_at: {}\n",
            "- source: {}\n",
            "- worktree_path: {}\n",
            "- worktree_exists: {}\n",
            "- branch: {}\n",
            "- last_commit: {}\n",
            "- capture_error: {}\n\n",
            "## Dirty Status\n\n",
            "```text\n{}\n```\n\n",
            "## Diff Summary\n\n",
            "```text\n{}\n```\n"
        ),
        task.id,
        task.title,
        task.status,
        captured_at,
        source,
        path,
        snapshot.exists,
        branch,
        last_commit,
        error,
        snapshot.status_short,
        snapshot.diff_stat
    )
}

fn append_artifact_ref(task_path: &Path, artifact_ref: &str) -> Result<()> {
    crate::team::task_cmd::update_task_frontmatter(task_path, |mapping| {
        append_yaml_string(mapping, "artifacts", artifact_ref);
    })
}

fn append_yaml_string(mapping: &mut Mapping, key: &str, value: &str) {
    let key = crate::team::task_cmd::yaml_key(key);
    let entry = mapping
        .entry(key)
        .or_insert_with(|| Value::Sequence(Vec::new()));
    match entry {
        Value::Sequence(values) => {
            if !values
                .iter()
                .any(|existing| existing.as_str() == Some(value))
            {
                values.push(Value::String(value.to_string()));
            }
        }
        _ => {
            *entry = Value::Sequence(vec![Value::String(value.to_string())]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{git, git_ok};

    fn write_task(board_dir: &Path, id: u32, extra: &str) -> Task {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join(format!("{id:03}-orphan.md"));
        std::fs::write(
            &task_path,
            format!(
                "---\nid: {id}\ntitle: Orphan {id}\nstatus: in-progress\npriority: high\n{extra}---\n\nBody.\n"
            ),
        )
        .unwrap();
        Task::from_file(&task_path).unwrap()
    }

    fn init_repo(path: &Path) {
        git_ok(path, &["init", "-b", "main"]);
        git_ok(path, &["config", "user.email", "test@example.com"]);
        git_ok(path, &["config", "user.name", "Test User"]);
        std::fs::write(path.join("README.md"), "base\n").unwrap();
        git_ok(path, &["add", "README.md"]);
        git_ok(path, &["commit", "-m", "base"]);
    }

    #[test]
    fn captures_clean_worktree_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let board_dir = project_root.join(".batty").join("team_config").join("board");
        let worktree_dir = project_root.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        init_repo(&worktree_dir);
        let task = write_task(
            &board_dir,
            11,
            "branch: main\nworktree_path: .batty/worktrees/eng-1\n",
        );

        let breadcrumb = capture_orphan_demotion_breadcrumb(
            project_root,
            &board_dir,
            &task,
            "test.clean",
        )
        .unwrap();

        let content = std::fs::read_to_string(&breadcrumb.artifact_path).unwrap();
        assert!(content.contains("- worktree_exists: true"));
        assert!(content.contains("- branch: main"));
        assert!(content.contains("clean"));
        let updated = Task::from_file(&task.source_path).unwrap();
        assert_eq!(updated.artifacts, vec![breadcrumb.artifact_ref]);
    }

    #[test]
    fn captures_dirty_worktree_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let board_dir = project_root.join(".batty").join("team_config").join("board");
        let worktree_dir = project_root.join(".batty").join("worktrees").join("eng-2");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        init_repo(&worktree_dir);
        std::fs::write(worktree_dir.join("README.md"), "dirty\n").unwrap();
        std::fs::write(worktree_dir.join("new.txt"), "new\n").unwrap();
        let task = write_task(
            &board_dir,
            12,
            "branch: main\nworktree_path: .batty/worktrees/eng-2\n",
        );

        let breadcrumb = capture_orphan_demotion_breadcrumb(
            project_root,
            &board_dir,
            &task,
            "test.dirty",
        )
        .unwrap();

        let content = std::fs::read_to_string(&breadcrumb.artifact_path).unwrap();
        assert!(content.contains(" M README.md"));
        assert!(content.contains("?? new.txt"));
        assert!(content.contains("README.md"));
    }

    #[test]
    fn captures_missing_worktree_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let board_dir = project_root.join(".batty").join("team_config").join("board");
        let task = write_task(
            &board_dir,
            13,
            "branch: eng-3/13\nworktree_path: .batty/worktrees/eng-3\ncommit: abc1234\n",
        );

        let breadcrumb = capture_orphan_demotion_breadcrumb(
            project_root,
            &board_dir,
            &task,
            "test.missing",
        )
        .unwrap();

        let content = std::fs::read_to_string(&breadcrumb.artifact_path).unwrap();
        assert!(content.contains("- worktree_exists: false"));
        assert!(content.contains("- branch: eng-3/13"));
        assert!(content.contains("- last_commit: abc1234"));
        assert!(content.contains("worktree missing"));
    }
}
