//! Core merge and worktree-reset operations.
//!
//! `merge_engineer_branch` rebases an engineer's worktree branch onto main and
//! fast-forward merges it. `reset_engineer_worktree` returns the worktree to
//! the engineer's base branch after a successful merge.

use std::path::Path;

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::team::task_loop::{
    branch_is_merged_into, checkout_worktree_branch_from_main, current_worktree_branch,
    delete_branch, engineer_base_branch_name, is_worktree_safe_to_mutate,
};

use super::git_ops::{describe_git_failure, force_clean_worktree, run_git_with_context};
use super::lock::MergeOutcome;

pub(crate) fn merge_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
) -> Result<MergeOutcome> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        bail!(
            "no worktree found for '{}' at {}",
            engineer_name,
            worktree_dir.display()
        );
    }

    let branch = current_worktree_branch(&worktree_dir)?;
    info!(engineer = engineer_name, branch = %branch, "merging worktree branch");

    // Ensure project_root is on main before merging. Without this check,
    // the merge silently lands on whatever branch happens to be checked out,
    // causing "merge reported success but commits not on main" (#189, #198).
    let main_branch = current_worktree_branch(project_root)?;
    if main_branch != "main" {
        warn!(
            engineer = engineer_name,
            branch = %branch,
            actual_branch = %main_branch,
            "project root not on main before merge, attempting checkout"
        );
        let checkout = run_git_with_context(
            project_root,
            &["checkout", "main"],
            "checkout main in project root before merge",
        )?;
        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr).trim().to_string();
            return Ok(MergeOutcome::MergeFailure(format!(
                "project root is on '{main_branch}', not 'main', and checkout failed: {stderr}"
            )));
        }
    }

    let rebase = run_git_with_context(
        &worktree_dir,
        &["rebase", "main"],
        &format!(
            "rebase engineer branch '{branch}' onto main before merging for '{engineer_name}'"
        ),
    )?;

    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ = run_git_with_context(
            &worktree_dir,
            &["rebase", "--abort"],
            &format!("abort rebase for engineer branch '{branch}' after conflict"),
        );
        warn!(engineer = engineer_name, branch = %branch, "rebase conflict during merge");
        return Ok(MergeOutcome::RebaseConflict(describe_git_failure(
            &worktree_dir,
            &["rebase", "main"],
            &format!(
                "rebase engineer branch '{branch}' onto main before merging for '{engineer_name}'"
            ),
            &stderr,
        )));
    }

    let output = run_git_with_context(
        project_root,
        &["merge", &branch, "--no-edit"],
        &format!("merge engineer branch '{branch}' from '{engineer_name}' into main"),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(engineer = engineer_name, branch = %branch, "git merge failed");
        return Ok(MergeOutcome::MergeFailure(describe_git_failure(
            project_root,
            &["merge", &branch, "--no-edit"],
            &format!("merge engineer branch '{branch}' from '{engineer_name}' into main"),
            &stderr,
        )));
    }

    println!("Merged branch '{branch}' from {engineer_name}");

    if let Err(error) = reset_engineer_worktree(project_root, engineer_name) {
        warn!(
            engineer = engineer_name,
            error = %error,
            "worktree reset failed after merge"
        );
    }

    Ok(MergeOutcome::Success)
}

