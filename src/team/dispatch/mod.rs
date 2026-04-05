//! Auto-dispatch: task assignment, queue orchestration, and delivery tracking.
//!
//! This module decomposes the dispatch system into focused submodules:
//! - `queue` — dispatch queue population, processing, and task selection
//! - `wip` — WIP limit enforcement and active task counting
//! - `stabilization` — post-merge cooldown before re-dispatch
//! - `readiness` — pane CWD correction and workspace readiness
//! - `guard` — escalation and failure routing

mod guard;
mod queue;
mod readiness;
mod stabilization;
mod wip;

#[cfg(test)]
mod tests;

use super::super::events::TeamEvent;
use super::super::task_loop::prepare_engineer_assignment_worktree;
use super::super::task_loop::prepare_multi_repo_assignment_worktree;
use super::helpers::describe_command_failure;
use super::launcher::{
    agent_supports_sdk_mode, canonical_agent_name, new_member_session_id, strip_nudge_section,
    write_launch_script,
};
use super::*;
use crate::team::append_shim_event_log;
use serde::{Deserialize, Serialize};
use tracing::debug;

#[cfg(test)]
pub(super) use self::readiness::normalized_assignment_dir;

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
    pub(in crate::team) fn assignment_sender(&self, engineer: &str) -> String {
        self.config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .and_then(|member| member.reports_to.clone())
            .unwrap_or_else(|| "human".to_string())
    }

    fn shim_assignment_preview(task: &str) -> String {
        let single_line = task.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut preview = single_line.chars().take(160).collect::<String>();
        if single_line.chars().count() > 160 {
            preview.push_str("...");
        }
        preview
    }

    fn prepare_assignment_launch(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
    ) -> Result<AssignmentLaunch> {
        let member = self
            .config
            .members
            .iter()
            .find(|m| m.name == engineer)
            .cloned();
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let use_worktrees = (self.is_git_repo || self.is_multi_repo)
            && member.as_ref().map(|m| m.use_worktrees).unwrap_or(false);
        if !use_worktrees {
            debug!(
                member = %engineer,
                "Skipping worktree setup for {engineer}: use_worktrees=false"
            );
        }
        let task_branch = use_worktrees.then(|| engineer_task_branch_name(engineer, task, task_id));
        let work_dir = if let Some(task_branch) = task_branch.as_deref() {
            let work_dir = self
                .config
                .project_root
                .join(".batty")
                .join("worktrees")
                .join(engineer);
            if self.is_multi_repo {
                prepare_multi_repo_assignment_worktree(
                    &self.config.project_root,
                    &work_dir,
                    engineer,
                    task_branch,
                    &team_config_dir,
                    &self.sub_repo_names,
                )?
            } else {
                prepare_engineer_assignment_worktree(
                    &self.config.project_root,
                    &work_dir,
                    engineer,
                    task_branch,
                    &team_config_dir,
                )?
            }
        } else {
            self.config.project_root.clone()
        };

        self.validate_member_work_dir(engineer, &work_dir)?;

        Ok(AssignmentLaunch {
            branch: task_branch,
            work_dir,
        })
    }

    fn deliver_shim_assignment(
        &mut self,
        sender: &str,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
        emit_task_assigned: bool,
    ) -> Result<AssignmentLaunch> {
        let launch = self.prepare_assignment_launch(engineer, task, task_id)?;
        if let Some(handle) = self.shim_handles.get_mut(engineer) {
            if handle.is_ready() {
                handle.send_message(sender, task)?;
                handle.apply_state_change(crate::shim::protocol::ShimState::Working);
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    engineer,
                    &format!("-> {sender}: {}", Self::shim_assignment_preview(task)),
                );
            } else if !handle.is_terminal() {
                self.pending_delivery_queue
                    .entry(engineer.to_string())
                    .or_default()
                    .push(PendingMessage {
                        from: sender.to_string(),
                        body: task.to_string(),
                        queued_at: Instant::now(),
                    });
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    engineer,
                    &format!(
                        ".. pending {sender}: {}",
                        Self::shim_assignment_preview(task)
                    ),
                );
            } else {
                bail!("shim for '{engineer}' is not available");
            }
        } else {
            bail!("no shim handle found for engineer '{engineer}'");
        }

        self.mark_member_working(engineer);
        if emit_task_assigned {
            self.emit_event(TeamEvent::task_assigned(engineer, task));
        }
        Ok(launch)
    }

    pub(super) fn launch_task_assignment(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
        emit_task_assigned: bool,
    ) -> Result<AssignmentLaunch> {
        let sender = self.assignment_sender(engineer);
        self.launch_task_assignment_as(&sender, engineer, task, task_id, emit_task_assigned)
    }

    pub(super) fn launch_task_assignment_as(
        &mut self,
        sender: &str,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
        emit_task_assigned: bool,
    ) -> Result<AssignmentLaunch> {
        info!(engineer, task, "assigning task");

        if self.config.team_config.use_shim && self.shim_handles.contains_key(engineer) {
            return self.deliver_shim_assignment(
                sender,
                engineer,
                task,
                task_id,
                emit_task_assigned,
            );
        }

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
        let worktree_launch = self.prepare_assignment_launch(engineer, task, task_id)?;
        let work_dir = worktree_launch.work_dir.clone();
        let task_branch = worktree_launch.branch.clone();
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");

        self.ensure_member_pane_cwd(engineer, &pane_id, &work_dir)?;

        let role_context = member
            .as_ref()
            .map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));
        let normalized_agent = canonical_agent_name(agent_name);
        let session_id = new_member_session_id(&normalized_agent);

        std::thread::sleep(Duration::from_secs(1));
        let sdk_mode = agent_supports_sdk_mode(agent_name) && self.config.team_config.use_sdk_mode;
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
            sdk_mode,
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
                let detail = format!(
                    "failed while trying to {action}: could not execute `kanban-md {}`: {error}",
                    args.join(" ")
                );
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

    #[allow(dead_code)]
    pub(crate) fn assign_task(&mut self, engineer: &str, task: &str) -> Result<AssignmentLaunch> {
        self.assign_task_with_task_id(engineer, task, None)
    }

    pub(super) fn assign_task_with_task_id(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
    ) -> Result<AssignmentLaunch> {
        self.launch_task_assignment(engineer, task, task_id, true)
    }

    pub(in crate::team) fn assign_task_with_task_id_as(
        &mut self,
        sender: &str,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
    ) -> Result<AssignmentLaunch> {
        let effective_task_id = task_id.or_else(|| parse_assignment_task_id(task));
        let board_dir = self.board_dir();
        let active_task_ids = self.engineer_active_board_task_ids(&board_dir, engineer)?;
        if !active_task_ids.is_empty()
            && effective_task_id.is_none_or(|task_id| !active_task_ids.contains(&task_id))
        {
            anyhow::bail!(
                "dispatch guard blocked assignment for '{engineer}': already owns active board task(s) {}",
                active_task_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let launch =
            self.launch_task_assignment_as(sender, engineer, task, effective_task_id, true)?;
        if let Some(task_id) = effective_task_id {
            self.active_tasks.insert(engineer.to_string(), task_id);
        }
        Ok(launch)
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

pub(super) fn summarize_assignment(task: &str) -> String {
    let summary = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("task")
        .trim();
    if summary.len() <= 120 {
        summary.to_string()
    } else {
        // Find a char boundary near 117 bytes to avoid panicking on multi-byte UTF-8
        let mut end = 117;
        while end > 0 && !summary.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &summary[..end])
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

pub(crate) fn parse_assignment_task_id(task: &str) -> Option<u32> {
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
