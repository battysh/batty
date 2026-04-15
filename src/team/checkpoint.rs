//! Progress checkpoint files for agent restart context.
//!
//! Before restarting a stalled or context-exhausted agent, the daemon writes a
//! checkpoint file to `.batty/progress/<role>.md` containing the agent's current
//! task context. This file is included in the restart prompt so the agent can
//! resume with full awareness of what it was doing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Information captured in a progress checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub role: String,
    pub task_id: u32,
    pub task_title: String,
    pub task_description: String,
    pub branch: Option<String>,
    pub last_commit: Option<String>,
    pub test_summary: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartContext {
    pub role: String,
    pub task_id: u32,
    pub task_title: String,
    pub task_description: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub restart_count: u32,
    pub reason: String,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    #[serde(default)]
    pub last_commit: Option<String>,
    #[serde(default)]
    pub created_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pub handoff_consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservedLaneArtifactKind {
    Commit,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedLaneRecord {
    pub role: String,
    pub task_id: u32,
    pub task_title: String,
    pub task_status: String,
    pub source_branch: String,
    pub target_branch: String,
    pub reason: String,
    pub created_at: String,
    pub source_head: Option<String>,
    pub artifact_kind: PreservedLaneArtifactKind,
    pub preserved_commit: Option<String>,
    pub snapshot_path: Option<String>,
}

impl PreservedLaneRecord {
    pub fn commit(
        role: &str,
        task: &crate::task::Task,
        source_branch: &str,
        target_branch: &str,
        reason: &str,
        source_head: Option<String>,
        preserved_commit: String,
    ) -> Self {
        Self {
            role: role.to_string(),
            task_id: task.id,
            task_title: task.title.clone(),
            task_status: task.status.clone(),
            source_branch: source_branch.to_string(),
            target_branch: target_branch.to_string(),
            reason: reason.to_string(),
            created_at: chrono_timestamp(),
            source_head,
            artifact_kind: PreservedLaneArtifactKind::Commit,
            preserved_commit: Some(preserved_commit),
            snapshot_path: None,
        }
    }

    pub fn snapshot(
        role: &str,
        task: &crate::task::Task,
        source_branch: &str,
        target_branch: &str,
        reason: &str,
        source_head: Option<String>,
        snapshot_path: String,
    ) -> Self {
        Self {
            role: role.to_string(),
            task_id: task.id,
            task_title: task.title.clone(),
            task_status: task.status.clone(),
            source_branch: source_branch.to_string(),
            target_branch: target_branch.to_string(),
            reason: reason.to_string(),
            created_at: chrono_timestamp(),
            source_head,
            artifact_kind: PreservedLaneArtifactKind::Snapshot,
            preserved_commit: None,
            snapshot_path: Some(snapshot_path),
        }
    }

    pub fn status_signal(&self) -> String {
        match self.artifact_kind {
            PreservedLaneArtifactKind::Commit => format!(
                "saved completed lane #{} before cleanup (commit {} @ {})",
                self.task_id,
                self.source_branch,
                abbreviate_commit(self.preserved_commit.as_deref())
            ),
            PreservedLaneArtifactKind::Snapshot => format!(
                "saved completed lane #{} before cleanup (snapshot {})",
                self.task_id,
                self.snapshot_path.as_deref().unwrap_or("-")
            ),
        }
    }

    pub fn doctor_check_line(&self) -> String {
        match self.artifact_kind {
            PreservedLaneArtifactKind::Commit => format!(
                "{} preserved completed task #{} before cleanup on {} ({})",
                self.role,
                self.task_id,
                self.source_branch,
                abbreviate_commit(self.preserved_commit.as_deref())
            ),
            PreservedLaneArtifactKind::Snapshot => format!(
                "{} preserved completed task #{} before cleanup in {}",
                self.role,
                self.task_id,
                self.snapshot_path.as_deref().unwrap_or("-")
            ),
        }
    }
}

/// Returns the progress directory path: `<project_root>/.batty/progress/`.
pub fn progress_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("progress")
}

/// Returns the checkpoint file path for a given role.
pub fn checkpoint_path(project_root: &Path, role: &str) -> PathBuf {
    progress_dir(project_root).join(format!("{role}.md"))
}

pub fn preserved_lane_dir(project_root: &Path) -> PathBuf {
    progress_dir(project_root).join("preserved-lanes")
}

pub fn preserved_lane_record_path(project_root: &Path, role: &str) -> PathBuf {
    preserved_lane_dir(project_root).join(format!("{role}.json"))
}

pub fn restart_context_path(worktree_dir: &Path) -> PathBuf {
    worktree_dir.join("restart_context.json")
}

/// Write a progress checkpoint file for the given role.
///
/// Creates the `.batty/progress/` directory if it doesn't exist.
pub fn write_checkpoint(project_root: &Path, checkpoint: &Checkpoint) -> Result<()> {
    let dir = progress_dir(project_root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", checkpoint.role));
    let content = format_checkpoint(checkpoint);
    std::fs::write(&path, content)?;
    Ok(())
}

/// Read a checkpoint file for the given role, if it exists.
pub fn read_checkpoint(project_root: &Path, role: &str) -> Option<String> {
    let path = checkpoint_path(project_root, role);
    std::fs::read_to_string(&path).ok()
}

/// Remove the checkpoint file for the given role. No-op if it doesn't exist.
pub fn remove_checkpoint(project_root: &Path, role: &str) {
    let path = checkpoint_path(project_root, role);
    let _ = std::fs::remove_file(&path);
}

pub fn write_preserved_lane_record(
    project_root: &Path,
    record: &PreservedLaneRecord,
) -> Result<PathBuf> {
    let dir = preserved_lane_dir(project_root);
    std::fs::create_dir_all(&dir)?;
    let path = preserved_lane_record_path(project_root, &record.role);
    let content = serde_json::to_vec_pretty(record)?;
    std::fs::write(&path, content)?;
    Ok(path)
}

pub fn read_preserved_lane_record(project_root: &Path, role: &str) -> Option<PreservedLaneRecord> {
    let path = preserved_lane_record_path(project_root, role);
    let content = std::fs::read(path).ok()?;
    serde_json::from_slice(&content).ok()
}

pub fn list_preserved_lane_records(project_root: &Path) -> Vec<PreservedLaneRecord> {
    let dir = preserved_lane_dir(project_root);
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return records;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice(&content) else {
            continue;
        };
        records.push(record);
    }

    records.sort_by(|left, right| {
        left.role
            .cmp(&right.role)
            .then(left.task_id.cmp(&right.task_id))
    });
    records
}

pub fn remove_preserved_lane_record(project_root: &Path, role: &str) {
    let path = preserved_lane_record_path(project_root, role);
    let _ = std::fs::remove_file(path);
}

#[allow(clippy::too_many_arguments)]
pub fn write_dirty_lane_snapshot(
    project_root: &Path,
    worktree_dir: &Path,
    role: &str,
    task: &crate::task::Task,
    source_branch: &str,
    target_branch: &str,
    reason: &str,
    source_head: Option<&str>,
) -> Result<PathBuf> {
    let dir = preserved_lane_dir(project_root);
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("{role}-task-{}-{stamp}.md", task.id));

    let status = git_stdout(
        worktree_dir,
        &["status", "--short", "--untracked-files=all"],
    )
    .context("failed to capture worktree status for preserved lane snapshot")?;
    let staged_diff = git_stdout(
        worktree_dir,
        &["diff", "--cached", "--binary", "--no-ext-diff"],
    )
    .context("failed to capture staged diff for preserved lane snapshot")?;
    let unstaged_diff = git_stdout(worktree_dir, &["diff", "--binary", "--no-ext-diff"])
        .context("failed to capture unstaged diff for preserved lane snapshot")?;
    let untracked_paths = git_stdout(
        worktree_dir,
        &["ls-files", "--others", "--exclude-standard"],
    )
    .context("failed to capture untracked files for preserved lane snapshot")?;

    let mut content = String::new();
    content.push_str(&format!("# Preserved Dirty Lane Snapshot: {role}\n\n"));
    content.push_str(&format!("Task: #{} - {}\n", task.id, task.title));
    content.push_str(&format!("Task Status: {}\n", task.status));
    content.push_str(&format!("Source Branch: {source_branch}\n"));
    content.push_str(&format!("Target Branch: {target_branch}\n"));
    content.push_str(&format!("Reason: {reason}\n"));
    content.push_str(&format!("Created At: {}\n", chrono_timestamp()));
    if let Some(head) = source_head {
        content.push_str(&format!("Source HEAD: {head}\n"));
    }
    content.push_str("\n## Git Status\n\n```text\n");
    content.push_str(status.trim_end());
    content.push_str("\n```\n");

    if !staged_diff.trim().is_empty() {
        content.push_str("\n## Staged Diff\n\n```diff\n");
        content.push_str(staged_diff.trim_end());
        content.push_str("\n```\n");
    }

    if !unstaged_diff.trim().is_empty() {
        content.push_str("\n## Unstaged Diff\n\n```diff\n");
        content.push_str(unstaged_diff.trim_end());
        content.push_str("\n```\n");
    }

    let tracked_untracked = untracked_paths
        .lines()
        .map(str::trim)
        .filter(|path| {
            !path.is_empty() && !path.starts_with(".batty/") && !path.starts_with(".cargo/")
        })
        .collect::<Vec<_>>();
    if !tracked_untracked.is_empty() {
        content.push_str("\n## Untracked Files\n");
        for relative in tracked_untracked {
            let file_path = worktree_dir.join(relative);
            let file_body = std::fs::read(&file_path).with_context(|| {
                format!("failed to read untracked file {}", file_path.display())
            })?;
            content.push_str(&format!("\n### {relative}\n\n```text\n"));
            content.push_str(&String::from_utf8_lossy(&file_body));
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("```\n");
        }
    }

    std::fs::write(&path, content)
        .with_context(|| format!("failed to write preserved lane snapshot {}", path.display()))?;
    Ok(path)
}

