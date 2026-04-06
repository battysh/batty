use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::TeamDaemon;
use crate::team::daemon::verification::run_automatic_verification;
use crate::team::merge::{MergeLock, MergeOutcome, merge_engineer_branch};
use crate::team::task_loop::read_task_title;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeRequest {
    pub task_id: u32,
    pub engineer: String,
    pub branch: String,
    pub worktree_dir: PathBuf,
    pub queued_at: Instant,
    pub test_passed: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeQueueOutcome {
    Success,
    Conflict,
    Reverted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeQueueEvent {
    pub task_id: u32,
    pub engineer: String,
    pub outcome: MergeQueueOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeQueueLastResult {
    task_id: u32,
    outcome: MergeQueueOutcome,
    finished_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct MergeQueue {
    queue: VecDeque<MergeRequest>,
    active: Option<MergeRequest>,
    last_result: Option<MergeQueueLastResult>,
    last_reported_status: Option<String>,
}

impl MergeQueue {
    pub(crate) fn enqueue(&mut self, request: MergeRequest) {
        self.queue.push_back(request);
    }

    #[allow(dead_code)]
    pub(crate) fn queued_len(&self) -> usize {
        self.queue.len()
    }

    #[allow(dead_code)]
    pub(crate) fn active_task_id(&self) -> Option<u32> {
        self.active.as_ref().map(|request| request.task_id)
    }

    pub(crate) fn process_next<F>(&mut self, mut processor: F) -> Result<Option<MergeQueueEvent>>
    where
        F: FnMut(&MergeRequest) -> Result<MergeQueueOutcome>,
    {
        if self.active.is_some() {
            return Ok(None);
        }

        let Some(request) = self.queue.pop_front() else {
            return Ok(None);
        };

        self.active = Some(request.clone());
        let outcome = match processor(&request) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        self.active = None;
        self.last_result = Some(MergeQueueLastResult {
            task_id: request.task_id,
            outcome: outcome.clone(),
            finished_at: Instant::now(),
        });

        Ok(Some(MergeQueueEvent {
            task_id: request.task_id,
            engineer: request.engineer,
            outcome,
        }))
    }

    fn status_line(&self) -> Option<String> {
        if self.queue.is_empty() && self.active.is_none() && self.last_result.is_none() {
            return None;
        }

        let queued = self.queue.len();
        let merging = self
            .active
            .as_ref()
            .map(|request| format!("#{} ({})", request.task_id, request.branch))
            .unwrap_or_else(|| "idle".to_string());
        let last = self
            .last_result
            .as_ref()
            .map(|result| {
                format!(
                    "#{} {} {}s ago",
                    result.task_id,
                    match result.outcome {
                        MergeQueueOutcome::Success => "merged",
                        MergeQueueOutcome::Conflict => "conflicted",
                        MergeQueueOutcome::Reverted => "reverted",
                        MergeQueueOutcome::Failed => "failed",
                    },
                    result.finished_at.elapsed().as_secs()
                )
            })
            .unwrap_or_else(|| "none".to_string());

        Some(format!(
            "[merge] queued: {queued} | merging: {merging} | last: {last}"
        ))
    }

    pub(crate) fn take_status_update(&mut self) -> Option<String> {
        let status = self.status_line()?;
        if self.last_reported_status.as_deref() == Some(status.as_str()) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}

impl TeamDaemon {
    pub(super) fn process_merge_queue(&mut self) -> Result<()> {
        let mut merge_queue = std::mem::take(&mut self.merge_queue);
        let _ = merge_queue.process_next(|request| self.execute_queued_merge(request))?;
        if let Some(status) = merge_queue.take_status_update() {
            self.record_orchestrator_action(status);
        }
        self.merge_queue = merge_queue;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn enqueue_merge_request(&mut self, request: MergeRequest) {
        self.merge_queue.enqueue(request);
    }

    fn execute_queued_merge(&mut self, request: &MergeRequest) -> Result<MergeQueueOutcome> {
        if self.is_multi_repo {
            bail!("merge queue execution is not yet implemented for multi-repo projects");
        }

        let _lock = MergeLock::acquire(self.project_root()).context("failed to acquire merge lock")?;
        let board_dir = self.board_dir();
        let board_dir_str = board_dir.to_string_lossy().to_string();
        let manager_name = self.manager_name(&request.engineer);
        let task_title = read_task_title(&board_dir, request.task_id);
        let pre_merge_head = git_head(self.project_root())?;

        match merge_engineer_branch(self.project_root(), &request.engineer)? {
            MergeOutcome::Success => {
                if self.config.team_config.workflow_policy.auto_merge.post_merge_verify {
                    let verification_policy = &self.config.team_config.workflow_policy.verification;
                    let test_command = verification_policy.test_command.as_deref().or(
                        self.config
                            .team_config
                            .workflow_policy
                            .test_command
                            .as_deref(),
                    );
                    let verification = run_automatic_verification(self.project_root(), test_command)
                        .context("post-merge verification on main failed to execute")?;
                    if !verification.passed {
                        reset_main_to(self.project_root(), &pre_merge_head)?;
                        let engineer_notice = format!(
                            "Your task for #{} merged cleanly but failed post-merge verification on main, so Batty reverted it.\nLatest output:\n{}",
                            request.task_id, verification.output
                        );
                        self.queue_message("daemon", &request.engineer, &engineer_notice)?;
                        self.mark_member_working(&request.engineer);
                        if let Some(ref manager_name) = manager_name {
                            let manager_notice = format!(
                                "[{}] Task #{} failed post-merge verification on main and was reverted.\nTitle: {}\nLatest output:\n{}",
                                request.engineer, request.task_id, task_title, verification.output
                            );
                            self.queue_message("daemon", manager_name, &manager_notice)?;
                            self.mark_member_working(manager_name);
                        }
                        return Ok(MergeQueueOutcome::Reverted);
                    }
                }

                let board_update_ok =
                    move_task_to_done(self, &board_dir, &board_dir_str, request, manager_name.as_deref());

                if let Some(ref manager_name) = manager_name {
                    let msg = format!(
                        "[{}] Task #{} completed from merge queue.\nTitle: {}\nTests: passed\nMerge: success{}",
                        request.engineer,
                        request.task_id,
                        task_title,
                        if board_update_ok {
                            ""
                        } else {
                            "\nBoard: update failed; decide next board action manually."
                        }
                    );
                    self.queue_message(&request.engineer, manager_name, &msg)?;
                    self.mark_member_working(manager_name);
                    let rollup = format!(
                        "Rollup: Task #{} completed by {} from the merge queue. Tests passed, merged to main.{}",
                        request.task_id,
                        request.engineer,
                        if board_update_ok {
                            ""
                        } else {
                            " Board automation failed; decide manually."
                        }
                    );
                    self.notify_reports_to(manager_name, &rollup)?;
                }

                self.clear_active_task(&request.engineer);
                self.record_task_completed(&request.engineer, Some(request.task_id));
                self.set_member_idle(&request.engineer);
                info!(
                    engineer = request.engineer,
                    task_id = request.task_id,
                    "merge queue processed request successfully"
                );
                Ok(MergeQueueOutcome::Success)
            }
            MergeOutcome::RebaseConflict(conflict_info) => {
                let attempt = self.increment_retry(&request.engineer);
                if attempt <= 2 {
                    let msg = format!(
                        "Merge conflict during rebase onto main (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
                    );
                    self.queue_message("batty", &request.engineer, &msg)?;
                    self.mark_member_working(&request.engineer);
                } else {
                    if let Some(ref manager_name) = manager_name {
                        let msg = format!(
                            "[{}] task #{} has unresolvable merge conflicts after 2 retries. Escalating.\n{}",
                            request.engineer, request.task_id, conflict_info
                        );
                        self.queue_message(&request.engineer, manager_name, &msg)?;
                        self.mark_member_working(manager_name);
                        let escalation = format!(
                            "ESCALATION: Task #{} assigned to {} has unresolvable merge conflicts. Task blocked on board.",
                            request.task_id, request.engineer
                        );
                        self.notify_reports_to(manager_name, &escalation)?;
                    }

                    self.record_task_escalated(
                        &request.engineer,
                        request.task_id.to_string(),
                        Some("merge_conflict"),
                    );
                    self.run_kanban_md_nonfatal(
                        &[
                            "edit",
                            &request.task_id.to_string(),
                            "--block",
                            "merge conflicts after 2 retries",
                            "--dir",
                            &board_dir_str,
                        ],
                        &format!("block task #{} after merge conflict retries", request.task_id),
                        manager_name
                            .as_deref()
                            .into_iter()
                            .chain(std::iter::once(request.engineer.as_str())),
                    );
                    self.clear_active_task(&request.engineer);
                    self.set_member_idle(&request.engineer);
                }

                warn!(
                    engineer = request.engineer,
                    task_id = request.task_id,
                    "merge queue encountered a rebase conflict"
                );
                Ok(MergeQueueOutcome::Conflict)
            }
            MergeOutcome::MergeFailure(merge_info) => {
                let manager_notice = format!(
                    "Task #{} from {} passed tests but could not be merged to main.\n{}\nDecide whether to clean the main worktree, retry the merge, or redirect the engineer.",
                    request.task_id, request.engineer, merge_info
                );
                if let Some(ref manager_name) = manager_name {
                    self.queue_message("daemon", manager_name, &manager_notice)?;
                    self.mark_member_working(manager_name);
                    self.notify_reports_to(manager_name, &manager_notice)?;
                }

                let engineer_notice = format!(
                    "Your task passed tests, but Batty could not merge it into main.\n{}\nWait for lead direction before making more changes.",
                    merge_info
                );
                self.queue_message("daemon", &request.engineer, &engineer_notice)?;

                self.record_task_escalated(
                    &request.engineer,
                    request.task_id.to_string(),
                    Some("merge_failure"),
                );
                self.clear_active_task(&request.engineer);
                self.set_member_idle(&request.engineer);
                warn!(
                    engineer = request.engineer,
                    task_id = request.task_id,
                    error = %merge_info,
                    "merge queue failed to merge request"
                );
                Ok(MergeQueueOutcome::Failed)
            }
        }
    }
}

fn git_head(repo_dir: &std::path::Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to capture pre-merge HEAD for post-merge verification in {}",
                repo_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to capture pre-merge HEAD in {}: {}",
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn reset_main_to(repo_dir: &std::path::Path, target: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["reset", "--hard", target])
        .current_dir(repo_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to run git reset --hard {target} in {} after post-merge verification failure",
                repo_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to reset {} back to {}: {}",
            repo_dir.display(),
            target,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn move_task_to_done(
    daemon: &mut TeamDaemon,
    board_dir: &std::path::Path,
    board_dir_str: &str,
    request: &MergeRequest,
    manager_name: Option<&str>,
) -> bool {
    if crate::team::task_cmd::transition_task(board_dir, request.task_id, "done").is_ok() {
        return true;
    }

    if crate::team::task_cmd::transition_task(board_dir, request.task_id, "review").is_ok()
        && crate::team::task_cmd::cmd_review(board_dir, request.task_id, "approved", None).is_ok()
    {
        return true;
    }

    daemon.run_kanban_md_nonfatal(
        &[
            "move",
            &request.task_id.to_string(),
            "done",
            "--claim",
            &request.engineer,
            "--dir",
            board_dir_str,
        ],
        &format!("move task #{} to done", request.task_id),
        manager_name
            .into_iter()
            .chain(std::iter::once(request.engineer.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{engineer_member, git_ok, init_git_repo, manager_member};
    use std::path::Path;

    fn request(task_id: u32) -> MergeRequest {
        MergeRequest {
            task_id,
            engineer: "eng-1".to_string(),
            branch: format!("eng-1/task-{task_id}"),
            worktree_dir: PathBuf::from("/tmp/worktree"),
            queued_at: Instant::now(),
            test_passed: true,
        }
    }

    #[test]
    fn process_next_runs_requests_in_fifo_order() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));
        queue.enqueue(request(42));

        let first = queue
            .process_next(|request| {
                assert_eq!(request.task_id, 41);
                Ok(MergeQueueOutcome::Success)
            })
            .unwrap()
            .unwrap();
        let second = queue
            .process_next(|request| {
                assert_eq!(request.task_id, 42);
                Ok(MergeQueueOutcome::Conflict)
            })
            .unwrap()
            .unwrap();

        assert_eq!(first.task_id, 41);
        assert_eq!(second.task_id, 42);
        assert_eq!(queue.queued_len(), 0);
        assert_eq!(queue.active_task_id(), None);
    }

    #[test]
    fn take_status_update_reports_queue_state_changes() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));

        let initial = queue.take_status_update().unwrap();
        assert!(initial.contains("[merge] queued: 1"));
        assert!(queue.take_status_update().is_none());

        queue
            .process_next(|_| Ok(MergeQueueOutcome::Success))
            .unwrap();
        let updated = queue.take_status_update().unwrap();
        assert!(updated.contains("last: #41 merged"));
    }

    #[test]
    fn processor_errors_leave_active_request_cleared() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(99));

        let error = queue
            .process_next(|_| anyhow::bail!("merge execution failed"))
            .unwrap_err();

        assert!(error.to_string().contains("merge execution failed"));
        assert_eq!(queue.active_task_id(), None);
        assert_eq!(queue.queued_len(), 0);
    }

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

    #[test]
    fn daemon_process_merge_queue_merges_and_completes_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-test");
        write_task_file(&repo, 42, "merge-queue-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch: "eng-1/task-42".to_string(),
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
        });

        daemon.process_merge_queue().unwrap();

        assert_eq!(std::fs::read_to_string(repo.join("note.txt")).unwrap(), "queued merge\n");
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-merge-queue-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "done");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.member_state_for_test("eng-1"), Some(MemberState::Idle));
    }

    #[test]
    fn daemon_process_merge_queue_reverts_when_post_merge_verify_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-post-merge-verify-test");
        write_task_file(&repo, 42, "post-merge-verify-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("trigger.txt"), "fail main verify\n").unwrap();
        git_ok(&worktree_dir, &["add", "trigger.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "trigger post-merge verify failure"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.config.team_config.workflow_policy.test_command =
            Some("sh -c 'test ! -f trigger.txt'".to_string());
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch: "eng-1/task-42".to_string(),
            worktree_dir: worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
        });

        daemon.process_merge_queue().unwrap();

        assert!(!repo.join("trigger.txt").exists());
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-post-merge-verify-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "in-progress");
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }
}
