//! Merge orchestration extracted from the team daemon.
//!
//! This module owns the completion path after an engineer reports a task as
//! done in a worktree-based flow. It validates that the branch contains real
//! work, runs the configured test gate, serializes merges with a lock, and
//! either lands the branch on `main` or escalates conflicts and failures back
//! through the daemon.
//!
//! The daemon calls into this module so the poll loop can stay focused on
//! orchestration while merge-specific retries and board transitions remain in
//! one place.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::artifact::append_test_timing_record;
#[cfg(test)]
use super::artifact::read_test_timing_log;
use super::daemon::TeamDaemon;
use super::task_loop::{
    branch_is_merged_into, checkout_worktree_branch_from_main, current_worktree_branch,
    delete_branch, engineer_base_branch_name, read_task_title, run_tests_in_worktree,
};

fn run_git_with_context(
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

fn describe_git_failure(repo_dir: &Path, args: &[&str], intent: &str, stderr: &str) -> String {
    format!(
        "failed while trying to {intent}: `git {}` in {} returned: {}",
        args.join(" "),
        repo_dir.display(),
        stderr.trim()
    )
}

pub(crate) struct MergeLock {
    path: PathBuf,
}

impl MergeLock {
    pub fn acquire(project_root: &Path) -> Result<Self> {
        let path = project_root.join(".batty").join("merge.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > std::time::Duration::from_secs(60) {
                        bail!("merge lock timeout after 60s: {}", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(error) => bail!("failed to acquire merge lock: {error}"),
            }
        }
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub(crate) enum MergeOutcome {
    Success,
    RebaseConflict(String),
    MergeFailure(String),
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

    if commits_ahead_of_main(&worktree_dir)? == 0 {
        let msg = "Completion rejected: your branch has no commits ahead of main. Commit your changes before reporting done again.";
        daemon.queue_message("batty", engineer, msg)?;
        daemon.mark_member_working(engineer);
        info!(
            engineer,
            task_id, "completion rejected because branch has no commits"
        );
        return Ok(());
    }

    let task_branch = current_worktree_branch(&worktree_dir)?;
    let test_started = Instant::now();
    let (tests_passed, output_truncated) = run_tests_in_worktree(&worktree_dir)?;
    let test_duration_ms = test_started.elapsed().as_millis() as u64;
    if tests_passed {
        let task_title = read_task_title(&board_dir, task_id);
        let lock =
            MergeLock::acquire(daemon.project_root()).context("failed to acquire merge lock")?;

        match merge_engineer_branch(daemon.project_root(), engineer)? {
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
                daemon.record_task_completed(engineer);
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

                    daemon.record_task_escalated(engineer, task_id.to_string());

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

                daemon.record_task_escalated(engineer, task_id.to_string());
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

    daemon.record_task_escalated(engineer, task_id.to_string());

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
    if let Err(error) = checkout_worktree_branch_from_main(&worktree_dir, &base_branch) {
        warn!(
            engineer = engineer_name,
            error = %error,
            "failed to reset worktree after merge"
        );
        return Ok(());
    }

    if previous_branch != base_branch
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

fn commits_ahead_of_main(worktree_dir: &Path) -> Result<u32> {
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::{prepare_engineer_assignment_worktree, setup_engineer_worktree};
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{
        engineer_member, git, git_ok, git_stdout, init_git_repo, manager_member,
    };
    use std::path::Path;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

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

    fn setup_completion_daemon(repo: &Path, engineer: &str) -> TeamDaemon {
        let members = vec![
            manager_member("manager", None),
            engineer_member(engineer, Some("manager"), true),
        ];
        make_test_daemon(repo, members)
    }

    #[test]
    fn commits_ahead_of_main_error_includes_command_and_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let error = commits_ahead_of_main(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("count commits ahead of main before accepting engineer completion"));
        assert!(error.contains("git rev-list --count main..HEAD"));
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
    fn merge_rejects_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let err = merge_engineer_branch(tmp.path(), "eng-1-1").unwrap_err();
        assert!(err.to_string().contains("no worktree found"));
    }

    #[test]
    fn merge_lock_acquire_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let lock_path = tmp.path().join(".batty").join("merge.lock");

        {
            let lock = MergeLock::acquire(tmp.path()).unwrap();
            assert!(lock_path.exists());
            drop(lock);
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn merge_lock_second_acquire_waits_for_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        let first_lock = MergeLock::acquire(tmp.path()).unwrap();
        let project_root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let acquired = Arc::new(AtomicBool::new(false));

        let thread_barrier = Arc::clone(&barrier);
        let thread_acquired = Arc::clone(&acquired);
        let handle = thread::spawn(move || {
            thread_barrier.wait();
            let second_lock = MergeLock::acquire(&project_root).unwrap();
            thread_acquired.store(true, Ordering::SeqCst);
            drop(second_lock);
        });

        barrier.wait();
        thread::sleep(Duration::from_millis(600));
        assert!(!acquired.load(Ordering::SeqCst));

        drop(first_lock);
        handle.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
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
    fn completion_routes_engineers_with_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: super::super::config::RoleType::Engineer,
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
    fn completion_gate_rejects_zero_commits() {
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
            role_type: super::super::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
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
            role_type: super::super::config::RoleType::Engineer,
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
            role_type: super::super::config::RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: super::super::config::RoleType::Engineer,
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
                .contains("no commits ahead of main")
        );
        assert!(
            engineer_messages[0]
                .body
                .contains("Commit your changes before reporting done again")
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.is_empty());
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

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.md"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: super::super::config::RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: super::super::config::RoleType::Engineer,
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
            super::super::artifact::record_test_timing(
                &timing_log,
                &super::super::artifact::TestTimingRecord {
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
            role_type: super::super::config::RoleType::Engineer,
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
}
