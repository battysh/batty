use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use crate::task::load_tasks_from_dir;
use crate::team::board::{WorkflowMetadata, read_workflow_metadata};
use crate::team::daemon::verification::run_automatic_verification;
use crate::team::merge::{MergeLock, MergeOutcome, merge_engineer_branch};
use crate::team::task_loop::{current_worktree_branch, read_task_title};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeRequest {
    pub task_id: u32,
    pub engineer: String,
    pub branch: String,
    pub worktree_dir: PathBuf,
    pub queued_at: Instant,
    pub test_passed: bool,
    pub should_post_merge_verify: bool,
    pub test_duration_ms: u64,
    pub confidence: f64,
    pub files_changed: usize,
    pub lines_changed: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeQueueOutcome {
    Success,
    Conflict,
    Reverted,
    Skipped,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoMergeSkipReason {
    WrongStatus,
    MissingPacket,
    NoBranch,
}

impl AutoMergeSkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::WrongStatus => "wrong_status",
            Self::MissingPacket => "missing_packet",
            Self::NoBranch => "no_branch",
        }
    }
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
                        MergeQueueOutcome::Skipped => "skipped",
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
        let queued = merge_queue.queued_len();
        if queued > 0 || merge_queue.active_task_id().is_some() {
            debug!(
                queued,
                active_task_id = ?merge_queue.active_task_id(),
                "processing merge queue"
            );
        }
        let event = merge_queue.process_next(|request| self.execute_queued_merge(request))?;
        if let Some(ref event) = event {
            info!(
                task_id = event.task_id,
                engineer = %event.engineer,
                outcome = ?event.outcome,
                "merge queue processed request"
            );
        }
        if let Some(status) = merge_queue.take_status_update() {
            self.record_orchestrator_action(status);
        }
        self.merge_queue = merge_queue;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn enqueue_merge_request(&mut self, request: MergeRequest) {
        info!(
            task_id = request.task_id,
            engineer = %request.engineer,
            "enqueuing merge request"
        );
        self.merge_queue.enqueue(request);
    }

    fn execute_queued_merge(&mut self, request: &MergeRequest) -> Result<MergeQueueOutcome> {
        if self.is_multi_repo {
            bail!("merge queue execution is not yet implemented for multi-repo projects");
        }

        if let Some((reason, detail)) = merge_request_skip_reason(self.project_root(), request)? {
            warn!(
                task_id = request.task_id,
                engineer = request.engineer,
                reason = reason.as_str(),
                detail = %detail,
                "skipping daemon auto-merge request"
            );
            self.record_orchestrator_action(format!(
                "merge queue: skipped auto-merge for task #{} ({reason}: {detail})",
                request.task_id,
                reason = reason.as_str()
            ));
            return Ok(MergeQueueOutcome::Skipped);
        }

        let _lock =
            MergeLock::acquire(self.project_root()).context("failed to acquire merge lock")?;
        let board_dir = self.board_dir();
        let board_dir_str = board_dir.to_string_lossy().to_string();
        let manager_name = self.manager_name(&request.engineer);
        let task_title = read_task_title(&board_dir, request.task_id);
        let pre_merge_head = git_head(self.project_root())?;

        match merge_engineer_branch(self.project_root(), &request.engineer)? {
            MergeOutcome::Success => {
                let mut post_merge_verify_recorded = false;
                if request.should_post_merge_verify
                    && self
                        .config
                        .team_config
                        .workflow_policy
                        .auto_merge
                        .post_merge_verify
                {
                    let verification_policy = &self.config.team_config.workflow_policy.verification;
                    let test_command = verification_policy.test_command.as_deref().or(self
                        .config
                        .team_config
                        .workflow_policy
                        .test_command
                        .as_deref());
                    let verification =
                        run_automatic_verification(self.project_root(), test_command)
                            .context("post-merge verification on main failed to execute")?;
                    if !verification.passed {
                        self.record_auto_merge_post_verify_result(
                            &request.engineer,
                            request.task_id,
                            Some(false),
                            "failed",
                            Some("post-merge verification on main failed"),
                        );
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
                    self.record_auto_merge_post_verify_result(
                        &request.engineer,
                        request.task_id,
                        Some(true),
                        "passed",
                        Some("post-merge verification on main passed"),
                    );
                    post_merge_verify_recorded = true;
                }
                if !post_merge_verify_recorded {
                    self.record_auto_merge_post_verify_result(
                        &request.engineer,
                        request.task_id,
                        None,
                        "skipped",
                        Some("post-merge verification was not requested for this merge"),
                    );
                }

                let board_update_ok = move_task_to_done(
                    self,
                    &board_dir,
                    &board_dir_str,
                    request,
                    manager_name.as_deref(),
                );
                if let Err(error) = crate::team::merge::record_merge_test_timing(
                    self,
                    request.task_id,
                    &request.engineer,
                    &request.branch,
                    request.test_duration_ms,
                ) {
                    warn!(
                        engineer = request.engineer,
                        task_id = request.task_id,
                        error = %error,
                        "failed to record merge test timing"
                    );
                }
                self.record_task_auto_merged(
                    &request.engineer,
                    request.task_id,
                    request.confidence,
                    request.files_changed,
                    request.lines_changed,
                );

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

                // Post-merge disk hygiene: clean build artifacts and prune branch
                let hygiene_config = &self.config.team_config.automation.disk_hygiene;
                let hygiene_report = super::health::disk_hygiene::post_merge_cleanup(
                    self.project_root(),
                    &request.engineer,
                    request.task_id,
                    &request.branch,
                    hygiene_config,
                );
                if hygiene_report.any_action_taken() {
                    let summary = hygiene_report.summary();
                    info!(
                        engineer = request.engineer,
                        task_id = request.task_id,
                        summary = %summary,
                        "post-merge disk hygiene"
                    );
                    self.record_orchestrator_action(format!(
                        "disk-hygiene: post-merge cleanup for {} task #{}: {summary}",
                        request.engineer, request.task_id
                    ));
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
                        &format!(
                            "block task #{} after merge conflict retries",
                            request.task_id
                        ),
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

fn merge_request_skip_reason(
    project_root: &std::path::Path,
    request: &MergeRequest,
) -> Result<Option<(AutoMergeSkipReason, String)>> {
    let task = load_tasks_from_dir(
        &project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )?
    .into_iter()
    .find(|task| task.id == request.task_id);

    let Some(task) = task else {
        return Ok(Some((
            AutoMergeSkipReason::WrongStatus,
            "task is missing from the board".to_string(),
        )));
    };

    if task.status != "review" {
        return Ok(Some((
            AutoMergeSkipReason::WrongStatus,
            format!("task status is '{}' instead of 'review'", task.status),
        )));
    }

    let metadata = read_workflow_metadata(&task.source_path)?;
    if let Some(detail) = missing_completion_packet_detail(project_root, request, &metadata) {
        return Ok(Some((AutoMergeSkipReason::MissingPacket, detail)));
    }

    if let Some(detail) = unavailable_branch_detail(request)? {
        return Ok(Some((AutoMergeSkipReason::NoBranch, detail)));
    }

    Ok(None)
}

fn missing_completion_packet_detail(
    project_root: &std::path::Path,
    request: &MergeRequest,
    metadata: &WorkflowMetadata,
) -> Option<String> {
    let mut missing = Vec::new();

    match metadata
        .branch
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(branch) if branch == request.branch => {}
        Some(branch) => missing.push(format!(
            "branch marker '{}' does not match queued branch '{}'",
            branch, request.branch
        )),
        None => missing.push("branch marker missing".to_string()),
    }

    if metadata
        .commit
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        missing.push("commit marker missing".to_string());
    }

    match metadata
        .worktree_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(path) => {
            let resolved = resolve_project_path(project_root, path);
            if resolved != request.worktree_dir {
                missing.push(format!(
                    "worktree marker '{}' resolves to '{}' instead of '{}'",
                    path,
                    resolved.display(),
                    request.worktree_dir.display()
                ));
            }
        }
        None => missing.push("worktree marker missing".to_string()),
    }

    if metadata.tests_run != Some(true) {
        missing.push("tests_run marker is not true".to_string());
    }
    if metadata.tests_passed != Some(true) {
        missing.push("tests_passed marker is not true".to_string());
    }
    if !metadata.review_blockers.is_empty() {
        missing.push(format!(
            "review blockers present: {}",
            metadata.review_blockers.join(", ")
        ));
    }
    match metadata
        .outcome
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some("verification_retry_required" | "verification_escalated") => {
            missing.push(format!(
                "outcome marker '{}' is not merge-ready",
                metadata.outcome.as_deref().unwrap_or_default()
            ));
        }
        Some(_) => {}
        None => missing.push("outcome marker missing".to_string()),
    }

    (!missing.is_empty()).then(|| missing.join("; "))
}

fn resolve_project_path(project_root: &std::path::Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn unavailable_branch_detail(request: &MergeRequest) -> Result<Option<String>> {
    if !request.worktree_dir.exists() {
        return Ok(Some(format!(
            "worktree '{}' does not exist",
            request.worktree_dir.display()
        )));
    }

    let current_branch = match current_worktree_branch(&request.worktree_dir) {
        Ok(branch) => branch,
        Err(error) => {
            return Ok(Some(format!(
                "failed to read worktree branch at '{}': {error}",
                request.worktree_dir.display()
            )));
        }
    };
    if current_branch != request.branch {
        return Ok(Some(format!(
            "worktree branch is '{}' instead of '{}'",
            current_branch, request.branch
        )));
    }

    let commits_ahead = match commits_ahead_of_main(&request.worktree_dir) {
        Ok(commits) => commits,
        Err(error) => {
            return Ok(Some(format!(
                "failed to count commits ahead of main for '{}': {error}",
                request.branch
            )));
        }
    };
    if commits_ahead == 0 {
        return Ok(Some(format!(
            "branch '{}' has no commits ahead of main",
            request.branch
        )));
    }

    Ok(None)
}

fn commits_ahead_of_main(worktree_dir: &std::path::Path) -> Result<u32> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "main..HEAD"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to count commits ahead of main in {}",
                worktree_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git rev-list --count main..HEAD failed in {}: {}",
            worktree_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .with_context(|| {
            format!(
                "failed to parse git rev-list --count main..HEAD output in {}",
                worktree_dir.display()
            )
        })
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
        persist_completed_profile(daemon, board_dir, request.task_id);
        return true;
    }

    if crate::team::task_cmd::transition_task(board_dir, request.task_id, "review").is_ok()
        && crate::team::task_cmd::cmd_review(board_dir, request.task_id, "approved", None).is_ok()
    {
        persist_completed_profile(daemon, board_dir, request.task_id);
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

fn persist_completed_profile(daemon: &TeamDaemon, board_dir: &std::path::Path, task_id: u32) {
    let Ok(tasks) = crate::task::load_tasks_from_dir(&board_dir.join("tasks")) else {
        return;
    };
    let Some(task) = tasks.into_iter().find(|task| task.id == task_id) else {
        return;
    };
    if let Err(error) =
        crate::team::allocation::persist_completed_task_profile(daemon.project_root(), &task)
    {
        warn!(task_id, error = %error, "failed to persist completed task profile");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::board::write_workflow_metadata;
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
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
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

    fn write_task_file(project_root: &Path, id: u32, title: &str, status: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn write_completion_metadata(
        project_root: &Path,
        id: u32,
        title: &str,
        branch: &str,
        worktree_dir: &Path,
        commit: &str,
    ) {
        let task_path = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join(format!("{id:03}-{title}.md"));
        write_workflow_metadata(
            &task_path,
            &WorkflowMetadata {
                branch: Some(branch.to_string()),
                worktree_path: Some(worktree_dir.to_string_lossy().into_owned()),
                commit: Some(commit.to_string()),
                changed_paths: vec!["note.txt".to_string()],
                tests_run: Some(true),
                tests_passed: Some(true),
                test_results: None,
                artifacts: Vec::new(),
                outcome: Some("verification_passed".to_string()),
                review_blockers: Vec::new(),
            },
        )
        .unwrap();
    }

    fn current_head(repo_dir: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo_dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    #[test]
    fn daemon_process_merge_queue_merges_and_completes_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-test");
        write_task_file(&repo, 42, "merge-queue-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "merge-queue-task",
            &branch,
            &worktree_dir,
            &commit,
        );

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
            branch,
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
        });

        daemon.process_merge_queue().unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "queued merge\n"
        );
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
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
    }

    #[test]
    fn daemon_process_merge_queue_reverts_when_post_merge_verify_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-post-merge-verify-test");
        write_task_file(&repo, 42, "post-merge-verify-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("trigger.txt"), "fail main verify\n").unwrap();
        git_ok(&worktree_dir, &["add", "trigger.txt"]);
        git_ok(
            &worktree_dir,
            &["commit", "-m", "trigger post-merge verify failure"],
        );
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "post-merge-verify-task",
            &branch,
            &worktree_dir,
            &commit,
        );

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
            branch,
            worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
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
        assert_eq!(task.status, "review");
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }

    #[test]
    fn merge_request_skip_reason_requires_review_status() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-status-test");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let commit = current_head(&worktree_dir);

        for status in ["todo", "blocked", "done"] {
            write_task_file(&repo, 42, "merge-queue-status-task", status);
            write_completion_metadata(
                &repo,
                42,
                "merge-queue-status-task",
                "eng-1/task-42",
                &worktree_dir,
                &commit,
            );
            let reason = merge_request_skip_reason(
                &repo,
                &MergeRequest {
                    task_id: 42,
                    engineer: "eng-1".to_string(),
                    branch: "eng-1/task-42".to_string(),
                    worktree_dir: worktree_dir.clone(),
                    queued_at: Instant::now(),
                    test_passed: true,
                    should_post_merge_verify: true,
                    test_duration_ms: 1,
                    confidence: 0.95,
                    files_changed: 1,
                    lines_changed: 1,
                },
            )
            .unwrap();

            assert_eq!(
                reason,
                Some((
                    AutoMergeSkipReason::WrongStatus,
                    format!("task status is '{}' instead of 'review'", status)
                ))
            );
        }
    }

    #[test]
    fn merge_request_skip_reason_requires_completion_packet_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-packet-test");
        write_task_file(&repo, 42, "merge-queue-packet-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);

        let reason = merge_request_skip_reason(
            &repo,
            &MergeRequest {
                task_id: 42,
                engineer: "eng-1".to_string(),
                branch: "eng-1/task-42".to_string(),
                worktree_dir,
                queued_at: Instant::now(),
                test_passed: true,
                should_post_merge_verify: true,
                test_duration_ms: 1,
                confidence: 0.95,
                files_changed: 1,
                lines_changed: 1,
            },
        )
        .unwrap();

        assert!(matches!(
            reason,
            Some((AutoMergeSkipReason::MissingPacket, detail))
                if detail.contains("branch marker missing")
                    && detail.contains("commit marker missing")
                    && detail.contains("worktree marker missing")
        ));
    }

    #[test]
    fn daemon_process_merge_queue_skips_todo_task_even_with_branch_and_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-todo-skip-test");
        write_task_file(&repo, 42, "merge-queue-todo-task", "todo");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "merge-queue-todo-task",
            "eng-1/task-42",
            &worktree_dir,
            &commit,
        );

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
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
        });

        daemon.process_merge_queue().unwrap();

        assert!(!repo.join("note.txt").exists());
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-merge-queue-todo-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "todo");
    }

    #[test]
    fn process_merge_queue_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.process_merge_queue().unwrap();
        assert_eq!(daemon.merge_queue.queued_len(), 0);
    }

    #[test]
    fn process_next_returns_event_with_outcome() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(99));

        let event = queue
            .process_next(|_| Ok(MergeQueueOutcome::Success))
            .unwrap();
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.task_id, 99);
        assert_eq!(event.engineer, "eng-1");
        assert!(matches!(event.outcome, MergeQueueOutcome::Success));
    }

    #[test]
    fn process_next_returns_conflict_outcome() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(88));

        let event = queue
            .process_next(|_| Ok(MergeQueueOutcome::Conflict))
            .unwrap();
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.task_id, 88);
        assert!(matches!(event.outcome, MergeQueueOutcome::Conflict));
    }
}
