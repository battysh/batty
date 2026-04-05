//! Engineer completion handling — the daemon's entry point when an engineer
//! reports a task as done.
//!
//! Validates commits, runs tests, evaluates auto-merge policy, performs the
//! merge, and handles retries and escalation.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::team::artifact::append_test_timing_record;
#[cfg(test)]
use crate::team::artifact::read_test_timing_log;
use crate::team::auto_merge::{self, AutoMergeDecision};
use crate::team::daemon::TeamDaemon;
use crate::team::task_loop::{
    checkout_worktree_branch_from_main, current_worktree_branch, engineer_base_branch_name,
    read_task_title, run_tests_in_worktree,
};

use super::git_ops::{
    code_files_changed_from_main, commits_ahead_of_main, diff_stat_from_main,
    files_changed_from_main, now_unix, run_git_with_context,
};
use super::lock::{MergeLock, MergeOutcome};
use super::operations::merge_engineer_branch;

fn move_task_to_review(
    _daemon: &mut TeamDaemon,
    board_dir: &Path,
    task_id: u32,
    manager_name: Option<&str>,
    engineer: &str,
) {
    if let Err(error) = crate::team::task_cmd::transition_task(board_dir, task_id, "review") {
        warn!(
            engineer,
            task_id,
            error = %error,
            "failed to move task to review — attempting force via in-progress first"
        );
        // If the task is in an unexpected state (e.g. blocked), try
        // transitioning through in-progress first, then to review.
        // This prevents the stuck loop where completion fires repeatedly
        // but the state transition always fails.
        let _ = crate::team::task_cmd::transition_task(board_dir, task_id, "in-progress");
        if let Err(error2) = crate::team::task_cmd::transition_task(board_dir, task_id, "review") {
            warn!(
                engineer,
                task_id,
                error = %error2,
                "force review transition also failed — leaving task in current state"
            );
        }
    }
    if let Some(manager_name) = manager_name
        && let Err(error) = crate::team::task_cmd::assign_task_owners(
            board_dir,
            task_id,
            Some(engineer),
            Some(manager_name),
        )
    {
        warn!(
            engineer,
            task_id,
            manager = manager_name,
            error = %error,
            "failed to set review owner"
        );
    }
}

