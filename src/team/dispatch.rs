use super::super::events::TeamEvent;
#[cfg(test)]
use super::super::task_loop::{engineer_base_branch_name, setup_engineer_worktree};
use super::super::task_loop::{
    engineer_worktree_ready_for_dispatch, prepare_engineer_assignment_worktree,
};
use super::launcher::{
    canonical_agent_name, new_member_session_id, strip_nudge_section, write_launch_script,
};
use super::task_cmd::{assign_task_owners, transition_task};
use super::*;
use serde::{Deserialize, Serialize};

use super::super::policy::check_wip_limit;

const DISPATCH_QUEUE_FAILURE_LIMIT: u32 = 3;

fn dispatch_priority_rank(priority: &str) -> u32 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchQueueEntry {
    pub engineer: String,
    pub task_id: u32,
    pub task_title: String,
    pub queued_at: u64,
    pub validation_failures: u32,
    pub last_failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssignmentLaunch {
    pub(crate) branch: Option<String>,
    pub(crate) work_dir: PathBuf,
}

impl TeamDaemon {
    pub(super) fn launch_task_assignment(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
        reset_context: bool,
        emit_task_assigned: bool,
    ) -> Result<AssignmentLaunch> {
        info!(engineer, task, "assigning task");

        let Some(pane_id) = self.config.pane_map.get(engineer).cloned() else {
            bail!("no pane found for engineer '{engineer}'");
        };

        let member = self
            .config
            .members
            .iter()
            .find(|m| m.name == engineer)
            .cloned();
        let agent_name = member
            .as_ref()
            .and_then(|m| m.agent.as_deref())
            .unwrap_or("claude");

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let use_worktrees = member.as_ref().map(|m| m.use_worktrees).unwrap_or(false);
        let task_branch = use_worktrees.then(|| engineer_task_branch_name(engineer, task, task_id));
        let work_dir = if let Some(task_branch) = task_branch.as_deref() {
            let work_dir = self
                .config
                .project_root
                .join(".batty")
                .join("worktrees")
                .join(engineer);
            prepare_engineer_assignment_worktree(
                &self.config.project_root,
                &work_dir,
                engineer,
                task_branch,
                &team_config_dir,
            )?
        } else {
            self.config.project_root.clone()
        };

        if reset_context {
            let adapter = agent::adapter_from_name(agent_name);
            if let Some(adapter) = &adapter {
                for (keys, enter) in adapter.reset_context_keys() {
                    tmux::send_keys(&pane_id, &keys, enter)?;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }

        self.ensure_member_pane_cwd(engineer, &pane_id, &work_dir)?;

        let role_context = member
            .as_ref()
            .map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));
        let normalized_agent = canonical_agent_name(agent_name);
        let session_id = new_member_session_id(&normalized_agent);

        std::thread::sleep(Duration::from_secs(1));
        let short_cmd = write_launch_script(
            engineer,
            agent_name,
            task,
            role_context.as_deref(),
            &work_dir,
            &self.config.project_root,
            false,
            false,
            session_id.as_deref(),
        )?;
        if let Some(watcher) = self.watchers.get_mut(engineer) {
            watcher.set_session_id(session_id.clone());
        }
        tmux::send_keys(&pane_id, &short_cmd, true)?;
        if let Some(session_id) = session_id.as_deref() {
            self.persist_member_session_id(engineer, session_id)?;
        }

        self.mark_member_working(engineer);

        if emit_task_assigned {
            self.emit_event(TeamEvent::task_assigned(engineer, task));
        }

        Ok(AssignmentLaunch {
            branch: task_branch,
            work_dir,
        })
    }

    pub(super) fn ensure_member_pane_cwd(
        &mut self,
        member_name: &str,
        pane_id: &str,
        expected_dir: &Path,
    ) -> Result<()> {
        let current_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
        let normalized_expected = normalized_assignment_dir(expected_dir);
        if normalized_assignment_dir(&current_path) == normalized_expected {
            return Ok(());
        }

        // Codex agents run from {worktree}/.batty/codex-context/{member_name} by
        // design.  Accept that path as a valid CWD so we don't fail assignments
        // when the agent is already running in the correct codex context directory.
        let codex_context_dir = expected_dir
            .join(".batty")
            .join("codex-context")
            .join(member_name);
        if normalized_assignment_dir(&current_path) == normalized_assignment_dir(&codex_context_dir)
        {
            return Ok(());
        }

        warn!(
            member = %member_name,
            pane = %pane_id,
            current = %current_path.display(),
            expected = %expected_dir.display(),
            "correcting pane cwd before agent interaction"
        );

        let command = format!(
            "cd '{}'",
            shell_single_quote(expected_dir.to_string_lossy().as_ref())
        );
        tmux::send_keys(pane_id, &command, true)?;
        std::thread::sleep(Duration::from_millis(200));

        let corrected_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
        let normalized_corrected = normalized_assignment_dir(&corrected_path);
        if normalized_corrected != normalized_expected
            && normalized_corrected != normalized_assignment_dir(&codex_context_dir)
        {
            bail!(
                "failed to correct pane cwd for '{member_name}': expected {}, got {}",
                expected_dir.display(),
                corrected_path.display()
            );
        }

        self.emit_event(TeamEvent::cwd_corrected(
            member_name,
            &expected_dir.display().to_string(),
        ));
        Ok(())
    }

    pub(crate) fn run_kanban_md_nonfatal<'a, I>(
        &mut self,
        args: &[&str],
        action: &str,
        recipients: I,
    ) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        match std::process::Command::new("kanban-md").args(args).output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                let detail = describe_command_failure("kanban-md", args, &output);
                self.report_nonfatal_kanban_failure(action, &detail, recipients);
                false
            }
            Err(error) => {
                let detail = format!("failed to execute `kanban-md {}`: {error}", args.join(" "));
                self.report_nonfatal_kanban_failure(action, &detail, recipients);
                false
            }
        }
    }

    pub(super) fn report_nonfatal_kanban_failure<'a, I>(
        &mut self,
        action: &str,
        detail: &str,
        recipients: I,
    ) where
        I: IntoIterator<Item = &'a str>,
    {
        warn!(
            action,
            error = detail,
            "kanban-md command failed; continuing"
        );

        let body = format!(
            "Board automation failed while trying to {action}.\n{detail}\nDecide the next board action manually."
        );
        let mut notified = HashSet::new();
        for recipient in recipients {
            if !notified.insert(recipient.to_string()) {
                continue;
            }
            if let Err(error) = self.queue_daemon_message(recipient, &body) {
                warn!(to = recipient, error = %error, "failed to relay kanban-md failure");
            }
        }
    }

    pub(crate) fn notify_assignment_sender_success(
        &mut self,
        sender: &str,
        engineer: &str,
        msg_id: &str,
        task: &str,
        launch: &AssignmentLaunch,
    ) {
        let mut body = format!(
            "Assignment delivered.\nEngineer: {engineer}\nMessage ID: {msg_id}\nTask: {}",
            summarize_assignment(task)
        );
        if let Some(branch) = launch.branch.as_deref() {
            body.push_str(&format!("\nBranch: {branch}"));
        }
        body.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));

        if let Err(error) = self.queue_daemon_message(sender, &body) {
            warn!(to = sender, error = %error, "failed to notify assignment sender");
        }
    }

    pub(crate) fn record_assignment_success(
        &self,
        engineer: &str,
        msg_id: &str,
        task: &str,
        launch: &AssignmentLaunch,
    ) {
        let result = AssignmentDeliveryResult {
            message_id: msg_id.to_string(),
            status: AssignmentResultStatus::Delivered,
            engineer: engineer.to_string(),
            task_summary: summarize_assignment(task),
            branch: launch.branch.clone(),
            work_dir: Some(launch.work_dir.display().to_string()),
            detail: "assignment launched".to_string(),
            ts: now_unix(),
        };
        if let Err(error) = store_assignment_result(&self.config.project_root, &result) {
            warn!(id = msg_id, error = %error, "failed to record assignment success");
        }
    }

    pub(crate) fn notify_assignment_sender_failure(
        &mut self,
        sender: &str,
        engineer: &str,
        msg_id: &str,
        task: &str,
        error: &anyhow::Error,
    ) {
        let body = format!(
            "Assignment failed.\nEngineer: {engineer}\nMessage ID: {msg_id}\nTask: {}\nReason: {error}",
            summarize_assignment(task)
        );

        if let Err(notify_error) = self.queue_daemon_message(sender, &body) {
            warn!(
                to = sender,
                error = %notify_error,
                "failed to notify assignment sender of failure"
            );
        }
    }

    pub(crate) fn record_assignment_failure(
        &self,
        engineer: &str,
        msg_id: &str,
        task: &str,
        error: &anyhow::Error,
    ) {
        let work_dir = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer);
        let result = AssignmentDeliveryResult {
            message_id: msg_id.to_string(),
            status: AssignmentResultStatus::Failed,
            engineer: engineer.to_string(),
            task_summary: summarize_assignment(task),
            branch: None,
            work_dir: Some(work_dir.display().to_string()),
            detail: error.to_string(),
            ts: now_unix(),
        };
        if let Err(store_error) = store_assignment_result(&self.config.project_root, &result) {
            warn!(id = msg_id, error = %store_error, "failed to record assignment failure");
        }
    }

    pub(crate) fn assign_task(&mut self, engineer: &str, task: &str) -> Result<AssignmentLaunch> {
        self.assign_task_with_task_id(engineer, task, None)
    }

    pub(super) fn assign_task_with_task_id(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
    ) -> Result<AssignmentLaunch> {
        self.launch_task_assignment(engineer, task, task_id, true, true)
    }

    pub(super) fn idle_engineer_names(&self) -> Vec<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .filter(|member| self.states.get(&member.name) == Some(&MemberState::Idle))
            .map(|member| member.name.clone())
            .collect()
    }

    fn next_dispatch_task(
        &self,
        board_dir: &Path,
        queued_task_ids: &HashSet<u32>,
    ) -> Result<Option<crate::task::Task>> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let task_status_by_id: HashMap<u32, String> = tasks
            .iter()
            .map(|task| (task.id, task.status.clone()))
            .collect();

        let mut available: Vec<crate::task::Task> = tasks
            .into_iter()
            .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
            .filter(|task| task.claimed_by.is_none())
            .filter(|task| task.blocked.is_none())
            .filter(|task| task.blocked_on.is_none())
            .filter(|task| !queued_task_ids.contains(&task.id))
            .filter(|task| {
                task.depends_on.iter().all(|dep_id| {
                    task_status_by_id
                        .get(dep_id)
                        .is_none_or(|status| status == "done")
                })
            })
            .collect();

        available.sort_by_key(|task| (dispatch_priority_rank(&task.priority), task.id));
        Ok(available.into_iter().next())
    }

    fn enqueue_dispatch_candidates(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let mut queued_task_ids: HashSet<u32> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.task_id)
            .collect();
        let queued_engineers: HashSet<String> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.engineer.clone())
            .collect();

        let mut engineers = self.idle_engineer_names();
        engineers.sort();
        for engineer_name in engineers {
            if queued_engineers.contains(&engineer_name) {
                continue;
            }
            let Some(task) = self.next_dispatch_task(&board_dir, &queued_task_ids)? else {
                break;
            };
            queued_task_ids.insert(task.id);
            self.dispatch_queue.push(DispatchQueueEntry {
                engineer: engineer_name,
                task_id: task.id,
                task_title: task.title,
                queued_at: now_unix(),
                validation_failures: 0,
                last_failure: None,
            });
        }
        Ok(())
    }

    fn engineer_active_board_item_count(&self, board_dir: &Path, engineer: &str) -> Result<u32> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        Ok(tasks
            .into_iter()
            .filter(|task| {
                (matches!(task.status.as_str(), "todo" | "in-progress")
                    && task.claimed_by.as_deref() == Some(engineer))
                    || (task.status == "review" && task.review_owner.as_deref() == Some(engineer))
            })
            .count() as u32)
    }

    fn task_for_dispatch_entry(
        &self,
        board_dir: &Path,
        entry: &DispatchQueueEntry,
    ) -> Result<Option<crate::task::Task>> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let task_status_by_id: HashMap<u32, String> = tasks
            .iter()
            .map(|task| (task.id, task.status.clone()))
            .collect();
        Ok(tasks.into_iter().find(|task| {
            task.id == entry.task_id
                && matches!(task.status.as_str(), "backlog" | "todo")
                && task.claimed_by.is_none()
                && task.blocked.is_none()
                && task.blocked_on.is_none()
                && task.depends_on.iter().all(|dep_id| {
                    task_status_by_id
                        .get(dep_id)
                        .is_none_or(|status| status == "done")
                })
        }))
    }

    fn should_hold_dispatch_for_stabilization(&self, engineer: &str) -> bool {
        let idle_since = self.idle_started_at.get(engineer);
        let delay = Duration::from_secs(
            self.config
                .team_config
                .board
                .dispatch_stabilization_delay_secs,
        );
        idle_since.is_none_or(|started| started.elapsed() < delay)
    }

    fn dispatch_failure_recipient(&self, engineer: &str) -> Option<String> {
        self.manager_name(engineer).or_else(|| {
            self.config
                .members
                .iter()
                .find(|member| member.role_type == RoleType::Manager)
                .map(|member| member.name.clone())
        })
    }

    fn escalate_dispatch_queue_entry(
        &mut self,
        entry: &DispatchQueueEntry,
        detail: &str,
    ) -> Result<()> {
        let Some(manager) = self.dispatch_failure_recipient(&entry.engineer) else {
            warn!(
                engineer = %entry.engineer,
                task_id = entry.task_id,
                detail,
                "dispatch queue entry exhausted retries without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Dispatch queue entry failed validation too many times.\nEngineer: {}\nTask #{}: {}\nFailures: {}\nLast failure: {}",
            entry.engineer, entry.task_id, entry.task_title, entry.validation_failures, detail
        );
        self.queue_daemon_message(&manager, &body)?;
        Ok(())
    }

    fn process_dispatch_queue(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let mut pending: Vec<DispatchQueueEntry> = std::mem::take(&mut self.dispatch_queue);
        let mut retained = Vec::new();

        for mut entry in pending.drain(..) {
            if self.states.get(&entry.engineer) != Some(&MemberState::Idle) {
                retained.push(entry);
                continue;
            }
            if self.should_hold_dispatch_for_stabilization(&entry.engineer) {
                retained.push(entry);
                continue;
            }

            let Some(task) = self.task_for_dispatch_entry(&board_dir, &entry)? else {
                continue;
            };

            let active_count = self.engineer_active_board_item_count(&board_dir, &entry.engineer)?;
            if active_count > 0 {
                entry.validation_failures += 1;
                entry.last_failure = Some(format!(
                    "Dispatch guard blocked assignment for '{}' with {} active board item(s)",
                    entry.engineer, active_count
                ));
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    self.escalate_dispatch_queue_entry(
                        &entry,
                        entry
                            .last_failure
                            .as_deref()
                            .unwrap_or("dispatch guard blocked assignment"),
                    )?;
                } else {
                    retained.push(entry);
                }
                continue;
            }

            if !check_wip_limit(
                &self.config.team_config.workflow_policy,
                RoleType::Engineer,
                active_count,
            ) {
                entry.validation_failures += 1;
                entry.last_failure = Some(format!(
                    "WIP gate blocked dispatch for '{}' with {} active board task(s)",
                    entry.engineer, active_count
                ));
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    self.escalate_dispatch_queue_entry(
                        &entry,
                        entry
                            .last_failure
                            .as_deref()
                            .unwrap_or("wip gate blocked dispatch"),
                    )?;
                } else {
                    retained.push(entry);
                }
                continue;
            }

            let member_uses_worktrees = self.member_uses_worktrees(&entry.engineer);
            if member_uses_worktrees {
                let worktree_dir = self.worktree_dir(&entry.engineer);
                if let Err(error) = engineer_worktree_ready_for_dispatch(
                    &self.config.project_root,
                    &worktree_dir,
                    &entry.engineer,
                ) {
                    entry.validation_failures += 1;
                    entry.last_failure = Some(error.to_string());
                    if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                        self.escalate_dispatch_queue_entry(
                            &entry,
                            entry
                                .last_failure
                                .as_deref()
                                .unwrap_or("worktree readiness validation failed"),
                        )?;
                    } else {
                        retained.push(entry);
                    }
                    continue;
                }
            }

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            match self.assign_task_with_task_id(&entry.engineer, &assignment_message, Some(task.id))
            {
                Ok(_) => {
                    assign_task_owners(&board_dir, task.id, Some(&entry.engineer), None)?;
                    transition_task(&board_dir, task.id, "in-progress")?;
                    self.active_tasks.insert(entry.engineer.clone(), task.id);
                    self.retry_counts.remove(&entry.engineer);
                    self.record_orchestrator_action(format!(
                        "dispatch queue: selected runnable task #{} ({}) and dispatched it to {}",
                        task.id, task.title, entry.engineer
                    ));
                    info!(
                        engineer = %entry.engineer,
                        task_id = task.id,
                        task_title = %task.title,
                        "queued task dispatched"
                    );
                }
                Err(error) => {
                    entry.validation_failures += 1;
                    entry.last_failure = Some(error.to_string());
                    if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                        self.escalate_dispatch_queue_entry(
                            &entry,
                            entry
                                .last_failure
                                .as_deref()
                                .unwrap_or("assignment launch failed"),
                        )?;
                    } else {
                        retained.push(entry);
                    }
                }
            }
        }

        self.dispatch_queue = retained;
        Ok(())
    }

    pub(super) fn maybe_auto_dispatch(&mut self) -> Result<()> {
        if !self.config.team_config.board.auto_dispatch {
            return Ok(());
        }

        if self.last_auto_dispatch.elapsed() < Duration::from_secs(10) {
            return Ok(());
        }

        if let Err(error) = self.enqueue_dispatch_candidates() {
            warn!(error = %error, "failed to enqueue dispatch candidates");
        }
        if let Err(error) = self.process_dispatch_queue() {
            warn!(error = %error, "auto-dispatch failed");
        }
        self.last_auto_dispatch = Instant::now();
        Ok(())
    }
}