pub fn write_restart_context(worktree_dir: &Path, context: &RestartContext) -> Result<()> {
    std::fs::create_dir_all(worktree_dir)?;
    let path = restart_context_path(worktree_dir);
    let content = serde_json::to_vec_pretty(context)?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn read_restart_context(worktree_dir: &Path) -> Option<RestartContext> {
    let path = restart_context_path(worktree_dir);
    let content = std::fs::read(path).ok()?;
    serde_json::from_slice(&content).ok()
}

pub fn remove_restart_context(worktree_dir: &Path) {
    let path = restart_context_path(worktree_dir);
    let _ = std::fs::remove_file(path);
}

/// Gather checkpoint information from the worktree and task.
pub fn gather_checkpoint(project_root: &Path, role: &str, task: &crate::task::Task) -> Checkpoint {
    let worktree_dir = project_root.join(".batty").join("worktrees").join(role);

    let branch = task
        .branch
        .clone()
        .or_else(|| git_current_branch(&worktree_dir));

    let last_commit = git_last_commit(&worktree_dir);
    let test_summary = last_test_output(&worktree_dir);

    let timestamp = chrono_timestamp();

    Checkpoint {
        role: role.to_string(),
        task_id: task.id,
        task_title: task.title.clone(),
        task_description: task.description.clone(),
        branch,
        last_commit,
        test_summary,
        timestamp,
    }
}

/// Format a checkpoint as Markdown content.
fn format_checkpoint(cp: &Checkpoint) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Progress Checkpoint: {}\n\n", cp.role));
    out.push_str(&format!(
        "**Task:** #{} — {}\n\n",
        cp.task_id, cp.task_title
    ));
    out.push_str(&format!("**Timestamp:** {}\n\n", cp.timestamp));

    if let Some(branch) = &cp.branch {
        out.push_str(&format!("**Branch:** {branch}\n\n"));
    }

    if let Some(commit) = &cp.last_commit {
        out.push_str(&format!("**Last commit:** {commit}\n\n"));
    }

    out.push_str("## Task Description\n\n");
    out.push_str(&cp.task_description);
    out.push('\n');

    if let Some(tests) = &cp.test_summary {
        out.push_str("\n## Last Test Output\n\n");
        out.push_str(tests);
        out.push('\n');
    }

    out
}