pub(crate) fn handle_engineer_completion(daemon: &mut TeamDaemon, engineer: &str) -> Result<()> {
    let Some(task_id) = daemon.active_task_id(engineer) else {
        return Ok(());
    };

    if !daemon.member_uses_worktrees(engineer) {
        return Ok(());
    }

    let worktree_dir = daemon.worktree_dir(engineer);
    let board_dir = daemon.board_dir();
    let board_dir_str = board_dir.to_string_lossy().to_string();
    let manager_name = daemon.manager_name(engineer);

    let total_commits = if daemon.is_multi_repo {
        multi_repo_commits_ahead_of_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        commits_ahead_of_main(&worktree_dir)?
    };

    if total_commits == 0 {
        let rejection_count = daemon
            .completion_rejection_counts
            .entry(engineer.to_string())
            .or_insert(0);
        *rejection_count += 1;
        let count = *rejection_count;

        const MAX_REJECTIONS_BEFORE_RESET: u32 = 3;

        warn!(
            engineer,
            task_id,
            rejection_count = count,
            "engineer idle but no commits on task branch — keeping task #{task_id} active for {engineer} (rejection {count}/{MAX_REJECTIONS_BEFORE_RESET})"
        );

        if count >= MAX_REJECTIONS_BEFORE_RESET {
            // Auto-recovery: the branch never diverged from main.
            // Reset the worktree to a fresh task branch so the engineer can start clean.
            warn!(
                engineer,
                task_id,
                "completion rejected {count} times — auto-resetting worktree branch for {engineer}"
            );
            let base_branch = engineer_base_branch_name(engineer);
            if let Err(error) = checkout_worktree_branch_from_main(&worktree_dir, &base_branch) {
                warn!(
                    engineer,
                    task_id,
                    error = %error,
                    "failed to auto-reset worktree to base branch"
                );
            }
            daemon.completion_rejection_counts.remove(engineer);

            // Escalate to manager so they know the engineer needs re-assignment
            if let Some(ref manager_name) = manager_name {
                let msg = format!(
                    "[daemon] Engineer {engineer} reported completion {count} times for task #{task_id} but branch has no commits. Worktree has been auto-reset. The engineer may need a clearer task specification or is not committing work. Please re-assign or investigate."
                );
                daemon.queue_message("daemon", manager_name, &msg)?;
            }
            info!(
                engineer,
                task_id,
                rejection_count = count,
                "auto-reset worktree after repeated completion rejections (branch never diverged from main)"
            );
        } else {
            // Check if there are uncommitted changes to give a more helpful message
            let has_dirty_files = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&worktree_dir)
                .output()
                .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
                .unwrap_or(false);

            let msg = if has_dirty_files {
                format!(
                    "Completion rejected ({count}/{MAX_REJECTIONS_BEFORE_RESET}): you have uncommitted changes but no commits ahead of main. Your work will be LOST if you don't commit. Run these commands NOW:\n\
                    ```\n\
                    git add -A\n\
                    git commit -m \"your task description\"\n\
                    ```\n\
                    After {MAX_REJECTIONS_BEFORE_RESET} rejections, your worktree will be auto-reset and all uncommitted work will be destroyed."
                )
            } else {
                format!(
                    "Completion rejected ({count}/{MAX_REJECTIONS_BEFORE_RESET}): your branch has no commits ahead of main and no modified files. You need to actually create the deliverables for this task, then commit them:\n\
                    ```\n\
                    git add -A\n\
                    git commit -m \"your task description\"\n\
                    ```\n\
                    After {MAX_REJECTIONS_BEFORE_RESET} rejections, your worktree will be auto-reset."
                )
            };
            daemon.queue_message("batty", engineer, &msg)?;
        }
        return Ok(());
    }

    // Clear rejection counter on successful completion
    daemon.completion_rejection_counts.remove(engineer);

    // --- Narration-only quality gate ---
    // Commits exist, but check if any files actually changed. If the agent produced
    // commits with only commentary (e.g. empty commits, metadata-only), reject.
    let diff_stat = if daemon.is_multi_repo {
        multi_repo_diff_stat_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        diff_stat_from_main(&worktree_dir)?
    };
    let files_changed = if daemon.is_multi_repo {
        multi_repo_files_changed_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        files_changed_from_main(&worktree_dir)?
    };
    let code_files_changed = if daemon.is_multi_repo {
        multi_repo_code_files_changed_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        code_files_changed_from_main(&worktree_dir)?
    };

    if files_changed == 0 || code_files_changed == 0 {
        let narration_count = daemon
            .narration_rejection_counts
            .entry(task_id)
            .or_insert(0);
        *narration_count += 1;
        let count = *narration_count;

        const MAX_NARRATION_REJECTIONS: u32 = 2;

        warn!(
            engineer,
            task_id,
            narration_rejection_count = count,
            diff_stat = %diff_stat,
            files_changed,
            code_files_changed,
            "commits exist but only narration/non-code diff — narration-only completion"
        );
        daemon.record_narration_rejection(engineer, task_id, count);

        if count == MAX_NARRATION_REJECTIONS {
            // Escalate to manager
            if let Some(ref manager_name) = manager_name {
                let msg = format!(
                    "[daemon] Engineer {engineer} hit the narration-only quality gate {count} times on task #{task_id}. \
                     Branch has {total_commits} commit(s) ahead of main, but the completion diff is narration-only ({files_changed} total file(s), {code_files_changed} code file(s)).\n\
                     Follow-up prompt sent: \"Your previous attempt only produced commentary. Execute the actual commands to make the code changes.\"\n\
                     Diff stat: {}\n\
                     The agent may need a more directive prompt or task restart.",
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    }
                );
                daemon.queue_message("daemon", manager_name, &msg)?;
            }
        }

        let msg = format!(
            "Your previous attempt only produced commentary. Execute the actual commands to make the code changes.\n\
             Batty checked `git diff --stat main..HEAD` and found no code changes.\n\
             Commits ahead of main: {total_commits}\n\
             Files changed: {files_changed}\n\
             Code files changed: {code_files_changed}\n\
             Diff stat: {}",
            if diff_stat.is_empty() {
                "(empty)".to_string()
            } else {
                diff_stat.clone()
            }
        );
        daemon.queue_message("batty", engineer, &msg)?;
        return Ok(());
    }

    // Clear narration rejection counter on real file changes
    daemon.narration_rejection_counts.remove(&task_id);

    let task_branch = if daemon.is_multi_repo {
        multi_repo_task_branch(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        current_worktree_branch(&worktree_dir)?
    };
    let test_started = Instant::now();
    // For multi-repo, run tests from the engineer's worktree root (parent of sub-repos)
    let (tests_passed, output_truncated) = run_tests_in_worktree(
        &worktree_dir,
        daemon
            .config
            .team_config
            .workflow_policy
            .test_command
            .as_deref(),
    )?;
    let test_duration_ms = test_started.elapsed().as_millis() as u64;
    if tests_passed {
        let task_title = read_task_title(&board_dir, task_id);

        // --- Confidence scoring (always runs for observability) ---
        let policy = daemon.config.team_config.workflow_policy.auto_merge.clone();
        let auto_merge_override = daemon.auto_merge_override(task_id);

        // Analyze diff and emit confidence score for every completed task
        let diff_analysis = auto_merge::analyze_diff(daemon.project_root(), "main", &task_branch);
        if let Ok(ref summary) = diff_analysis {
            let confidence = auto_merge::compute_merge_confidence(summary, &policy);
            let task_str = task_id.to_string();
            let info = super::super::events::MergeConfidenceInfo {
                engineer,
                task: &task_str,
                confidence,
                files_changed: summary.files_changed,
                lines_changed: summary.total_lines(),
                has_migrations: summary.has_migrations,
                has_config_changes: summary.has_config_changes,
                rename_count: summary.rename_count,
            };
            daemon.record_merge_confidence_scored(&info);
        }

        // If override explicitly disables auto-merge, route to manual review
        if auto_merge_override == Some(false) {
            info!(
                engineer,
                task_id, "auto-merge disabled by per-task override, routing to manual review"
            );
            move_task_to_review(
                daemon,
                &board_dir,
                task_id,
                manager_name.as_deref(),
                engineer,
            );
            if let Some(ref manager_name) = manager_name {
                let msg = format!(
                    "[{engineer}] Task #{task_id} passed tests. Auto-merge disabled by override — awaiting manual review.\nTitle: {task_title}"
                );
                daemon.queue_message(engineer, manager_name, &msg)?;
                daemon.mark_member_working(manager_name);
            }
            daemon.clear_active_task(engineer);
            daemon.record_task_completed(engineer, Some(task_id));
            daemon.set_member_idle(engineer);
            return Ok(());
        }

        // Evaluate auto-merge if policy is enabled or override forces it
        let should_try_auto_merge = auto_merge_override == Some(true) || policy.enabled;
        if should_try_auto_merge {
            match diff_analysis {
                Ok(ref summary) => {
                    let decision = if auto_merge_override == Some(true) {
                        // Force auto-merge regardless of policy thresholds
                        AutoMergeDecision::AutoMerge {
                            confidence: auto_merge::compute_merge_confidence(summary, &policy),
                        }
                    } else {
                        auto_merge::should_auto_merge(summary, &policy, true)
                    };

                    match decision {
                        AutoMergeDecision::AutoMerge { confidence } => {
                            info!(
                                engineer,
                                task_id,
                                confidence,
                                files = summary.files_changed,
                                lines = summary.total_lines(),
                                "auto-merging task"
                            );
                            daemon.record_task_auto_merged(
                                engineer,
                                task_id,
                                confidence,
                                summary.files_changed,
                                summary.total_lines(),
                            );
                            // Fall through to normal merge path below
                        }
                        AutoMergeDecision::ManualReview {
                            confidence,
                            reasons,
                        } => {
                            info!(
                                engineer,
                                task_id,
                                confidence,
                                ?reasons,
                                "routing to manual review"
                            );
                            move_task_to_review(
                                daemon,
                                &board_dir,
                                task_id,
                                manager_name.as_deref(),
                                engineer,
                            );
                            if let Some(ref manager_name) = manager_name {
                                let reason_text = reasons.join("; ");
                                let msg = format!(
                                    "[{engineer}] Task #{task_id} passed tests but requires manual review.\nTitle: {task_title}\nConfidence: {confidence:.2}\nReasons: {reason_text}"
                                );
                                daemon.queue_message(engineer, manager_name, &msg)?;
                                daemon.mark_member_working(manager_name);
                            }
                            daemon.clear_active_task(engineer);
                            daemon.record_task_completed(engineer, Some(task_id));
                            daemon.set_member_idle(engineer);
                            return Ok(());
                        }
                    }
                }
                Err(ref error) => {
                    warn!(engineer, task_id, error = %error, "auto-merge diff analysis failed, falling through to normal merge");
                }
            }
        }

        let lock =
            MergeLock::acquire(daemon.project_root()).context("failed to acquire merge lock")?;

        match if daemon.is_multi_repo {
            merge_multi_repo_engineer_branch(
                daemon.project_root(),
                engineer,
                &daemon.sub_repo_names,
            )?
        } else {
            merge_engineer_branch(daemon.project_root(), engineer)?
        } {
            MergeOutcome::Success => {
                drop(lock);

                if let Err(error) = record_merge_test_timing(
                    daemon,
                    task_id,
                    engineer,
                    &task_branch,
                    test_duration_ms,
                ) {
                    warn!(
                        engineer,
                        task_id,
                        error = %error,
                        "failed to record merge test timing"
                    );
                }

                let board_update_ok = daemon.run_kanban_md_nonfatal(
                    &[
                        "move",
                        &task_id.to_string(),
                        "done",
                        "--claim",
                        engineer,
                        "--dir",
                        &board_dir_str,
                    ],
                    &format!("move task #{task_id} to done"),
                    manager_name
                        .as_deref()
                        .into_iter()
                        .chain(std::iter::once(engineer)),
                );

                if let Some(ref manager_name) = manager_name {
                    let msg = format!(
                        "[{engineer}] Task #{task_id} completed.\nTitle: {task_title}\nTests: passed\nMerge: success{}",
                        if board_update_ok {
                            ""
                        } else {
                            "\nBoard: update failed; decide next board action manually."
                        }
                    );
                    daemon.queue_message(engineer, manager_name, &msg)?;
                    daemon.mark_member_working(manager_name);
                }

                if let Some(ref manager_name) = manager_name {
                    let rollup = format!(
                        "Rollup: Task #{task_id} completed by {engineer}. Tests passed, merged to main.{}",
                        if board_update_ok {
                            ""
                        } else {
                            " Board automation failed; decide manually."
                        }
                    );
                    daemon.notify_reports_to(manager_name, &rollup)?;
                }

                daemon.clear_active_task(engineer);
                daemon.record_task_completed(engineer, Some(task_id));
                daemon.set_member_idle(engineer);
            }
            MergeOutcome::RebaseConflict(conflict_info) => {
                drop(lock);

                let attempt = daemon.increment_retry(engineer);
                if attempt <= 2 {
                    let msg = format!(
                        "Merge conflict during rebase onto main (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
                    );
                    daemon.queue_message("batty", engineer, &msg)?;
                    daemon.mark_member_working(engineer);
                    info!(engineer, attempt, "rebase conflict, sending back for retry");
                } else {
                    if let Some(ref manager_name) = manager_name {
                        let msg = format!(
                            "[{engineer}] task #{task_id} has unresolvable merge conflicts after 2 retries. Escalating.\n{conflict_info}"
                        );
                        daemon.queue_message(engineer, manager_name, &msg)?;
                        daemon.mark_member_working(manager_name);
                    }

                    daemon.record_task_escalated(
                        engineer,
                        task_id.to_string(),
                        Some("merge_conflict"),
                    );

                    if let Some(ref manager_name) = manager_name {
                        let escalation = format!(
                            "ESCALATION: Task #{task_id} assigned to {engineer} has unresolvable merge conflicts. Task blocked on board."
                        );
                        daemon.notify_reports_to(manager_name, &escalation)?;
                    }

                    daemon.run_kanban_md_nonfatal(
                        &[
                            "edit",
                            &task_id.to_string(),
                            "--block",
                            "merge conflicts after 2 retries",
                            "--dir",
                            &board_dir_str,
                        ],
                        &format!("block task #{task_id} after merge conflict retries"),
                        manager_name
                            .as_deref()
                            .into_iter()
                            .chain(std::iter::once(engineer)),
                    );

                    daemon.clear_active_task(engineer);
                    daemon.set_member_idle(engineer);
                }
            }
            MergeOutcome::MergeFailure(merge_info) => {
                drop(lock);

                let manager_notice = format!(
                    "Task #{task_id} from {engineer} passed tests but could not be merged to main.\n{merge_info}\nDecide whether to clean the main worktree, retry the merge, or redirect the engineer."
                );
                if let Some(ref manager_name) = manager_name {
                    daemon.queue_message("daemon", manager_name, &manager_notice)?;
                    daemon.mark_member_working(manager_name);
                    daemon.notify_reports_to(manager_name, &manager_notice)?;
                }

                let engineer_notice = format!(
                    "Your task passed tests, but Batty could not merge it into main.\n{merge_info}\nWait for lead direction before making more changes."
                );
                daemon.queue_message("daemon", engineer, &engineer_notice)?;

                daemon.record_task_escalated(engineer, task_id.to_string(), Some("merge_failure"));
                daemon.clear_active_task(engineer);
                daemon.set_member_idle(engineer);
                warn!(
                    engineer,
                    task_id,
                    error = %merge_info,
                    "merge into main failed after passing tests; escalated without exiting daemon"
                );
            }
        }
        return Ok(());
    }

    let attempt = daemon.increment_retry(engineer);
    if attempt <= 2 {
        let msg = format!(
            "Tests failed (attempt {attempt}/2). Fix the failures and try again:\n{output_truncated}"
        );
        daemon.queue_message("batty", engineer, &msg)?;
        daemon.mark_member_working(engineer);
        info!(engineer, attempt, "test failure, sending back for retry");
        return Ok(());
    }

    if let Some(ref manager_name) = manager_name {
        let msg = format!(
            "[{engineer}] task #{task_id} failed tests after 2 retries. Escalating.\nLast output:\n{output_truncated}"
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);
    }

    daemon.record_task_escalated(engineer, task_id.to_string(), Some("tests_failed"));

    if let Some(ref manager_name) = manager_name {
        let escalation = format!(
            "ESCALATION: Task #{task_id} assigned to {engineer} failed tests after 2 retries. Task blocked on board."
        );
        daemon.notify_reports_to(manager_name, &escalation)?;
    }

    daemon.run_kanban_md_nonfatal(
        &[
            "edit",
            &task_id.to_string(),
            "--block",
            "tests failed after 2 retries",
            "--dir",
            &board_dir_str,
        ],
        &format!("block task #{task_id} after max test retries"),
        manager_name
            .as_deref()
            .into_iter()
            .chain(std::iter::once(engineer)),
    );

    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    info!(engineer, task_id, "escalated to manager after max retries");
    Ok(())
}