pub(super) fn normalized_assignment_dir(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn summarize_assignment(task: &str) -> String {
    let summary = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("task")
        .trim();
    if summary.len() <= 120 {
        summary.to_string()
    } else {
        format!("{}...", &summary[..117])
    }
}

pub(super) fn engineer_task_branch_name(
    engineer: &str,
    task: &str,
    explicit_task_id: Option<u32>,
) -> String {
    let suffix = explicit_task_id
        .or_else(|| parse_assignment_task_id(task))
        .map(|task_id| task_id.to_string())
        .unwrap_or_else(|| {
            let slug = slugify_task_branch(task);
            let unique = Uuid::new_v4().simple().to_string();
            format!("task-{slug}-{}", &unique[..8])
        });
    format!("{engineer}/{suffix}")
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn parse_assignment_task_id(task: &str) -> Option<u32> {
    let mut candidates = Vec::new();
    let bytes = task.as_bytes();
    for (index, window) in bytes.windows(6).enumerate() {
        if window.eq_ignore_ascii_case(b"task #") {
            let digits_start = index + 6;
            let digits = bytes[digits_start..]
                .iter()
                .copied()
                .take_while(u8::is_ascii_digit)
                .collect::<Vec<_>>();
            if digits.is_empty() {
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&digits) {
                if let Ok(value) = text.parse::<u32>() {
                    candidates.push(value);
                }
            }
        }
    }
    candidates.into_iter().next()
}

fn slugify_task_branch(task: &str) -> String {
    let summary = summarize_assignment(task).to_ascii_lowercase();
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in summary.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{BoardConfig, WorkflowPolicy};
    use crate::team::inbox;
    use crate::team::standup::MemberState;
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, init_git_repo, manager_member, write_open_task_file,
    };
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn engineer_task_branch_name_uses_explicit_task_id() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "freeform task body", Some(123)),
            "eng-1-3/123"
        );
    }

    #[test]
    fn engineer_task_branch_name_extracts_task_id_from_assignment_text() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "Task #456: fix move generation", None),
            "eng-1-3/456"
        );
    }

    #[test]
    fn engineer_task_branch_name_falls_back_to_slugged_branch() {
        let branch = engineer_task_branch_name("eng-1-3", "Fix castling rights sync", None);
        assert!(branch.starts_with("eng-1-3/task-fix-castling-rights-sy"));
    }

    #[test]
    fn summarize_assignment_uses_first_non_empty_line() {
        assert_eq!(
            summarize_assignment("\n\nTask #9: fix move ordering\n\nDetails below"),
            "Task #9: fix move ordering"
        );
    }

    #[test]
    fn stabilization_delay_prevents_premature_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(members)
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 30,
                ..BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon
            .idle_started_at
            .insert("eng-1".to_string(), Instant::now() - Duration::from_secs(5));

        daemon.maybe_auto_dispatch().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 0);
        assert_eq!(daemon.dispatch_queue[0].task_id, 101);
    }

    #[test]
    fn wip_gate_blocks_double_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");
        crate::team::test_support::write_owned_task_file(
            tmp.path(),
            91,
            "active-task",
            "in-progress",
            "eng-1",
        );
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(members)
            .workflow_policy(WorkflowPolicy {
                wip_limit_per_engineer: Some(1),
                ..WorkflowPolicy::default()
            })
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 0,
                ..BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        daemon.maybe_auto_dispatch().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 1);
        assert!(
            daemon.dispatch_queue[0]
                .last_failure
                .as_deref()
                .unwrap_or_default()
                .contains("Dispatch guard")
        );
    }

    #[test]
    fn dispatch_guard_blocks_claimed_todo_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");
        crate::team::test_support::write_owned_task_file(tmp.path(), 91, "claimed-todo", "todo", "eng-1");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(members)
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 0,
                ..BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        daemon.maybe_auto_dispatch().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 1);
        assert!(
            daemon.dispatch_queue[0]
                .last_failure
                .as_deref()
                .unwrap_or_default()
                .contains("Dispatch guard blocked assignment")
        );
    }

    #[test]
    fn active_board_item_count_includes_todo_in_progress_and_review() {
        let tmp = tempfile::tempdir().unwrap();
        crate::team::test_support::write_owned_task_file(tmp.path(), 11, "todo-task", "todo", "eng-1");
        crate::team::test_support::write_owned_task_file(
            tmp.path(),
            12,
            "working-task",
            "in-progress",
            "eng-1",
        );
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("013-review-task.md"),
            "---\nid: 13\ntitle: review-task\nstatus: review\npriority: critical\nclaimed_by: manager\nreview_owner: eng-1\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            3
        );
    }

    #[test]
    fn worktree_gate_blocks_dirty_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "dispatch-queue");
        write_open_task_file(&repo, 101, "queued-task", "todo");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &engineer_base_branch_name("eng-1"),
            &team_config_dir,
        )
        .unwrap();
        std::fs::write(worktree_dir.join("DIRTY.txt"), "dirty\n").unwrap();
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 0,
                ..BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        daemon.maybe_auto_dispatch().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 1);
        assert!(
            daemon.dispatch_queue[0]
                .last_failure
                .as_deref()
                .unwrap_or_default()
                .contains("uncommitted changes")
        );
    }

    #[test]
    fn queue_escalates_after_repeated_validation_failures() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");
        crate::team::test_support::write_owned_task_file(
            tmp.path(),
            91,
            "active-task",
            "in-progress",
            "eng-1",
        );
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(members)
            .workflow_policy(WorkflowPolicy {
                wip_limit_per_engineer: Some(1),
                ..WorkflowPolicy::default()
            })
            .board(BoardConfig {
                auto_dispatch: true,
                dispatch_stabilization_delay_secs: 0,
                ..BoardConfig::default()
            })
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("manager".to_string(), MemberState::Idle),
            ]))
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        for _ in 0..DISPATCH_QUEUE_FAILURE_LIMIT {
            daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
            daemon.maybe_auto_dispatch().unwrap();
        }

        assert!(daemon.dispatch_queue.is_empty());
        let inbox_root = inbox::inboxes_root(tmp.path());
        let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert!(
            manager_messages[0]
                .body
                .contains("Dispatch queue entry failed validation")
        );
    }
}
