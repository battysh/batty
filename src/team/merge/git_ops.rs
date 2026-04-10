//! Low-level git helpers used by the merge subsystem.
//!
//! These wrappers run `git` as a subprocess and attach human-readable context
//! to errors so that callers (and operators reading logs) can tell which
//! high-level operation failed and why.

use std::path::{Path, PathBuf};
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

pub(crate) fn changed_paths_from_main(worktree_dir: &Path) -> Result<Vec<PathBuf>> {
    let output = run_git_with_context(
        worktree_dir,
        &["diff", "--name-only", "main..HEAD"],
        "list changed paths between main and HEAD for narration-only detection",
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            describe_git_failure(
                worktree_dir,
                &["diff", "--name-only", "main..HEAD"],
                "list changed paths between main and HEAD for narration-only detection",
                &stderr,
            )
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn is_completion_noise_path(path: &Path) -> bool {
    let first_component = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();

    if matches!(
        first_component,
        ".batty" | ".batty-target" | ".agents" | ".claude" | "target"
    ) {
        return true;
    }

    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("AGENTS.md" | "CLAUDE.md")
    )
}

fn evidence_paths_from_main(worktree_dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(changed_paths_from_main(worktree_dir)?
        .into_iter()
        .filter(|path| !is_completion_noise_path(path))
        .collect())
}

pub(crate) fn is_non_code_path(path: &Path) -> bool {
    let root_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        root_name,
        "README" | "README.md" | "CHANGELOG.md" | "LICENSE" | "LICENSE.md"
    ) {
        return true;
    }

    let first_component = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();
    if matches!(first_component, "docs" | "planning" | "assets") {
        return true;
    }

    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "md" | "markdown"
                | "rst"
                | "adoc"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "svg"
                | "webp"
                | "bmp"
                | "ico"
                | "pdf"
        )
    )
}

/// Count files changed between main and HEAD.
pub(crate) fn files_changed_from_main(worktree_dir: &Path) -> Result<u32> {
    Ok(evidence_paths_from_main(worktree_dir)?.len() as u32)
}

/// Count code-relevant files changed between main and HEAD.
pub(crate) fn code_files_changed_from_main(worktree_dir: &Path) -> Result<u32> {
    Ok(evidence_paths_from_main(worktree_dir)?
        .into_iter()
        .filter(|path| !is_non_code_path(path))
        .count() as u32)
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

    #[test]
    fn code_files_changed_from_main_ignores_docs_only_diff() {
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
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("notes.md"), "narration only").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "docs only"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn files_changed_from_main_ignores_runtime_noise_paths() {
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

        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join(".batty-target").join("debug")).unwrap();
        std::fs::write(repo.join(".batty-target").join("CACHEDIR.TAG"), "cache\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::write(repo.join(".batty").join("state.json"), "{}\n").unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("notes.md"), "docs only\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A", "-f", ".batty-target", ".batty", "docs"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "runtime noise"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn code_files_changed_from_main_counts_source_changes_but_skips_runtime_noise() {
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

        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty-target").join("debug")).unwrap();
        std::fs::write(repo.join(".batty-target").join("CACHEDIR.TAG"), "cache\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A", "-f", "src", ".batty-target"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "source plus noise"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 1);
    }

    #[test]
    fn is_non_code_path_preserves_code_adjacent_files() {
        assert!(!is_non_code_path(Path::new("Cargo.toml")));
        assert!(!is_non_code_path(Path::new("Cargo.lock")));
        assert!(!is_non_code_path(Path::new("src/main.rs")));
        assert!(is_non_code_path(Path::new("docs/guide.md")));
        assert!(is_non_code_path(Path::new("assets/logo.svg")));
    }

    #[test]
    fn completion_noise_paths_are_excluded_from_evidence_counts() {
        assert!(is_completion_noise_path(Path::new(
            ".batty-target/debug/output"
        )));
        assert!(is_completion_noise_path(Path::new(".batty/state.json")));
        assert!(is_completion_noise_path(Path::new("target/debug/batty")));
        assert!(is_completion_noise_path(Path::new("CLAUDE.md")));
        assert!(!is_completion_noise_path(Path::new("docs/guide.md")));
        assert!(!is_completion_noise_path(Path::new("src/main.rs")));
    }
}