/// Merge engineer branches across all sub-repos in a multi-repo project.
pub(crate) fn merge_multi_repo_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
    sub_repo_names: &[String],
) -> Result<MergeOutcome> {
    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        let sub_wt = project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer_name)
            .join(repo_name);
        if !sub_wt.exists() {
            continue;
        }
        // Check if there are commits to merge in this sub-repo
        if commits_ahead_of_main(&sub_wt).unwrap_or(0) == 0 {
            continue;
        }
        match merge_engineer_branch_in_repo(&repo_root, &sub_wt, engineer_name)? {
            MergeOutcome::Success => {}
            other => return Ok(other),
        }
    }

    // Reset all sub-repo worktrees after successful merge
    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        if let Err(e) = reset_engineer_worktree_in_repo(&repo_root, engineer_name, repo_name) {
            warn!(
                engineer = engineer_name,
                repo = repo_name,
                error = %e,
                "worktree reset failed after multi-repo merge"
            );
        }
    }

    Ok(MergeOutcome::Success)
}

/// Merge a single sub-repo's engineer worktree branch into main.
fn merge_engineer_branch_in_repo(
    repo_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
) -> Result<MergeOutcome> {
    let branch = current_worktree_branch(worktree_dir)?;
    info!(engineer = engineer_name, branch = %branch, repo = %repo_root.display(), "merging sub-repo worktree branch");

    let main_branch = current_worktree_branch(repo_root)?;
    if main_branch != "main" {
        let checkout = run_git_with_context(
            repo_root,
            &["checkout", "main"],
            "checkout main in sub-repo before merge",
        )?;
        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr).trim().to_string();
            return Ok(MergeOutcome::MergeFailure(format!(
                "sub-repo {} on '{main_branch}', checkout main failed: {stderr}",
                repo_root.display()
            )));
        }
    }

    let rebase = run_git_with_context(
        worktree_dir,
        &["rebase", "main"],
        &format!("rebase '{branch}' onto main in {}", repo_root.display()),
    )?;
    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ = run_git_with_context(worktree_dir, &["rebase", "--abort"], "abort rebase");
        return Ok(MergeOutcome::RebaseConflict(format!(
            "rebase conflict in {}: {stderr}",
            repo_root.display()
        )));
    }

    let output = run_git_with_context(
        repo_root,
        &["merge", &branch, "--no-edit"],
        &format!("merge '{branch}' into main in {}", repo_root.display()),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(MergeOutcome::MergeFailure(format!(
            "merge failed in {}: {stderr}",
            repo_root.display()
        )));
    }

    println!(
        "Merged branch '{branch}' from {engineer_name} in {}",
        repo_root.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(MergeOutcome::Success)
}

