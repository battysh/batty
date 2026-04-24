//! Core merge and worktree-reset operations.
//!
//! `merge_engineer_branch` rebases an engineer's worktree branch onto trunk and
//! fast-forward merges it. `reset_engineer_worktree` returns the worktree to
//! the engineer's base branch after a successful merge.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::team::task_loop::{
    ADDITIVE_CONFLICT_AUTO_RESOLVE_FENCE, branch_is_merged_into,
    checkout_worktree_branch_from_trunk, current_worktree_branch, delete_branch,
    engineer_base_branch_name, is_worktree_safe_to_mutate, merge_additive_only_text,
    worktree_has_user_changes,
};
use crate::team::verification::{self, VerifyStatus};

use super::git_ops::{describe_git_failure, force_clean_worktree, run_git_with_context};
use super::lock::{MergeMode, MergeOutcome, MergeSuccess};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootMergePlan {
    mode: MergeMode,
    reason: Option<String>,
}

pub(crate) fn merge_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
) -> Result<MergeOutcome> {
    merge_engineer_branch_into_trunk(project_root, engineer_name, "main")
}

pub(crate) fn merge_engineer_branch_into_trunk(
    project_root: &Path,
    engineer_name: &str,
    trunk_branch: &str,
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

    // Fetch latest trunk into the worktree so rebase sees current state.
    let _ = run_git_with_context(
        &worktree_dir,
        &["fetch", ".", &format!("{trunk_branch}:{trunk_branch}")],
        &format!("fetch {trunk_branch} into worktree before rebase"),
    );

    let rebase = run_git_with_context(
        &worktree_dir,
        &["rebase", trunk_branch],
        &format!(
            "rebase engineer branch '{branch}' onto {trunk_branch} before merging for '{engineer_name}'"
        ),
    )?;

    if !rebase.status.success() {
        // Try to auto-resolve: for auto-generated files (.cargo/config.toml),
        // accept the main version and continue the rebase.
        let resolved = try_auto_resolve_rebase(&worktree_dir);
        if !resolved {
            let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
            let _ = run_git_with_context(
                &worktree_dir,
                &["rebase", "--abort"],
                &format!("abort rebase for engineer branch '{branch}' after conflict"),
            );
            warn!(engineer = engineer_name, branch = %branch, "rebase conflict during merge");
            return Ok(MergeOutcome::RebaseConflict(describe_git_failure(
                &worktree_dir,
                &["rebase", trunk_branch],
                &format!(
                    "rebase engineer branch '{branch}' onto {trunk_branch} before merging for '{engineer_name}'"
                ),
                &stderr,
            )));
        }
        info!(engineer = engineer_name, branch = %branch, "auto-resolved rebase conflicts");
    }

    let verification =
        verification::verify_project(&worktree_dir, project_root).with_context(|| {
            format!("run verification before merging branch '{branch}' for '{engineer_name}'")
        })?;
    if verification.status == VerifyStatus::Failed {
        let regressions = if verification.regressions.is_empty() {
            "unknown".to_string()
        } else {
            verification.regressions.join(", ")
        };
        let report_path = verification
            .report_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string());
        return Ok(MergeOutcome::MergeFailure(format!(
            "verification detected regressions for behavior(s): {regressions}. Report: {report_path}"
        )));
    }

    let merge_plan = plan_root_merge(project_root, trunk_branch).with_context(|| {
        format!("determine merge strategy for engineer branch '{branch}' from '{engineer_name}'")
    })?;
    let merge_result = match merge_plan.mode {
        MergeMode::DirectRoot => {
            merge_branch_into_root_trunk(project_root, engineer_name, &branch, trunk_branch)
        }
        MergeMode::IsolatedIntegration => merge_branch_via_isolated_integration(
            project_root,
            engineer_name,
            &branch,
            trunk_branch,
        ),
    };
    if let Err(error) = merge_result {
        let prefix = match merge_plan.mode {
            MergeMode::DirectRoot => "direct merge path failed",
            MergeMode::IsolatedIntegration => "isolated merge path failed",
        };
        let reason = merge_plan
            .reason
            .as_deref()
            .map(|reason| format!("{prefix}: {reason}: {error}"))
            .unwrap_or_else(|| format!("{prefix}: {error}"));
        warn!(
            engineer = engineer_name,
            branch = %branch,
            mode = ?merge_plan.mode,
            error = %reason,
            "merge execution failed"
        );
        return Ok(MergeOutcome::MergeFailure(reason));
    }

    println!("Merged branch '{branch}' from {engineer_name}");

    if let Err(error) = reset_engineer_worktree_to_trunk(project_root, engineer_name, trunk_branch)
    {
        warn!(
            engineer = engineer_name,
            error = %error,
            "worktree reset failed after merge"
        );
    }

    Ok(MergeOutcome::Success(MergeSuccess {
        mode: merge_plan.mode,
        reason: merge_plan.reason,
    }))
}

