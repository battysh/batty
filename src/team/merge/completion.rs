//! Engineer completion handling — the daemon's entry point when an engineer
//! reports a task as done.
//!
//! Validates commits, runs tests, evaluates auto-merge policy, performs the
//! merge, and handles retries and escalation.

use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::team::artifact::append_test_timing_record;
#[cfg(test)]
use crate::team::artifact::read_test_timing_log;
use crate::team::auto_merge::{self, AutoMergeDecision};
use crate::team::daemon::TeamDaemon;
use crate::team::task_loop::{current_worktree_branch, read_task_title, run_tests_in_worktree};

use super::git_ops::{commits_ahead_of_main, now_unix};
use super::lock::{MergeLock, MergeOutcome};
use super::operations::merge_engineer_branch;

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
        // Do NOT clear active task or set idle — the engineer still owns this task.
        // Clearing would orphan the board task in-progress with no engineer tracking it.
        let msg = "Completion rejected: your branch has no commits ahead of main. Commit your changes before reporting done again.";
        daemon.queue_message("batty", engineer, msg)?;
        warn!(
            engineer,
            task_id,
            "engineer idle but no commits on task branch — keeping task #{task_id} active for {engineer}"
        );
        return Ok(());
    }

    let task_branch = current_worktree_branch(&worktree_dir)?;
    let test_started = Instant::now();
    let (tests_passed, output_truncated) = run_tests_in_worktree(&worktree_dir)?;
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
            if let Some(ref manager_name) = manager_name {
                let msg = format!(
                    "[{engineer}] Task #{task_id} passed tests. Auto-merge disabled by override — awaiting manual review.\nTitle: {task_title}"
                );
                daemon.queue_message(engineer, manager_name, &msg)?;
                daemon.mark_member_working(manager_name);
            }
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
                            if let Some(ref manager_name) = manager_name {
                                let reason_text = reasons.join("; ");
                                let msg = format!(
                                    "[{engineer}] Task #{task_id} passed tests but requires manual review.\nTitle: {task_title}\nConfidence: {confidence:.2}\nReasons: {reason_text}"
                                );
                                daemon.queue_message(engineer, manager_name, &msg)?;
                                daemon.mark_member_working(manager_name);
                            }
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