fn reset_engineer_worktree_in_repo(
    repo_root: &Path,
    engineer_name: &str,
    repo_name: &str,
) -> Result<()> {
    let worktree_dir = repo_root
        .parent()
        .unwrap_or(repo_root)
        .join(".batty")
        .join("worktrees")
        .join(engineer_name)
        .join(repo_name);
    if !worktree_dir.exists() {
        return Ok(());
    }
    let base_branch = engineer_base_branch_name(engineer_name);
    checkout_worktree_branch_from_main(&worktree_dir, &base_branch)?;
    Ok(())
}

fn record_merge_test_timing(
    daemon: &mut TeamDaemon,
    task_id: u32,
    engineer: &str,
    task_branch: &str,
    test_duration_ms: u64,
) -> Result<()> {
    let log_path = daemon
        .project_root()
        .join(".batty")
        .join("test_timing.jsonl");
    let record = append_test_timing_record(
        &log_path,
        task_id,
        engineer,
        task_branch,
        now_unix(),
        test_duration_ms,
    )?;

    if record.regression_detected {
        let rolling_average_ms = record.rolling_average_ms.unwrap_or_default();
        let regression_pct = record.regression_pct.unwrap_or_default();
        let reason = format!(
            "runtime_ms={} avg_ms={} pct={}",
            record.duration_ms, rolling_average_ms, regression_pct
        );
        daemon.record_performance_regression(task_id.to_string(), &reason);
        warn!(
            engineer,
            task_id,
            runtime_ms = record.duration_ms,
            rolling_average_ms,
            regression_pct,
            "post-merge test runtime exceeded rolling average"
        );
    }

    Ok(())
}

