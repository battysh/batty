//! Merge orchestration extracted from the team daemon.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::daemon::TeamDaemon;
use super::events::TeamEvent;
use super::standup::MemberState;
use super::task_loop::{
    branch_is_merged_into, checkout_worktree_branch_from_main, current_worktree_branch,
    delete_branch, engineer_base_branch_name, read_task_title, run_tests_in_worktree,
};

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

    let (tests_passed, output_truncated) = run_tests_in_worktree(&worktree_dir)?;
    if tests_passed {
        let task_title = read_task_title(&board_dir, task_id);
        let lock =
            MergeLock::acquire(daemon.project_root()).context("failed to acquire merge lock")?;

        match merge_engineer_branch(daemon.project_root(), engineer)? {
            MergeOutcome::Success => {
                drop(lock);

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
                daemon.emit_event(TeamEvent::task_completed(engineer));
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

                    daemon.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));

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

                daemon.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));
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

    daemon.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));

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

    let rebase = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(&worktree_dir)
        .output()
        .context("failed to rebase engineer branch onto main")?;

    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ = std::process::Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&worktree_dir)
            .output();
        warn!(engineer = engineer_name, branch = %branch, "rebase conflict during merge");
        return Ok(MergeOutcome::RebaseConflict(stderr));
    }

    let output = std::process::Command::new("git")
        .args(["merge", &branch, "--no-edit"])
        .current_dir(project_root)
        .output()
        .context("git merge failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(engineer = engineer_name, branch = %branch, "git merge failed");
        return Ok(MergeOutcome::MergeFailure(stderr));
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

fn commits_ahead_of_main(worktree_dir: &Path) -> Result<u32> {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--count", "main..HEAD"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to run git rev-list --count main..HEAD")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-list --count main..HEAD failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u32>().with_context(|| {
        format!(
            "failed to parse git rev-list --count main..HEAD output: {:?}",
            stdout.trim()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, StandupConfig, WorkflowMode,
        WorkflowPolicy,
    };
    use crate::team::daemon::DaemonConfig;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::task_loop::{prepare_engineer_assignment_worktree, setup_engineer_worktree};
    use crate::team::test_support::{git, git_ok, git_stdout, init_git_repo};
    use std::collections::HashMap;
    use std::path::Path;

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

    fn make_test_daemon(project_root: &Path, members: Vec<MemberInstance>) -> TeamDaemon {
        TeamDaemon::new(DaemonConfig {
            project_root: project_root.to_path_buf(),
            team_config: super::super::config::TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members,
            pane_map: HashMap::new(),
        })
        .unwrap()
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
    fn reset_worktree_restores_engineer_base_branch_after_task_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            "eng-1/task-42",
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

        let branch_check = git(&repo, &["rev-parse", "--verify", "eng-1/task-42"]);
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
}
