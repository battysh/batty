#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub use super::errors::GitError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

fn classify_error(stderr: &str) -> GitError {
    let message = stderr.trim().to_string();
    let lowered = stderr.to_ascii_lowercase();

    if lowered.contains("lock")
        || lowered.contains("index.lock")
        || lowered.contains("unable to create")
        || lowered.contains("connection refused")
        || lowered.contains("timeout")
        || lowered.contains("could not read")
        || lowered.contains("resource temporarily unavailable")
    {
        GitError::Transient {
            message,
            stderr: stderr.to_string(),
        }
    } else {
        GitError::Permanent {
            message,
            stderr: stderr.to_string(),
        }
    }
}

fn format_git_command(repo_dir: &Path, args: &[&str]) -> String {
    let mut parts = vec![
        "git".to_string(),
        "-C".to_string(),
        repo_dir.display().to_string(),
    ];
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.join(" ")
}

fn run_git_with_status(repo_dir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|source| GitError::Exec {
            command: format_git_command(repo_dir, args),
            source,
        })
}

/// Check whether `path` is inside a git work tree.
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn run_git(repo_dir: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
    let output = run_git_with_status(repo_dir, args)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(GitOutput { stdout, stderr })
    } else {
        Err(classify_error(&stderr))
    }
}

pub fn worktree_add(
    repo: &Path,
    path: &Path,
    branch: &str,
    start: &str,
) -> Result<GitOutput, GitError> {
    let path = path.to_string_lossy();
    run_git(
        repo,
        &["worktree", "add", "-b", branch, path.as_ref(), start],
    )
}

pub fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<(), GitError> {
    let path = path.to_string_lossy();
    if force {
        run_git(repo, &["worktree", "remove", "--force", path.as_ref()])?;
    } else {
        run_git(repo, &["worktree", "remove", path.as_ref()])?;
    }
    Ok(())
}

pub fn worktree_list(repo: &Path) -> Result<String, GitError> {
    Ok(run_git(repo, &["worktree", "list", "--porcelain"])?.stdout)
}

pub fn rebase(repo: &Path, onto: &str) -> Result<(), GitError> {
    run_git(repo, &["rebase", onto])
        .map(|_| ())
        .map_err(|error| match error {
            GitError::Transient { .. } | GitError::Exec { .. } => error,
            GitError::Permanent { stderr, .. } => GitError::RebaseFailed {
                branch: onto.to_string(),
                stderr,
            },
            GitError::RebaseFailed { .. }
            | GitError::MergeFailed { .. }
            | GitError::RevParseFailed { .. }
            | GitError::InvalidRevListCount { .. } => error,
        })
}

pub fn rebase_abort(repo: &Path) -> Result<(), GitError> {
    run_git(repo, &["rebase", "--abort"])?;
    Ok(())
}

pub fn merge(repo: &Path, branch: &str) -> Result<(), GitError> {
    run_git(repo, &["merge", branch, "--no-edit"])
        .map(|_| ())
        .map_err(|error| match error {
            GitError::Transient { .. } | GitError::Exec { .. } => error,
            GitError::Permanent { stderr, .. } => GitError::MergeFailed {
                branch: branch.to_string(),
                stderr,
            },
            GitError::RebaseFailed { .. }
            | GitError::MergeFailed { .. }
            | GitError::RevParseFailed { .. }
            | GitError::InvalidRevListCount { .. } => error,
        })
}

pub fn merge_base_is_ancestor(repo: &Path, commit: &str, base: &str) -> Result<bool, GitError> {
    let output = run_git_with_status(repo, &["merge-base", "--is-ancestor", commit, base])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(classify_error(&String::from_utf8_lossy(&output.stderr))),
    }
}

pub fn rev_parse_branch(repo: &Path) -> Result<String, GitError> {
    run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|output| output.stdout.trim().to_string())
        .map_err(|error| match error {
            GitError::Transient { .. } | GitError::Exec { .. } => error,
            GitError::Permanent { stderr, .. } => GitError::RevParseFailed {
                spec: "--abbrev-ref HEAD".to_string(),
                stderr,
            },
            GitError::RebaseFailed { .. }
            | GitError::MergeFailed { .. }
            | GitError::RevParseFailed { .. }
            | GitError::InvalidRevListCount { .. } => error,
        })
}

