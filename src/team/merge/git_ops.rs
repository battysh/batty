//! Low-level git helpers used by the merge subsystem.
//!
//! These wrappers run `git` as a subprocess and attach human-readable context
//! to errors so that callers (and operators reading logs) can tell which
//! high-level operation failed and why.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::team::task_loop::auto_commit_before_reset;

pub(crate) fn run_git_with_context(
    repo_dir: &Path,
    args: &[&str],
    intent: &str,
) -> Result<std::process::Output> {
    let command = format!("git {}", args.join(" "));
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .with_context(|| {
            format!(
                "failed while trying to {intent}: could not execute `{command}` in {}",
                repo_dir.display()
            )
        })
}

pub(crate) fn describe_git_failure(
    repo_dir: &Path,
    args: &[&str],
    intent: &str,
    stderr: &str,
) -> String {
    format!(
        "failed while trying to {intent}: `git {}` in {} returned: {}",
        args.join(" "),
        repo_dir.display(),
        stderr.trim()
    )
}

pub(crate) fn commits_ahead_of_main(worktree_dir: &Path) -> Result<u32> {
    let output = run_git_with_context(
        worktree_dir,
        &["rev-list", "--count", "main..HEAD"],
        "count commits ahead of main before accepting engineer completion",
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            describe_git_failure(
                worktree_dir,
                &["rev-list", "--count", "main..HEAD"],
                "count commits ahead of main before accepting engineer completion",
                &stderr,
            )
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u32>().with_context(|| {
        format!(
            "failed to parse git rev-list --count main..HEAD output: {:?}",
            stdout.trim()
        )
    })
}

/// Return the diff stat between main and HEAD using `git diff --stat main..HEAD`.
/// An empty string means the branch has no material file changes relative to main.
pub(crate) fn diff_stat_from_main(worktree_dir: &Path) -> Result<String> {
    let output = run_git_with_context(
        worktree_dir,
        &["diff", "--stat", "main..HEAD"],
        "check diff stat between main and HEAD for narration-only detection",
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            describe_git_failure(
                worktree_dir,
                &["diff", "--stat", "main..HEAD"],
                "check diff stat between main and HEAD for narration-only detection",
                &stderr,
            )
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Count files changed between main and HEAD using `git diff --stat main..HEAD`.
/// Returns 0 if the diff stat is empty (narration-only: commits exist but no file changes).
pub(crate) fn files_changed_from_main(worktree_dir: &Path) -> Result<u32> {
    let stdout = diff_stat_from_main(worktree_dir)?;
    let count = stdout
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && line
                    .chars()
                    .next()
                    .is_some_and(|first| !first.is_ascii_digit())
        })
        .count();
    Ok(count as u32)
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Preserve uncommitted changes via auto-commit, then force-clean the worktree
/// so `checkout -B` can succeed.
/// Best-effort: failures are logged but do not block the reset attempt.
pub(crate) fn force_clean_worktree(worktree_dir: &Path, engineer_name: &str) {
    // Try to auto-commit first to preserve work in git history.
    if !auto_commit_before_reset(worktree_dir) {
        info!(
            engineer = engineer_name,
            "auto-commit skipped or failed, proceeding with force-clean"
        );
    }

    if let Err(error) = run_git_with_context(
        worktree_dir,
        &["reset", "--hard"],
        "discard staged/unstaged changes before worktree reset",
    ) {
        warn!(
            engineer = engineer_name,
            error = %error,
            "git reset --hard failed during worktree cleanup"
        );
    }
    if let Err(error) = run_git_with_context(
        worktree_dir,
        &["clean", "-fd", "--exclude=.batty/"],
        "remove untracked files before worktree reset",
    ) {
        warn!(
            engineer = engineer_name,
            error = %error,
            "git clean failed during worktree cleanup"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_ahead_of_main_error_includes_command_and_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let error = commits_ahead_of_main(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("count commits ahead of main before accepting engineer completion"));
        assert!(error.contains("git rev-list --count main..HEAD"));
    }

    #[test]
    fn files_changed_from_main_error_on_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let error = files_changed_from_main(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("narration-only detection"));
    }

    #[test]
    fn files_changed_from_main_zero_when_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Set up a git repo with main branch
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // Create branch with empty commit (no file changes)
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "narration only"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert_eq!(diff_stat_from_main(repo).unwrap(), "");
        assert_eq!(files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn files_changed_from_main_counts_changed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // Create branch with actual file change
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("src.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.join("test.rs"), "fn test() {}").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "real changes"])
            .current_dir(repo)
            .output()
            .unwrap();
        let diff_stat = diff_stat_from_main(repo).unwrap();
        assert!(diff_stat.contains("src.rs"));
        assert!(diff_stat.contains("test.rs"));
        assert_eq!(files_changed_from_main(repo).unwrap(), 2);
    }
}
