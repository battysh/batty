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
}