fn plan_root_merge(project_root: &Path, trunk_branch: &str) -> Result<RootMergePlan> {
    let branch = current_worktree_branch(project_root).unwrap_or_else(|_| "HEAD".to_string());
    let has_user_changes = worktree_has_user_changes(project_root)?;
    if branch == trunk_branch && !has_user_changes {
        return Ok(RootMergePlan {
            mode: MergeMode::DirectRoot,
            reason: None,
        });
    }

    let reason = match (branch.as_str(), has_user_changes) {
        (branch, true) if branch == trunk_branch => {
            format!("root {trunk_branch} checkout has local changes")
        }
        ("HEAD", true) => "root checkout is detached HEAD with local changes".to_string(),
        ("HEAD", false) => "root checkout is detached HEAD".to_string(),
        (other, true) => format!("root checkout is on '{other}' with local changes"),
        (other, false) => format!("root checkout is on '{other}' instead of '{trunk_branch}'"),
    };
    Ok(RootMergePlan {
        mode: MergeMode::IsolatedIntegration,
        reason: Some(reason),
    })
}

fn merge_branch_into_root_trunk(
    project_root: &Path,
    engineer_name: &str,
    branch: &str,
    trunk_branch: &str,
) -> Result<()> {
    let output = run_git_with_context(
        project_root,
        &["merge", branch, "--no-edit"],
        &format!("merge engineer branch '{branch}' from '{engineer_name}' into {trunk_branch}"),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "{}",
            describe_git_failure(
                project_root,
                &["merge", branch, "--no-edit"],
                &format!(
                    "merge engineer branch '{branch}' from '{engineer_name}' into {trunk_branch}"
                ),
                &stderr,
            )
        );
    }
    Ok(())
}

fn merge_branch_via_isolated_integration(
    project_root: &Path,
    engineer_name: &str,
    branch: &str,
    trunk_branch: &str,
) -> Result<()> {
    let trunk_before = run_git_with_context(
        project_root,
        &["rev-parse", &format!("refs/heads/{trunk_branch}")],
        &format!("capture {trunk_branch} ref before isolated merge"),
    )?;
    if !trunk_before.status.success() {
        bail!(
            "failed to read {trunk_branch} ref before isolated merge: {}",
            String::from_utf8_lossy(&trunk_before.stderr).trim()
        );
    }
    let trunk_before = String::from_utf8_lossy(&trunk_before.stdout)
        .trim()
        .to_string();
    let integration =
        crate::worktree::prepare_integration_worktree(project_root, "merge-main-", trunk_branch)?;
    let merge = run_git_with_context(
        integration.path(),
        &["merge", branch, "--no-edit"],
        &format!(
            "merge engineer branch '{branch}' from '{engineer_name}' in isolated integration worktree"
        ),
    )?;
    if !merge.status.success() {
        let stderr = String::from_utf8_lossy(&merge.stderr).trim().to_string();
        bail!(
            "{}",
            describe_git_failure(
                integration.path(),
                &["merge", branch, "--no-edit"],
                &format!(
                    "merge engineer branch '{branch}' from '{engineer_name}' in isolated integration worktree"
                ),
                &stderr,
            )
        );
    }

    let integration_head = run_git_with_context(
        integration.path(),
        &["rev-parse", "HEAD"],
        "capture isolated integration merge head",
    )?;
    if !integration_head.status.success() {
        bail!(
            "failed to read isolated integration merge head: {}",
            String::from_utf8_lossy(&integration_head.stderr).trim()
        );
    }
    let integration_head = String::from_utf8_lossy(&integration_head.stdout)
        .trim()
        .to_string();
    let advance_trunk = run_git_with_context(
        project_root,
        &[
            "update-ref",
            &format!("refs/heads/{trunk_branch}"),
            integration_head.as_str(),
            trunk_before.as_str(),
        ],
        &format!("advance {trunk_branch} ref after isolated merge"),
    )?;
    if !advance_trunk.status.success() {
        bail!(
            "failed to advance {trunk_branch} after isolated merge: {}",
            String::from_utf8_lossy(&advance_trunk.stderr).trim()
        );
    }
    Ok(())
}