pub fn rev_parse_toplevel(repo: &Path) -> Result<PathBuf, GitError> {
    Ok(PathBuf::from(
        run_git(repo, &["rev-parse", "--show-toplevel"])?
            .stdout
            .trim(),
    ))
}

pub fn status_porcelain(repo: &Path) -> Result<String, GitError> {
    Ok(run_git(repo, &["status", "--porcelain"])?.stdout)
}

pub fn checkout_new_branch(repo: &Path, branch: &str, start: &str) -> Result<(), GitError> {
    run_git(repo, &["checkout", "-B", branch, start])?;
    Ok(())
}

pub fn show_ref_exists(repo: &Path, branch: &str) -> Result<bool, GitError> {
    let ref_name = format!("refs/heads/{branch}");
    let output = run_git_with_status(repo, &["show-ref", "--verify", "--quiet", &ref_name])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(classify_error(&String::from_utf8_lossy(&output.stderr))),
    }
}

pub fn branch_delete(repo: &Path, branch: &str) -> Result<(), GitError> {
    run_git(repo, &["branch", "-D", branch])?;
    Ok(())
}

pub fn branch_rename(repo: &Path, old: &str, new: &str) -> Result<(), GitError> {
    run_git(repo, &["branch", "-m", old, new])?;
    Ok(())
}

pub fn rev_list_count(repo: &Path, range: &str) -> Result<u32, GitError> {
    let output = run_git(repo, &["rev-list", "--count", range])?;
    let count = output
        .stdout
        .trim()
        .parse()
        .map_err(|_| GitError::InvalidRevListCount {
            range: range.to_string(),
            output: output.stdout.trim().to_string(),
        })?;
    Ok(count)
}