/// Get the current branch name in a worktree directory.
fn git_current_branch(worktree_dir: &Path) -> Option<String> {
    if !worktree_dir.exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree_dir)
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() || branch == "HEAD" {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

fn git_stdout(worktree_dir: &Path, args: &[&str]) -> Result<String> {
    Ok(crate::team::git_cmd::run_git(worktree_dir, args)?.stdout)
}

fn abbreviate_commit(commit: Option<&str>) -> String {
    commit
        .map(|commit| commit.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Get the last commit hash and message in a worktree directory.
fn git_last_commit(worktree_dir: &Path) -> Option<String> {
    if !worktree_dir.exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["log", "-1", "--oneline"])
        .current_dir(worktree_dir)
        .output()
        .ok()?;
    if output.status.success() {
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if line.is_empty() { None } else { Some(line) }
    } else {
        None
    }
}

/// Try to read the last cargo test output from common locations.
/// Returns None if no recent test output is found.
fn last_test_output(worktree_dir: &Path) -> Option<String> {
    // Check for a batty-managed test output file
    let test_output_path = worktree_dir.join(".batty_test_output");
    if test_output_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&test_output_path) {
            if !content.is_empty() {
                // Truncate to last 50 lines to keep checkpoint manageable
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(50);
                return Some(lines[start..].join("\n"));
            }
        }
    }
    None
}