/// Try to auto-resolve rebase conflicts for files that batty manages.
/// Returns true if all conflicts were resolved and the rebase completed.
fn try_auto_resolve_rebase(worktree_dir: &Path) -> bool {
    // List conflicted files
    let status = match run_git_with_context(
        worktree_dir,
        &["diff", "--name-only", "--diff-filter=U"],
        "list rebase conflicts",
    ) {
        Ok(out) => out,
        Err(_) => return false,
    };
    let raw = String::from_utf8_lossy(&status.stdout).trim().to_string();
    let conflicts: Vec<&str> = raw.lines().collect();

    if conflicts.is_empty() {
        return false;
    }

    // Auto-resolvable patterns: batty-managed configs and generated files
    let auto_resolvable = |path: &str| -> bool {
        path == ".cargo/config.toml"
            || path.ends_with(".cargo/config.toml")
            || path.starts_with("src/team/templates/batty_")
    };

    for conflict in &conflicts {
        if auto_resolvable(conflict) {
            // Accept main's version for managed files
            let _ = run_git_with_context(
                worktree_dir,
                &["checkout", "--theirs", conflict],
                &format!("auto-resolve conflict in {conflict} (accept main)"),
            );
            let _ = run_git_with_context(
                worktree_dir,
                &["add", conflict],
                &format!("stage auto-resolved {conflict}"),
            );
        } else if try_auto_resolve_additive_conflict(worktree_dir, conflict) {
            info!(
                file = conflict,
                "auto-resolved additive-only rebase conflict"
            );
        } else {
            // Non-auto-resolvable conflict — bail
            info!(
                file = conflict,
                "rebase conflict in non-managed file, cannot auto-resolve"
            );
            return false;
        }
    }

    // Continue the rebase — may need multiple rounds if there are stacked conflicts
    for _ in 0..20 {
        let cont = run_git_with_context(
            worktree_dir,
            &["rebase", "--continue"],
            "continue rebase after auto-resolve",
        );
        match cont {
            Ok(output) if output.status.success() => return true,
            Ok(_) => {
                // Another conflict — try to auto-resolve again
                let status = run_git_with_context(
                    worktree_dir,
                    &["diff", "--name-only", "--diff-filter=U"],
                    "list remaining rebase conflicts",
                );
                let remaining: Vec<String> = match status {
                    Ok(out) => String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .lines()
                        .map(String::from)
                        .collect(),
                    Err(_) => return false,
                };
                if remaining.is_empty() {
                    continue; // no conflicts, just needs --continue again
                }
                for r in &remaining {
                    if auto_resolvable(r) {
                        let _ = run_git_with_context(
                            worktree_dir,
                            &["checkout", "--theirs", r],
                            &format!("auto-resolve {r}"),
                        );
                        let _ =
                            run_git_with_context(worktree_dir, &["add", r], &format!("stage {r}"));
                    } else if try_auto_resolve_additive_conflict(worktree_dir, r) {
                        info!(file = r, "auto-resolved additive-only rebase conflict");
                    } else {
                        return false;
                    }
                }
            }
            Err(_) => return false,
        }
    }
    false
}