pub fn for_each_ref_branches(repo: &Path) -> Result<Vec<String>, GitError> {
    Ok(run_git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?
    .stdout
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(ToOwned::to_owned)
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git_ok(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args));
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_ok(repo, &["init", "-b", "main"]);
        git_ok(repo, &["config", "user.email", "batty-test@example.com"]);
        git_ok(repo, &["config", "user.name", "Batty Test"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        git_ok(repo, &["add", "README.md"]);
        git_ok(repo, &["commit", "-m", "initial"]);
        tmp
    }

    #[test]
    fn classify_error_marks_transient_stderr() {
        let error = classify_error("Unable to create '/tmp/repo/.git/index.lock': File exists");
        assert!(matches!(error, GitError::Transient { .. }));
        assert!(error.is_transient());
    }

    #[test]
    fn classify_error_marks_permanent_stderr() {
        let error = classify_error("fatal: not a git repository");
        assert!(matches!(error, GitError::Permanent { .. }));
        assert!(!error.is_transient());
    }

    #[test]
    fn run_git_succeeds_for_valid_command() {
        let tmp = init_repo();
        let output = run_git(tmp.path(), &["rev-parse", "--show-toplevel"]).unwrap();
        let actual = PathBuf::from(output.stdout.trim()).canonicalize().unwrap();
        let expected = tmp.path().canonicalize().unwrap();
        assert_eq!(actual, expected);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn run_git_invalid_args_return_permanent_error() {
        let tmp = init_repo();
        let error = run_git(tmp.path(), &["not-a-real-subcommand"]).unwrap_err();
        assert!(matches!(error, GitError::Permanent { .. }));
        assert!(!error.is_transient());
    }

    #[test]
    fn is_transient_matches_variants() {
        let transient = GitError::Transient {
            message: "temporary lock".to_string(),
            stderr: "index.lock".to_string(),
        };
        let permanent = GitError::Permanent {
            message: "bad ref".to_string(),
            stderr: "fatal: bad revision".to_string(),
        };
        let exec = GitError::Exec {
            command: "git status --porcelain".to_string(),
            source: std::io::Error::other("missing git"),
        };

        assert!(transient.is_transient());
        assert!(!permanent.is_transient());
        assert!(!exec.is_transient());
        assert!(exec.to_string().contains("git status --porcelain"));
    }

    #[test]
    fn non_git_dir_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(tmp.path()));
    }

    #[test]
    fn git_initialized_dir_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(is_git_repo(tmp.path()));
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn classify_error_connection_refused_is_transient() {
        let error = classify_error("fatal: unable to access: Connection refused");
        assert!(matches!(error, GitError::Transient { .. }));
    }

    #[test]
    fn classify_error_timeout_is_transient() {
        let error = classify_error("fatal: unable to access: Timeout was reached");
        assert!(matches!(error, GitError::Transient { .. }));
    }

    #[test]
    fn classify_error_resource_unavailable_is_transient() {
        let error = classify_error("error: resource temporarily unavailable");
        assert!(matches!(error, GitError::Transient { .. }));
    }

    #[test]
    fn classify_error_could_not_read_is_transient() {
        let error = classify_error("fatal: could not read from remote repository");
        assert!(matches!(error, GitError::Transient { .. }));
    }

    #[test]
    fn run_git_on_nonexistent_dir_returns_error() {
        let error = run_git(Path::new("/tmp/__batty_nonexistent_dir__"), &["status"]).unwrap_err();
        // Git fails with a permanent error when the dir doesn't exist
        assert!(!error.is_transient());
    }

    #[test]
    fn rev_parse_branch_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = rev_parse_branch(tmp.path()).unwrap_err();
        assert!(matches!(
            error,
            GitError::Permanent { .. } | GitError::RevParseFailed { .. }
        ));
    }

    #[test]
    fn rev_parse_toplevel_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = rev_parse_toplevel(tmp.path()).unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn status_porcelain_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = status_porcelain(tmp.path()).unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn rebase_on_nonexistent_branch_returns_error() {
        let tmp = init_repo();
        let error = rebase(tmp.path(), "nonexistent-branch-xyz").unwrap_err();
        assert!(matches!(
            error,
            GitError::RebaseFailed { .. } | GitError::Permanent { .. }
        ));
    }

    #[test]
    fn merge_nonexistent_branch_returns_merge_failed() {
        let tmp = init_repo();
        let error = merge(tmp.path(), "nonexistent-branch-xyz").unwrap_err();
        assert!(matches!(
            error,
            GitError::MergeFailed { .. } | GitError::Permanent { .. }
        ));
    }

    #[test]
    fn checkout_new_branch_invalid_start_returns_error() {
        let tmp = init_repo();
        let error = checkout_new_branch(tmp.path(), "test-branch", "nonexistent-ref").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn rev_list_count_invalid_range_returns_error() {
        let tmp = init_repo();
        let error = rev_list_count(tmp.path(), "nonexistent..also-nonexistent").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn worktree_add_duplicate_branch_returns_error() {
        let tmp = init_repo();
        let wt_path = tmp.path().join("worktree1");
        // "main" branch already exists — worktree add with -b main should fail
        let error = worktree_add(tmp.path(), &wt_path, "main", "HEAD").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn worktree_remove_nonexistent_path_returns_error() {
        let tmp = init_repo();
        let error =
            worktree_remove(tmp.path(), Path::new("/tmp/__batty_no_wt__"), false).unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn show_ref_exists_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = show_ref_exists(tmp.path(), "main").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn branch_delete_nonexistent_returns_error() {
        let tmp = init_repo();
        let error = branch_delete(tmp.path(), "nonexistent-branch-xyz").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn for_each_ref_branches_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = for_each_ref_branches(tmp.path()).unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn rebase_abort_without_active_rebase_returns_error() {
        let tmp = init_repo();
        let error = rebase_abort(tmp.path()).unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn merge_base_is_ancestor_invalid_commit_returns_error() {
        let tmp = init_repo();
        let error =
            merge_base_is_ancestor(tmp.path(), "nonexistent-ref", "also-nonexistent").unwrap_err();
        assert!(!error.is_transient());
    }

    #[test]
    fn format_git_command_includes_repo_dir_and_args() {
        let cmd = format_git_command(Path::new("/my/repo"), &["status", "--porcelain"]);
        assert_eq!(cmd, "git -C /my/repo status --porcelain");
    }

    #[test]
    fn worktree_list_on_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let error = worktree_list(tmp.path()).unwrap_err();
        assert!(!error.is_transient());
    }
}