/// Generate an ISO-8601 timestamp string.
fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // Format as a simple UTC timestamp
    let secs = now.as_secs();
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    let seconds = secs % 60;
    let days_since_epoch = secs / 86400;
    // Simple date calculation from epoch days
    let (year, month, day) = epoch_days_to_date(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_task(id: u32, title: &str, description: &str) -> crate::task::Task {
        crate::task::Task {
            id,
            title: title.to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: None,
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: vec![],
            depends_on: vec![],
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: Some("eng-1-2/42".to_string()),
            commit: None,
            artifacts: vec![],
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: description.to_string(),
            batty_config: None,
            source_path: PathBuf::from("/tmp/fake.md"),
        }
    }

    #[test]
    fn write_and_read_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cp = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 42,
            task_title: "Fix the widget".to_string(),
            task_description: "Widget is broken, needs fixing.".to_string(),
            branch: Some("eng-1-1/42".to_string()),
            last_commit: Some("abc1234 fix widget rendering".to_string()),
            test_summary: Some("test result: ok. 10 passed".to_string()),
            timestamp: "2026-03-22T10:00:00Z".to_string(),
        };

        write_checkpoint(root, &cp).unwrap();

        let content = read_checkpoint(root, "eng-1-1").unwrap();
        assert!(content.contains("# Progress Checkpoint: eng-1-1"));
        assert!(content.contains("**Task:** #42 — Fix the widget"));
        assert!(content.contains("**Branch:** eng-1-1/42"));
        assert!(content.contains("**Last commit:** abc1234 fix widget rendering"));
        assert!(content.contains("Widget is broken, needs fixing."));
        assert!(content.contains("test result: ok. 10 passed"));
        assert!(content.contains("**Timestamp:** 2026-03-22T10:00:00Z"));
    }

    #[test]
    fn read_checkpoint_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_checkpoint(tmp.path(), "eng-nonexistent").is_none());
    }

    #[test]
    fn remove_checkpoint_deletes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cp = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 1,
            task_title: "t".to_string(),
            task_description: "d".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        write_checkpoint(root, &cp).unwrap();
        assert!(checkpoint_path(root, "eng-1-1").exists());

        remove_checkpoint(root, "eng-1-1");
        assert!(!checkpoint_path(root, "eng-1-1").exists());
    }

    #[test]
    fn remove_checkpoint_noop_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Should not panic
        remove_checkpoint(tmp.path(), "eng-nonexistent");
    }

    #[test]
    fn checkpoint_creates_progress_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = progress_dir(root);
        assert!(!dir.exists());

        let cp = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 1,
            task_title: "t".to_string(),
            task_description: "d".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        write_checkpoint(root, &cp).unwrap();
        assert!(dir.exists());
    }

    #[test]
    fn preserved_lane_record_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let task = crate::task::Task {
            status: "done".to_string(),
            claimed_by: Some("eng-1".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1".to_string()),
            ..make_task(628, "done lane", "done lane description")
        };
        let record = PreservedLaneRecord::commit(
            "eng-1",
            &task,
            "eng-1/628",
            "eng-main/eng-1",
            "completed task no longer needs engineer lane",
            Some("abc123456789".to_string()),
            "def4567890abc".to_string(),
        );

        let path = write_preserved_lane_record(tmp.path(), &record).unwrap();
        assert!(path.exists());

        let loaded = read_preserved_lane_record(tmp.path(), "eng-1").unwrap();
        assert_eq!(loaded, record);
        assert_eq!(
            loaded.status_signal(),
            "saved completed lane #628 before cleanup (commit eng-1/628 @ def4567890ab)"
        );
    }

    #[test]
    fn write_dirty_lane_snapshot_captures_diffs_and_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "checkpoint-snapshot");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            "eng-1/628",
            &team_config_dir,
        )
        .unwrap();
        fs::write(worktree_dir.join("tracked.txt"), "tracked\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "tracked.txt"]);
        fs::write(worktree_dir.join("unstaged.txt"), "unstaged\n").unwrap();

        let task = crate::task::Task {
            status: "done".to_string(),
            claimed_by: Some("eng-1".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1".to_string()),
            ..make_task(628, "done lane", "done lane description")
        };
        let snapshot_path = write_dirty_lane_snapshot(
            &repo,
            &worktree_dir,
            "eng-1",
            &task,
            "eng-1/628",
            "eng-main/eng-1",
            "completed task no longer needs engineer lane",
            Some("abc123456789"),
        )
        .unwrap();

        let content = fs::read_to_string(&snapshot_path).unwrap();
        assert!(content.contains("Source Branch: eng-1/628"));
        assert!(content.contains("tracked.txt"));
        assert!(content.contains("unstaged.txt"));
        assert!(content.contains("completed task no longer needs engineer lane"));
    }

    #[test]
    fn format_checkpoint_without_optional_fields() {
        let cp = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 99,
            task_title: "Minimal task".to_string(),
            task_description: "Do the thing.".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-03-22T12:00:00Z".to_string(),
        };
        let content = format_checkpoint(&cp);
        assert!(content.contains("# Progress Checkpoint: eng-1-1"));
        assert!(content.contains("**Task:** #99 — Minimal task"));
        assert!(!content.contains("**Branch:**"));
        assert!(!content.contains("**Last commit:**"));
        assert!(!content.contains("## Last Test Output"));
    }

    #[test]
    fn gather_checkpoint_uses_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let task = make_task(42, "Test task", "Test description");
        let cp = gather_checkpoint(tmp.path(), "eng-1-2", &task);
        assert_eq!(cp.task_id, 42);
        assert_eq!(cp.task_title, "Test task");
        assert_eq!(cp.task_description, "Test description");
        assert_eq!(cp.branch, Some("eng-1-2/42".to_string()));
        assert_eq!(cp.role, "eng-1-2");
        assert!(!cp.timestamp.is_empty());
    }

    #[test]
    fn last_test_output_reads_batty_test_file() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path();
        let test_file = worktree.join(".batty_test_output");
        fs::write(&test_file, "test result: ok. 5 passed; 0 failed").unwrap();

        let summary = last_test_output(worktree);
        assert!(summary.is_some());
        assert!(summary.unwrap().contains("5 passed"));
    }

    #[test]
    fn last_test_output_returns_none_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(last_test_output(tmp.path()).is_none());
    }

    #[test]
    fn last_test_output_truncates_long_output() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join(".batty_test_output");
        // Write 100 lines — should truncate to last 50
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        fs::write(&test_file, lines.join("\n")).unwrap();

        let summary = last_test_output(tmp.path()).unwrap();
        let result_lines: Vec<&str> = summary.lines().collect();
        assert_eq!(result_lines.len(), 50);
        assert!(result_lines[0].contains("line 50"));
        assert!(result_lines[49].contains("line 99"));
    }

    #[test]
    fn epoch_days_to_date_known_values() {
        // 2026-03-22 is day 20534 from epoch (1970-01-01)
        let (y, m, d) = epoch_days_to_date(0);
        assert_eq!((y, m, d), (1970, 1, 1));

        // 2000-01-01 = day 10957
        let (y, m, d) = epoch_days_to_date(10957);
        assert_eq!((y, m, d), (2000, 1, 1));
    }

    #[test]
    fn checkpoint_path_correct() {
        let root = Path::new("/project");
        assert_eq!(
            checkpoint_path(root, "eng-1-1"),
            PathBuf::from("/project/.batty/progress/eng-1-1.md")
        );
    }

    #[test]
    fn overwrite_existing_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let cp1 = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 1,
            task_title: "First".to_string(),
            task_description: "First task".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        write_checkpoint(root, &cp1).unwrap();

        let cp2 = Checkpoint {
            role: "eng-1-1".to_string(),
            task_id: 2,
            task_title: "Second".to_string(),
            task_description: "Second task".to_string(),
            branch: Some("eng-1-1/2".to_string()),
            last_commit: None,
            test_summary: None,
            timestamp: "2026-01-02T00:00:00Z".to_string(),
        };
        write_checkpoint(root, &cp2).unwrap();

        let content = read_checkpoint(root, "eng-1-1").unwrap();
        assert!(content.contains("**Task:** #2 — Second"));
        assert!(!content.contains("First"));
    }

    #[test]
    fn write_and_read_restart_context_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_dir = tmp.path().join("eng-1-1");
        let context = RestartContext {
            role: "eng-1-1".to_string(),
            task_id: 42,
            task_title: "Fix the widget".to_string(),
            task_description: "Widget is broken, needs fixing.".to_string(),
            branch: Some("eng-1-1/42".to_string()),
            worktree_path: Some("/tmp/worktrees/eng-1-1".to_string()),
            restart_count: 2,
            reason: "context_exhausted".to_string(),
            output_bytes: Some(512_000),
            last_commit: Some("abc1234 fix widget".to_string()),
            created_at_epoch_secs: Some(1_234_567_890),
            handoff_consumed: false,
        };

        write_restart_context(&worktree_dir, &context).unwrap();

        let loaded = read_restart_context(&worktree_dir).unwrap();
        assert_eq!(loaded, context);
    }

    #[test]
    fn remove_restart_context_deletes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_dir = tmp.path().join("eng-1-1");
        let context = RestartContext {
            role: "eng-1-1".to_string(),
            task_id: 42,
            task_title: "Fix the widget".to_string(),
            task_description: "Widget is broken, needs fixing.".to_string(),
            branch: None,
            worktree_path: None,
            restart_count: 1,
            reason: "stalled".to_string(),
            output_bytes: None,
            last_commit: None,
            created_at_epoch_secs: None,
            handoff_consumed: false,
        };

        write_restart_context(&worktree_dir, &context).unwrap();
        assert!(restart_context_path(&worktree_dir).exists());

        remove_restart_context(&worktree_dir);
        assert!(!restart_context_path(&worktree_dir).exists());
    }

    #[test]
    fn read_restart_context_returns_none_when_missing_or_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_dir = tmp.path().join("eng-1-1");
        assert!(read_restart_context(&worktree_dir).is_none());

        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(restart_context_path(&worktree_dir), b"{not json").unwrap();
        assert!(read_restart_context(&worktree_dir).is_none());
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn write_checkpoint_to_readonly_dir_fails() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let readonly = tmp.path().join("readonly_root");
            fs::create_dir(&readonly).unwrap();
            // Create .batty dir but make it readonly
            let batty_dir = readonly.join(".batty");
            fs::create_dir(&batty_dir).unwrap();
            fs::set_permissions(&batty_dir, fs::Permissions::from_mode(0o444)).unwrap();

            let cp = Checkpoint {
                role: "eng-1-1".to_string(),
                task_id: 1,
                task_title: "t".to_string(),
                task_description: "d".to_string(),
                branch: None,
                last_commit: None,
                test_summary: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            };
            let result = write_checkpoint(&readonly, &cp);
            assert!(result.is_err());

            // Restore permissions for cleanup
            fs::set_permissions(&batty_dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn git_current_branch_returns_none_for_nonexistent_dir() {
        let result = git_current_branch(Path::new("/tmp/__batty_no_dir_here__"));
        assert!(result.is_none());
    }

    #[test]
    fn git_current_branch_returns_none_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = git_current_branch(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn git_last_commit_returns_none_for_nonexistent_dir() {
        let result = git_last_commit(Path::new("/tmp/__batty_no_dir_here__"));
        assert!(result.is_none());
    }

    #[test]
    fn git_last_commit_returns_none_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = git_last_commit(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn last_test_output_returns_none_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join(".batty_test_output");
        fs::write(&test_file, "").unwrap();
        assert!(last_test_output(tmp.path()).is_none());
    }

    #[test]
    fn chrono_timestamp_returns_valid_format() {
        let ts = chrono_timestamp();
        // Should match ISO-8601 pattern: YYYY-MM-DDTHH:MM:SSZ
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20); // "2026-03-22T10:00:00Z" is 20 chars
    }
}
