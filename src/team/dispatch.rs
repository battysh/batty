use super::super::events::TeamEvent;
use super::super::task_loop::{next_unclaimed_task, prepare_engineer_assignment_worktree};
use super::launcher::{
    canonical_agent_name, new_member_session_id, strip_nudge_section, write_launch_script,
};
use super::*;

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
        if normalized_assignment_dir(&current_path)
            == normalized_assignment_dir(&codex_context_dir)
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

    fn auto_dispatch(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.to_string_lossy().to_string();

        for engineer_name in self.idle_engineer_names() {
            let Some(task) = next_unclaimed_task(&board_dir)? else {
                break;
            };

            let board_failure_recipients: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| {
                    matches!(member.role_type, RoleType::Architect | RoleType::Manager)
                })
                .map(|member| member.name.clone())
                .collect();
            if !self.run_kanban_md_nonfatal(
                &[
                    "pick",
                    "--claim",
                    &engineer_name,
                    "--move",
                    "in-progress",
                    "--dir",
                    &board_dir_str,
                ],
                &format!("pick the next task for {engineer_name}"),
                board_failure_recipients.iter().map(String::as_str),
            ) {
                break;
            }

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            self.assign_task_with_task_id(&engineer_name, &assignment_message, Some(task.id))?;
            self.active_tasks.insert(engineer_name.clone(), task.id);
            self.retry_counts.remove(&engineer_name);
            self.record_orchestrator_action(format!(
                "dependency resolution: selected runnable task #{} ({}) and dispatched it to {}",
                task.id, task.title, engineer_name
            ));
            info!(
                engineer = %engineer_name,
                task_id = task.id,
                task_title = %task.title,
                "auto-dispatched task"
            );
        }

        Ok(())
    }

    pub(super) fn maybe_auto_dispatch(&mut self) -> Result<()> {
        if !self.config.team_config.board.auto_dispatch {
            return Ok(());
        }

        if self.last_auto_dispatch.elapsed() < Duration::from_secs(10) {
            return Ok(());
        }

        if let Err(error) = self.auto_dispatch() {
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
        .map(|task_id| format!("task-{task_id}"))
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

    #[test]
    fn engineer_task_branch_name_uses_explicit_task_id() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "freeform task body", Some(123)),
            "eng-1-3/task-123"
        );
    }

    #[test]
    fn engineer_task_branch_name_extracts_task_id_from_assignment_text() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "Task #456: fix move generation", None),
            "eng-1-3/task-456"
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
}