/// For multi-repo: sum commits ahead of main across all sub-repo worktrees.
fn multi_repo_commits_ahead_of_main(worktree_dir: &Path, sub_repo_names: &[String]) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += commits_ahead_of_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

/// For multi-repo: get the task branch from the first sub-repo that has commits.
fn multi_repo_task_branch(worktree_dir: &Path, sub_repo_names: &[String]) -> Result<String> {
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() && commits_ahead_of_main(&sub_wt).unwrap_or(0) > 0 {
            return current_worktree_branch(&sub_wt);
        }
    }
    // Fall back to first existing sub-repo
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            return current_worktree_branch(&sub_wt);
        }
    }
    bail!("no sub-repo worktrees found in {}", worktree_dir.display())
}

/// For multi-repo: sum files changed from main across all sub-repo worktrees.
fn multi_repo_files_changed_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += files_changed_from_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

fn multi_repo_code_files_changed_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += code_files_changed_from_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

fn multi_repo_diff_stat_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<String> {
    let mut stats = Vec::new();
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if !sub_wt.exists() {
            continue;
        }
        let diff_stat = diff_stat_from_main(&sub_wt).unwrap_or_default();
        if !diff_stat.is_empty() {
            stats.push(format!("[{name}]\n{diff_stat}"));
        }
    }
    Ok(stats.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AutoMergePolicy;
    use crate::team::events::read_events;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{engineer_member, git_ok, init_git_repo, manager_member};
    use std::path::{Path, PathBuf};

    fn write_task_file(project_root: &Path, id: u32, title: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        (worktree_dir, team_config_dir)
    }

    fn setup_completion_daemon(repo: &Path, engineer: &str) -> crate::team::daemon::TeamDaemon {
        let members = vec![
            manager_member("manager", None),
            engineer_member(engineer, Some("manager"), true),
        ];
        make_test_daemon(repo, members)
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

    fn setup_auto_merge_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-auto-merge-test");
        write_task_file(&repo, 42, "auto-merge-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        // Create a small change in the worktree
        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);

        (tmp, repo, worktree_dir)
    }

    fn auto_merge_daemon(repo: &Path, policy: AutoMergePolicy) -> crate::team::daemon::TeamDaemon {
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(repo, members);
        daemon.config.team_config.workflow_policy.auto_merge = policy;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon
    }

    #[test]
    fn completion_routes_engineers_with_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn completion_gate_rejects_zero_commits_but_keeps_task_active() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "zero-commit-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Task stays active — engineer still owns it (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
    }

    #[test]
    fn completion_gate_passes_with_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "commit-gate-success");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        let timing_log = repo.join(".batty").join("test_timing.jsonl");
        let timings = read_test_timing_log(&timing_log).unwrap();
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].task_id, 42);
        assert_eq!(timings[0].engineer, "eng-1");
        assert_eq!(timings[0].branch, "eng-1");
        assert!(!timings[0].regression_detected);
    }

    #[test]
    fn zero_commit_retry_message_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "zero-commit-message");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: crate::team::config::RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![manager, engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "batty");
        assert!(
            engineer_messages[0]
                .body
                .contains("no commits ahead of main"),
            "rejection message should mention no commits: {:?}",
            engineer_messages[0].body
        );
        assert!(
            engineer_messages[0].body.contains("git add -A"),
            "rejection message should include commit instructions: {:?}",
            engineer_messages[0].body
        );

        // On first rejection, manager is NOT notified (only after 3 rejections)
        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.is_empty());
    }

    #[test]
    fn narration_only_completion_retries_then_escalates_after_two_rejections() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "narration-only-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        git_ok(
            &worktree_dir,
            &["commit", "--allow-empty", "-m", "commentary only"],
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(engineer_messages[0]
            .body
            .contains("Your previous attempt only produced commentary. Execute the actual commands to make the code changes."));
        assert!(
            engineer_messages[0]
                .body
                .contains("git diff --stat main..HEAD")
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.is_empty());
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 2);

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert!(
            manager_messages[0]
                .body
                .contains("hit the narration-only quality gate 2 times")
        );
        assert!(
            manager_messages[0]
                .body
                .contains("completion diff is narration-only")
        );

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let narration_events: Vec<_> = events
            .iter()
            .filter(|event| event.event == "narration_rejection")
            .collect();
        assert_eq!(narration_events.len(), 2);
        assert_eq!(narration_events[0].task.as_deref(), Some("42"));
        assert_eq!(
            narration_events[1].reason.as_deref(),
            Some("rejection_count=2")
        );
    }

    #[test]
    fn narration_rejection_count_is_task_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "first-task");
        write_task_file(&repo, 43, "second-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        git_ok(
            &worktree_dir,
            &["commit", "--allow-empty", "-m", "commentary only"],
        );

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();
        assert_eq!(daemon.narration_rejection_counts.get(&42), Some(&1));

        daemon.clear_active_task("eng-1");
        assert!(!daemon.narration_rejection_counts.contains_key(&42));

        daemon.set_active_task_for_test("eng-1", 43);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();
        assert_eq!(daemon.narration_rejection_counts.get(&43), Some(&1));
    }

    #[test]
    fn narration_only_completion_rejects_docs_only_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "docs-only-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::create_dir_all(worktree_dir.join("docs")).unwrap();
        std::fs::write(
            worktree_dir.join("docs").join("notes.md"),
            "commentary only\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "docs/notes.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "docs only"]);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(engineer_messages[0].body.contains("found no code changes"));
        assert!(engineer_messages[0].body.contains("Code files changed: 0"));

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "narration_rejection"
                && event.task.as_deref() == Some("42")
                && event.role.as_deref() == Some("eng-1")
        }));
    }

    #[test]
    fn no_commits_rejection_keeps_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "no-commits-keep");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let mut daemon = setup_completion_daemon(&repo, "eng-1");

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Assignment kept — engineer still owns the task (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn no_commits_rejection_does_not_retry_and_keeps_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "no-commits-no-retry");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let mut daemon = setup_completion_daemon(&repo, "eng-1");

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Retry count should not be incremented
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
        // Active task kept — engineer still owns it (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn rebase_conflict_first_retry_messages_engineer() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-retry");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "batty");
        assert!(
            engineer_messages[0]
                .body
                .contains("Merge conflict during rebase onto main")
        );
    }

    #[test]
    fn rebase_conflict_first_retry_keeps_task_active_and_counts_retry() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-state");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.retry_count_for_test("eng-1"), Some(1));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }

    #[test]
    fn rebase_conflict_third_attempt_escalates_to_manager() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-escalation");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.iter().any(|msg| {
            msg.from == "eng-1"
                && msg
                    .body
                    .contains("unresolvable merge conflicts after 2 retries")
        }));
    }

    #[test]
    fn rebase_conflict_third_attempt_clears_task_and_sets_idle() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-reset");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
    }

    #[test]
    fn rebase_conflict_third_attempt_records_escalation_event() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-event");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_escalated"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("42")
        }));
    }

    #[test]
    fn handle_engineer_completion_escalates_merge_failures_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "merge-blocked-task");

        std::fs::write(repo.join("journal.rs"), "fn base() {}\n").unwrap();
        git_ok(&repo, &["add", "journal.rs"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.rs"), "fn engineer() {}\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.rs"), "fn dirty_main() {}\n").unwrap();

        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: crate::team::config::RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: crate::team::config::RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: true,
            },
        ];

        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert_eq!(manager_messages[0].from, "daemon");
        assert!(
            manager_messages[0]
                .body
                .contains("could not be merged to main")
        );
        assert!(
            manager_messages[0]
                .body
                .contains("would be overwritten by merge")
                || manager_messages[0]
                    .body
                    .contains("Please commit your changes or stash them")
        );

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "daemon");
        assert!(
            engineer_messages[0]
                .body
                .contains("could not merge it into main")
        );
    }

    #[test]
    fn handle_engineer_completion_emits_performance_regression_event() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "runtime-regression-task");

        let timing_log = repo.join(".batty").join("test_timing.jsonl");
        for task_id in 1..=5 {
            crate::team::artifact::record_test_timing(
                &timing_log,
                &crate::team::artifact::TestTimingRecord {
                    task_id,
                    engineer: "eng-1".to_string(),
                    branch: format!("eng-1/task-{task_id}"),
                    measured_at: 1_777_000_000 + task_id as u64,
                    duration_ms: 1,
                    rolling_average_ms: Some(1),
                    regression_pct: Some(0),
                    regression_detected: false,
                },
            )
            .unwrap();
        }

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "performance_regression"
                && event.task.as_deref() == Some("42")
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("runtime_ms="))
        }));

        let timings = read_test_timing_log(&timing_log).unwrap();
        assert_eq!(timings.len(), 6);
        assert!(timings.last().unwrap().regression_detected);
    }

    #[test]
    fn completion_auto_merges_small_clean_diff() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Task should be completed and cleared
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        // note.txt should be merged into main
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        // Verify auto-merge event was emitted
        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let auto_merge_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "task_auto_merged")
            .collect();
        assert_eq!(auto_merge_events.len(), 1);
        assert_eq!(auto_merge_events[0].role.as_deref(), Some("eng-1"));
        assert_eq!(auto_merge_events[0].task.as_deref(), Some("42"));
    }

    #[test]
    fn completion_routes_large_diff_to_review() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-auto-merge-test");
        write_task_file(&repo, 42, "large-diff-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        // Create a large diff: many files across multiple modules
        for i in 0..10 {
            let dir = worktree_dir.join(format!("module_{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let content: String = (0..50).map(|j| format!("line {j}\n")).collect();
            std::fs::write(dir.join("file.rs"), content).unwrap();
        }
        git_ok(&worktree_dir, &["add", "."]);
        git_ok(&worktree_dir, &["commit", "-m", "large change"]);

        let policy = AutoMergePolicy {
            enabled: true,
            max_files_changed: 5,
            max_diff_lines: 200,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-large-diff-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");
        assert_eq!(task.review_owner.as_deref(), Some("manager"));
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|m| m.body.contains("manual review")),
            "manager should receive manual review message: {:?}",
            manager_messages
        );
    }

    #[test]
    fn completion_respects_disabled_policy() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        // Default policy has enabled: false
        let policy = AutoMergePolicy::default();
        assert!(!policy.enabled);

        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // With auto-merge disabled, should fall through to normal merge (no review gate)
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        // note.txt merged into main
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        // No auto-merge event should be emitted
        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        assert!(
            !events.iter().any(|e| e.event == "task_auto_merged"),
            "no auto-merge event should be emitted when policy is disabled"
        );
    }

    #[test]
    fn completion_respects_per_task_override() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);
        daemon.set_auto_merge_override(42, false); // Force manual review

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Manager should have received a message about override
        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|m| m.body.contains("Auto-merge disabled by override")),
            "manager should receive override message: {:?}",
            manager_messages
        );
    }

    #[test]
    fn auto_merge_emits_event() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let auto_event = events
            .iter()
            .find(|e| e.event == "task_auto_merged")
            .expect("should have task_auto_merged event");

        assert_eq!(auto_event.role.as_deref(), Some("eng-1"));
        assert_eq!(auto_event.task.as_deref(), Some("42"));
        // Confidence should be stored in load field
        assert!(auto_event.load.is_some());
        let confidence = auto_event.load.unwrap();
        assert!(
            confidence > 0.0 && confidence <= 1.0,
            "confidence should be between 0 and 1, got {}",
            confidence
        );
        // Reason should contain files and lines info
        assert!(
            auto_event
                .reason
                .as_ref()
                .is_some_and(|r| r.contains("files=") && r.contains("lines=")),
            "reason should contain diff stats: {:?}",
            auto_event.reason
        );
    }

    fn production_unwrap_expect_count(source: &str) -> usize {
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn production_merge_completion_has_no_unwrap_or_expect_calls() {
        let src = include_str!("completion.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production completion.rs should avoid unwrap/expect"
        );
    }
}