pub(crate) fn reset_engineer_worktree(project_root: &Path, engineer_name: &str) -> Result<()> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        return Ok(());
    }

    let previous_branch = current_worktree_branch(&worktree_dir)?;
    let base_branch = engineer_base_branch_name(engineer_name);

    // Guard: refuse to destroy uncommitted work on a task branch.
    if !is_worktree_safe_to_mutate(&worktree_dir)? {
        warn!(
            engineer = engineer_name,
            worktree = %worktree_dir.display(),
            "skipping worktree reset — uncommitted changes on task branch"
        );
        return Ok(());
    }

    // Force-clean uncommitted changes before switching branches.
    // Without this, `checkout -B` fails when the worktree is dirty.
    force_clean_worktree(&worktree_dir, engineer_name);

    if let Err(error) = checkout_worktree_branch_from_main(&worktree_dir, &base_branch) {
        warn!(
            engineer = engineer_name,
            current_branch = %previous_branch,
            expected_branch = %base_branch,
            error = %error,
            "failed to reset worktree after merge"
        );
        return Ok(());
    }

    // Verify HEAD landed on the base branch.
    match current_worktree_branch(&worktree_dir) {
        Ok(actual) if actual == base_branch => {}
        Ok(actual) => {
            warn!(
                engineer = engineer_name,
                current_branch = %actual,
                expected_branch = %base_branch,
                "worktree reset did not land on expected branch"
            );
        }
        Err(error) => {
            warn!(
                engineer = engineer_name,
                error = %error,
                "could not verify worktree branch after reset"
            );
        }
    }

    if previous_branch != base_branch
        && previous_branch != "HEAD"
        && (previous_branch == engineer_name
            || previous_branch.starts_with(&format!("{engineer_name}/")))
        && branch_is_merged_into(project_root, &previous_branch, "main")?
        && let Err(error) = delete_branch(project_root, &previous_branch)
    {
        warn!(
            engineer = engineer_name,
            branch = %previous_branch,
            error = %error,
            "failed to delete merged engineer task branch"
        );
    }

    info!(
        engineer = engineer_name,
        branch = %base_branch,
        worktree = %worktree_dir.display(),
        "reset worktree to main after merge"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::task_loop::{
        engineer_base_branch_name, prepare_engineer_assignment_worktree, setup_engineer_worktree,
    };
    use crate::team::test_support::{git, git_ok, git_stdout, init_git_repo};
    use std::path::PathBuf;

    fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        (worktree_dir, team_config_dir)
    }

    #[test]
    fn merge_rejects_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let err = merge_engineer_branch(tmp.path(), "eng-1-1").unwrap_err();
        assert!(err.to_string().contains("no worktree found"));
    }

    #[test]
    fn merge_with_rebase_picks_up_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        std::fs::write(repo.join("other.txt"), "main work\n").unwrap();
        git_ok(&repo, &["add", "other.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));
        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("other.txt").exists());
    }

    #[test]
    fn reset_worktree_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));

        let main_head = git_stdout(&repo, &["rev-parse", "HEAD"]);
        let worktree_head = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        assert_eq!(main_head, worktree_head);
    }

    #[test]
    fn merge_empty_diff_returns_success() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-empty");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-empty", &team_config_dir).unwrap();
        let main_before = git_stdout(&repo, &["rev-parse", "main"]);

        let result = merge_engineer_branch(&repo, "eng-empty").unwrap();

        assert!(matches!(result, MergeOutcome::Success));
        assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn merge_empty_diff_resets_worktree_to_engineer_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-empty");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-empty", &team_config_dir).unwrap();

        let result = merge_engineer_branch(&repo, "eng-empty").unwrap();

        assert!(matches!(result, MergeOutcome::Success));
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-empty")
        );
    }

    #[test]
    fn merge_with_two_main_advances_rebases_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-stale");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-stale", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        std::fs::write(repo.join("main-one.txt"), "main one\n").unwrap();
        git_ok(&repo, &["add", "main-one.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance 1"]);

        std::fs::write(repo.join("main-two.txt"), "main two\n").unwrap();
        git_ok(&repo, &["add", "main-two.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance 2"]);

        let result = merge_engineer_branch(&repo, "eng-stale").unwrap();

        assert!(matches!(result, MergeOutcome::Success));
        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("main-one.txt").exists());
        assert!(repo.join("main-two.txt").exists());
    }

    #[test]
    fn reset_worktree_restores_engineer_base_branch_after_task_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            "eng-1/42",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-1")
        );

        let branch_check = git(&repo, &["rev-parse", "--verify", "eng-1/42"]);
        assert!(
            !branch_check.status.success(),
            "merged task branch should be deleted"
        );
    }

    #[test]
    fn reset_worktree_leaves_clean_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("new.txt"), "content\n").unwrap();
        git_ok(&worktree_dir, &["add", "new.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add file"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success));

        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/"))
            .collect();
        assert!(
            tracked_changes.is_empty(),
            "worktree has tracked changes: {:?}",
            tracked_changes
        );
    }

    #[test]
    fn reset_worktree_noops_when_worktree_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");

        reset_engineer_worktree(&repo, "eng-missing").unwrap();
    }

    #[test]
    fn reset_worktree_keeps_unmerged_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-keep");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-keep",
            "eng-keep/77",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "keep me\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "unmerged feature"]);

        reset_engineer_worktree(&repo, "eng-keep").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-keep")
        );
        assert!(
            git(&repo, &["rev-parse", "--verify", "eng-keep/77"])
                .status
                .success()
        );
    }

    #[test]
    fn reset_worktree_deletes_merged_legacy_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-legacy");

        setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &engineer_base_branch_name("eng-legacy"),
            &team_config_dir,
        )
        .unwrap();
        git_ok(
            &worktree_dir,
            &["checkout", "-B", "eng-legacy/task-55", "main"],
        );
        std::fs::write(worktree_dir.join("legacy.txt"), "legacy branch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "legacy.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "legacy task work"]);
        git_ok(&repo, &["merge", "eng-legacy/task-55", "--no-edit"]);

        reset_engineer_worktree(&repo, "eng-legacy").unwrap();

        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-legacy/task-55"])
                .status
                .success()
        );
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-legacy")
        );
    }

    #[test]
    fn reset_worktree_keeps_non_engineer_namespace_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-keep");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-keep", &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-B", "feature/custom", "main"]);
        std::fs::write(worktree_dir.join("feature.txt"), "non engineer branch\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "feature branch work"]);

        reset_engineer_worktree(&repo, "eng-keep").unwrap();

        assert!(
            git(&repo, &["rev-parse", "--verify", "feature/custom"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_success_deletes_merged_engineer_branch_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-delete");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-delete", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "remove branch\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-delete").unwrap();

        assert!(matches!(result, MergeOutcome::Success));
        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-delete"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_rebase_conflict_returns_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "conflict.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();
        assert!(matches!(result, MergeOutcome::RebaseConflict(_)));

        let status = git(&worktree_dir, &["status", "--porcelain"]);
        assert!(status.status.success());
    }

    fn setup_rebase_conflict_repo(
        engineer: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, engineer);

        std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "conflict.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        (tmp, repo, worktree_dir, team_config_dir)
    }

    #[test]
    fn merge_rebase_conflict_aborts_rebase_state() {
        let (_tmp, repo, worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-4");

        let result = merge_engineer_branch(&repo, "eng-4").unwrap();

        assert!(matches!(result, MergeOutcome::RebaseConflict(_)));
        assert!(
            !git(&worktree_dir, &["rev-parse", "--verify", "REBASE_HEAD"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_with_dirty_main_returns_merge_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-3");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.md"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();
        match result {
            MergeOutcome::MergeFailure(stderr) => {
                assert!(
                    stderr.contains("would be overwritten by merge")
                        || stderr.contains("Please commit your changes or stash them"),
                    "unexpected merge failure stderr: {stderr}"
                );
            }
            other => panic!("expected merge failure outcome, got {other:?}"),
        }
    }

    #[test]
    fn merge_failure_retains_engineer_branch_for_manual_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-3");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.md"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();

        assert!(matches!(result, MergeOutcome::MergeFailure(_)));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-3");
        assert!(
            git(&repo, &["rev-parse", "--verify", "eng-3"])
                .status
                .success()
        );
    }

    #[test]
    fn reset_clears_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-reset");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-reset",
            "eng-reset/task-99",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("done.txt"), "work done\n").unwrap();
        git_ok(&worktree_dir, &["add", "done.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        // Merge the task branch into main so it's considered merged.
        git_ok(&repo, &["merge", "eng-reset/task-99", "--no-edit"]);

        reset_engineer_worktree(&repo, "eng-reset").unwrap();

        // Verify on base branch.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-reset")
        );
        // Verify task branch is deleted.
        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-reset/task-99"])
                .status
                .success(),
            "merged task branch should have been deleted"
        );
    }

    #[test]
    fn reset_handles_uncommitted_changes_on_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-dirty");
        let base = engineer_base_branch_name("eng-dirty");

        // Set up worktree on the base branch (not a task branch).
        setup_engineer_worktree(&repo, &worktree_dir, &base, &team_config_dir).unwrap();

        // Leave uncommitted staged and unstaged changes.
        std::fs::write(worktree_dir.join("staged.txt"), "staged\n").unwrap();
        git_ok(&worktree_dir, &["add", "staged.txt"]);
        std::fs::write(worktree_dir.join("unstaged.txt"), "unstaged\n").unwrap();

        // Reset should succeed — base branch is safe to mutate even when dirty.
        reset_engineer_worktree(&repo, "eng-dirty").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            base
        );
        // Worktree should be clean after reset.
        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/"))
            .collect();
        assert!(
            tracked_changes.is_empty(),
            "worktree should be clean after reset, got: {:?}",
            tracked_changes
        );
    }

    #[test]
    fn reset_skips_when_dirty_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-dirty-task");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-dirty-task",
            "eng-dirty-task/task-88",
            &team_config_dir,
        )
        .unwrap();

        // Leave uncommitted staged changes on a task branch.
        std::fs::write(worktree_dir.join("staged.txt"), "staged\n").unwrap();
        git_ok(&worktree_dir, &["add", "staged.txt"]);

        // Reset should skip — worktree is dirty on a task branch.
        reset_engineer_worktree(&repo, "eng-dirty-task").unwrap();

        // Worktree should remain on the task branch with changes intact.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-dirty-task/task-88"
        );
        assert!(worktree_dir.join("staged.txt").exists());
    }

    #[test]
    fn reset_handles_detached_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-detach");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-detach", &team_config_dir).unwrap();

        // Create a commit and detach HEAD.
        std::fs::write(worktree_dir.join("file.txt"), "content\n").unwrap();
        git_ok(&worktree_dir, &["add", "file.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "a commit"]);
        let commit_sha = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        git_ok(&worktree_dir, &["checkout", &commit_sha]);

        // Verify we are in detached HEAD state.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );

        // Reset should still check out the base branch.
        reset_engineer_worktree(&repo, "eng-detach").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-detach")
        );
    }

    #[test]
    fn merge_fails_when_project_root_not_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-off");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-off", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        // Move project root off main onto a detached HEAD.
        git_ok(&repo, &["checkout", "--detach", "HEAD"]);

        let result = merge_engineer_branch(&repo, "eng-off").unwrap();
        match result {
            MergeOutcome::Success => {
                let branch = git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
                assert_eq!(branch, "main");
            }
            MergeOutcome::MergeFailure(msg) => {
                assert!(
                    msg.contains("not 'main'"),
                    "expected branch mismatch message, got: {msg}"
                );
            }
            other => panic!("expected Success or MergeFailure, got {other:?}"),
        }
    }

    #[test]
    fn merge_succeeds_when_project_root_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-ok");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-ok", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );

        let result = merge_engineer_branch(&repo, "eng-ok").unwrap();
        assert!(matches!(result, MergeOutcome::Success));
        assert!(repo.join("feature.txt").exists());
    }

    #[test]
    fn reset_worktree_skips_when_dirty_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-wip");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-wip",
            "eng-wip/88",
            &team_config_dir,
        )
        .unwrap();

        // Create uncommitted changes on the task branch.
        std::fs::write(worktree_dir.join("wip.txt"), "work in progress\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);

        // reset_engineer_worktree should skip (not error) when dirty on task branch.
        reset_engineer_worktree(&repo, "eng-wip").unwrap();

        // Verify the worktree was NOT reset — still on task branch with changes.
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        assert_eq!(branch, "eng-wip/88");
        assert!(worktree_dir.join("wip.txt").exists());
    }
}