fn try_auto_resolve_additive_conflict(worktree_dir: &Path, path: &str) -> bool {
    if !ADDITIVE_CONFLICT_AUTO_RESOLVE_FENCE.contains(&path) {
        return false;
    }

    let Some(base) = read_conflict_stage(worktree_dir, 1, path) else {
        return false;
    };
    let Some(current) = read_conflict_stage(worktree_dir, 2, path) else {
        return false;
    };
    let Some(incoming) = read_conflict_stage(worktree_dir, 3, path) else {
        return false;
    };

    let Some(merged) = merge_additive_only_text(&base, &current, &incoming) else {
        return false;
    };

    std::fs::write(worktree_dir.join(path), merged).is_ok()
        && run_git_with_context(
            worktree_dir,
            &["add", path],
            &format!("stage additive-only conflict resolution for {path}"),
        )
        .is_ok_and(|output| output.status.success())
}

fn read_conflict_stage(worktree_dir: &Path, stage: u8, path: &str) -> Option<String> {
    let stage_spec = format!(":{stage}:{path}");
    let output = run_git_with_context(
        worktree_dir,
        &["show", stage_spec.as_str()],
        &format!("read stage {stage} for conflicted path {path}"),
    )
    .ok()?;
    if !output.status.success() || output.stdout.contains(&0) {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reset_engineer_worktree(project_root: &Path, engineer_name: &str) -> Result<()> {
    reset_engineer_worktree_to_trunk(project_root, engineer_name, "main")
}

pub(crate) fn reset_engineer_worktree_to_trunk(
    project_root: &Path,
    engineer_name: &str,
    trunk_branch: &str,
) -> Result<()> {
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

    if let Err(error) =
        checkout_worktree_branch_from_trunk(&worktree_dir, &base_branch, trunk_branch)
    {
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
        && branch_is_merged_into(project_root, &previous_branch, trunk_branch)?
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
        "reset worktree to trunk after merge"
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        (worktree_dir, team_config_dir)
    }

    fn write_script(path: &Path, lines: &[&str]) {
        let body = format!("#!/bin/sh\nprintf '%s\\n' {}\n", lines.join(" "));
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
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
        assert!(matches!(
            result,
            MergeOutcome::Success(MergeSuccess {
                mode: MergeMode::DirectRoot,
                ..
            })
        ));
        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("other.txt").exists());
    }

    #[test]
    fn merge_blocks_when_verification_detects_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(
            repo.join("PARITY.md"),
            r#"---
project: trivial
target: trivial.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 100%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Screen fill | complete | complete | complete | PASS | previous |
"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(".batty/verification.yml"),
            r#"behaviors:
  - behavior: Screen fill
    baseline: scripts/baseline.sh
    candidate: scripts/candidate.sh
    inputs: []
"#,
        )
        .unwrap();
        write_script(&repo.join("scripts/baseline.sh"), &["frame-a", "frame-b"]);
        write_script(&repo.join("scripts/candidate.sh"), &["frame-a", "frame-b"]);
        git_ok(
            &repo,
            &["add", "PARITY.md", ".batty/verification.yml", "scripts"],
        );
        git_ok(&repo, &["commit", "-m", "add verification fixtures"]);

        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-verify");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-verify", &team_config_dir).unwrap();

        write_script(
            &worktree_dir.join("scripts/candidate.sh"),
            &["frame-a", "frame-x"],
        );
        git_ok(&worktree_dir, &["add", "scripts/candidate.sh"]);
        git_ok(&worktree_dir, &["commit", "-m", "introduce regression"]);

        let result = merge_engineer_branch(&repo, "eng-verify").unwrap();
        match result {
            MergeOutcome::MergeFailure(message) => {
                assert!(message.contains("verification detected regressions"));
                assert!(message.contains("Screen fill"));
            }
            other => panic!("expected verification failure, got {other:?}"),
        }
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert_eq!(
            git_stdout(&repo, &["show", "main:scripts/candidate.sh"]),
            "#!/bin/sh\nprintf '%s\\n' frame-a frame-b"
        );
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
        assert!(matches!(result, MergeOutcome::Success(_)));

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

        assert!(matches!(result, MergeOutcome::Success(_)));
        assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn merge_empty_diff_resets_worktree_to_engineer_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-empty");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-empty", &team_config_dir).unwrap();

        let result = merge_engineer_branch(&repo, "eng-empty").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
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

        assert!(matches!(result, MergeOutcome::Success(_)));
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
        assert!(matches!(result, MergeOutcome::Success(_)));
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
        assert!(matches!(result, MergeOutcome::Success(_)));

        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/") && !line.starts_with("?? .cargo/"))
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

        assert!(matches!(result, MergeOutcome::Success(_)));
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

    #[test]
    fn merge_rebase_additive_conflict_keeps_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-2");
        let review_file = repo.join("src").join("team").join("review.rs");

        std::fs::create_dir_all(review_file.parent().unwrap()).unwrap();
        std::fs::write(
            &review_file,
            "const REVIEW_CHECKS: &[&str] = &[\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "src/team/review.rs"]);
        git_ok(&repo, &["commit", "-m", "add review checks"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("team").join("review.rs"),
            "const REVIEW_CHECKS: &[&str] = &[\n    \"engineer\",\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/team/review.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer review addition"]);

        std::fs::write(
            &review_file,
            "const REVIEW_CHECKS: &[&str] = &[\n    \"main\",\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "src/team/review.rs"]);
        git_ok(&repo, &["commit", "-m", "main review addition"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        let merged = std::fs::read_to_string(review_file).unwrap();
        assert!(merged.contains("\"main\""));
        assert!(merged.contains("\"engineer\""));
        assert!(merged.contains("\"existing\""));
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
    fn merge_with_dirty_main_uses_isolated_integration_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-3");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();
        match result {
            MergeOutcome::Success(success) => {
                assert_eq!(success.mode, MergeMode::IsolatedIntegration);
                assert!(
                    success
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("local changes")),
                    "expected dirty-root isolation reason, got {success:?}"
                );
                assert_eq!(
                    git_stdout(&repo, &["show", "main:feature.txt"]),
                    "engineer version"
                );
                assert_eq!(
                    std::fs::read_to_string(repo.join("journal.md")).unwrap(),
                    "dirty main\n"
                );
            }
            other => panic!("expected isolated merge success, got {other:?}"),
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
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::write(
            repo.join(".batty").join("integration-worktrees"),
            "not a directory\n",
        )
        .unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();

        assert!(matches!(result, MergeOutcome::MergeFailure(_)));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-3");
        assert!(
            git(&repo, &["rev-parse", "--verify", "eng-3"])
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("journal.md")).unwrap(),
            "dirty main\n"
        );
        assert!(
            !repo.join("feature.txt").exists(),
            "main ref should not advance when isolated merge preparation fails"
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
            .filter(|line| !line.starts_with("?? .batty/") && !line.starts_with("?? .cargo/"))
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
            MergeOutcome::Success(success) => {
                assert_eq!(success.mode, MergeMode::IsolatedIntegration);
                assert!(
                    success
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("detached HEAD")),
                    "expected detached-head isolation reason, got {success:?}"
                );
                let branch = git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
                assert_eq!(branch, "HEAD");
            }
            other => panic!("expected isolated merge success, got {other:?}"),
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
        assert!(matches!(
            result,
            MergeOutcome::Success(MergeSuccess {
                mode: MergeMode::DirectRoot,
                ..
            })
        ));
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
