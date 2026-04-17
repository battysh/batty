//! Telegram bridge orchestration for the daemon poll loop.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

use super::*;

pub(super) fn build_telegram_bot(
    team_config: &TeamConfig,
) -> Option<super::super::telegram::TelegramBot> {
    team_config
        .roles
        .iter()
        .find(|role| {
            role.role_type == RoleType::User && role.channel.as_deref() == Some("telegram")
        })
        .and_then(|role| role.channel_config.as_ref())
        .and_then(super::super::telegram::TelegramBot::from_config)
}

impl TeamDaemon {
    pub(super) fn process_telegram_queue(&mut self) -> Result<()> {
        self.poll_telegram()?;
        self.deliver_user_channel_inbox()
    }

    fn poll_telegram(&mut self) -> Result<()> {
        if self.telegram_bot.is_none() {
            return Ok(());
        }

        let messages = match self
            .telegram_bot
            .as_mut()
            .expect("checked telegram bot presence")
            .poll_updates()
        {
            Ok(msgs) => msgs,
            Err(error) => {
                debug!(error = %error, "telegram poll failed");
                return Ok(());
            }
        };

        if messages.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let targets: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .find(|role| role.role_type == RoleType::User)
            .map(|role| role.talks_to.clone())
            .unwrap_or_default();

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "telegram inbound"
            );

            if let Some(reply) = self.handle_telegram_command(&msg) {
                if let Some(bot) = self.telegram_bot.as_ref() {
                    if let Err(error) = bot.send_message(&msg.chat_id.to_string(), &reply) {
                        warn!(chat_id = msg.chat_id, error = %error, "failed to send telegram reply");
                    }
                }
                continue;
            }

            for target in &targets {
                let inbox_msg = inbox::InboxMessage::new_send("human", target, &msg.text);
                if let Err(error) = inbox::deliver_to_inbox(&root, &inbox_msg) {
                    warn!(
                        to = %target,
                        error = %error,
                        "failed to deliver telegram message to inbox"
                    );
                }
            }

            self.record_message_routed("human", "telegram");
        }

        Ok(())
    }

    fn handle_telegram_command(
        &mut self,
        msg: &super::super::telegram::InboundMessage,
    ) -> Option<String> {
        let command = match parse_telegram_command(&msg.text) {
            Ok(Some(command)) => command,
            Ok(None) => return None,
            Err(error) => return Some(error.to_string()),
        };

        Some(match self.execute_telegram_command(command) {
            Ok(reply) => reply,
            Err(error) => format!("Command failed: {error}"),
        })
    }

    pub(super) fn execute_telegram_command(&mut self, command: TelegramCommand) -> Result<String> {
        match command {
            TelegramCommand::Status => Ok(self.render_telegram_status_summary()),
            TelegramCommand::Board { filter } => {
                render_telegram_board_summary(&self.config.project_root, filter.as_deref())
            }
            TelegramCommand::Logs { member } => {
                render_telegram_logs(&self.config.project_root, &member)
            }
            TelegramCommand::Health => {
                render_telegram_health_summary(&self.config.project_root, &self.config.members)
            }
            TelegramCommand::Assign { engineer, task } => {
                self.execute_telegram_assign_command(&engineer, &task)
            }
            TelegramCommand::Merge { task_id } => self.execute_telegram_merge_command(task_id),
            TelegramCommand::Kick { member } => self.execute_telegram_kick_command(&member),
            TelegramCommand::Pause => {
                crate::team::pause_team(&self.config.project_root)?;
                Ok("Automation paused.".to_string())
            }
            TelegramCommand::Resume => {
                crate::team::resume_team(&self.config.project_root)?;
                Ok("Automation resumed.".to_string())
            }
            TelegramCommand::Goal { text } => {
                write_telegram_goal(&self.config.project_root, &text)?;
                Ok(format!("Goal updated: {}", preview_text(&text, 180)))
            }
            TelegramCommand::Task { title } => {
                create_telegram_task(&self.config.project_root, &title)
            }
            TelegramCommand::Block { task_id, reason } => {
                block_telegram_task(&self.config.project_root, task_id, &reason)
            }
            TelegramCommand::Stop { confirm } => {
                if !confirm {
                    Ok("Reply with /stop confirm to stop the team.".to_string())
                } else {
                    let root = self.config.project_root.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let _ = crate::team::stop_team(&root);
                    });
                    Ok("Stopping team.".to_string())
                }
            }
            TelegramCommand::Start => {
                let session = format!("batty-{}", self.config.team_config.name);
                if crate::tmux::session_exists(&session) {
                    Ok(format!("Team already running: {session}"))
                } else {
                    let started = crate::team::start_team(&self.config.project_root, false)?;
                    Ok(format!("Started team: {started}"))
                }
            }
            TelegramCommand::Help => Ok(render_telegram_help()),
            TelegramCommand::Send { role, message } => {
                crate::team::messaging::send_message_as(
                    &self.config.project_root,
                    Some("human"),
                    &role,
                    &message,
                )?;
                Ok(format!("Sent to {role}: {}", preview_text(&message, 120)))
            }
        }
    }

    pub(super) fn render_telegram_status_summary(&self) -> String {
        let session = format!("batty-{}", self.config.team_config.name);
        let running = crate::tmux::session_exists(&session);
        let paused = crate::team::pause_marker_path(&self.config.project_root).exists();
        let triage_backlog_counts = crate::team::status::triage_backlog_counts(
            &self.config.project_root,
            &self.config.members,
        );
        let inbox_root = crate::team::inbox::inboxes_root(&self.config.project_root);
        let pending_inbox_total: usize = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type != RoleType::User)
            .map(|member| {
                crate::team::inbox::pending_message_count(&inbox_root, &member.name).unwrap_or(0)
            })
            .sum();
        let triage_total: usize = triage_backlog_counts.values().sum();

        let mut state_counts = BTreeMap::new();
        for member in &self.config.members {
            if member.role_type == RoleType::User {
                continue;
            }
            let label = match self
                .states
                .get(&member.name)
                .copied()
                .unwrap_or(MemberState::Idle)
            {
                MemberState::Idle => "idle",
                MemberState::Working => "working",
            };
            *state_counts.entry(label).or_insert(0usize) += 1;
        }

        let states = if state_counts.is_empty() {
            "none".to_string()
        } else {
            state_counts
                .into_iter()
                .map(|(state, count)| format!("{state}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let (active_tasks, review_tasks) =
            crate::team::status::board_status_task_queues(&self.config.project_root)
                .unwrap_or_else(|_| (Vec::new(), Vec::new()));

        format!(
            "Team: {}\nSession: {}{}\nMembers: {}\nInbox: {}\nTriage: {}\nBoard: active={}, review={}",
            self.config.team_config.name,
            if running { "running" } else { "stopped" },
            if paused { " (paused)" } else { "" },
            states,
            pending_inbox_total,
            triage_total,
            active_tasks.len(),
            review_tasks.len(),
        )
    }

    pub(super) fn execute_telegram_assign_command(
        &mut self,
        engineer: &str,
        task: &str,
    ) -> Result<String> {
        let engineer_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .ok_or_else(|| anyhow!("Unknown engineer: {engineer}"))?;
        if engineer_member.role_type != RoleType::Engineer {
            bail!("{engineer} is not an engineer.");
        }
        if self.states.get(engineer) == Some(&MemberState::Working) {
            bail!("{engineer} is not idle.");
        }

        if let Some(task_id) = parse_task_id_arg(task) {
            let board_dir = self.board_dir();
            let task_path = crate::team::task_cmd::find_task_path(&board_dir, task_id)?;
            let task_record = crate::task::Task::from_file(&task_path)?;
            if !matches!(task_record.status.as_str(), "backlog" | "todo") {
                bail!(
                    "Task #{task_id} is not assignable from status '{}'.",
                    task_record.status
                );
            }
            if task_record.blocked.is_some() || task_record.blocked_on.is_some() {
                bail!("Task #{task_id} is blocked.");
            }

            crate::team::task_cmd::assign_task_owners(&board_dir, task_id, Some(engineer), None)?;
            if task_record.status == "backlog" {
                crate::team::task_cmd::transition_task(&board_dir, task_id, "todo")?;
            }
            crate::team::task_cmd::transition_task(&board_dir, task_id, "in-progress")?;

            let inbox_id = crate::team::messaging::assign_task(
                &self.config.project_root,
                engineer,
                &format!("Task #{task_id}: {}", task_record.title),
            )?;
            Ok(format!(
                "Assigned {engineer} to #{task_id}: {}\nInbox id: {inbox_id}",
                preview_text(&task_record.title, 120)
            ))
        } else {
            let id =
                crate::team::messaging::assign_task(&self.config.project_root, engineer, task)?;
            Ok(format!("Assigned {engineer}: {task}\nInbox id: {id}"))
        }
    }

    pub(super) fn execute_telegram_merge_command(&mut self, task_id: u32) -> Result<String> {
        let board_dir = self.board_dir();
        let task_path = crate::team::task_cmd::find_task_path(&board_dir, task_id)?;
        let task = crate::task::Task::from_file(&task_path)?;
        if task.status != "review" {
            bail!("Task #{task_id} is not in review.");
        }

        let engineer = engineer_for_merge_task(&self.config.project_root, task_id)?;
        let worktree_dir = self.worktree_dir(&engineer);
        let verification_policy = &self.config.team_config.workflow_policy.verification;
        let test_command = verification_policy.test_command.as_deref().or(self
            .config
            .team_config
            .workflow_policy
            .test_command
            .as_deref());
        let test_run = crate::team::task_loop::run_tests_in_worktree(&worktree_dir, test_command)?;
        if !test_run.passed {
            bail!(
                "Task #{task_id} verification failed before merge.\n{}",
                preview_text(&test_run.results.failure_summary(), 240)
            );
        }

        crate::team::messaging::merge_worktree(&self.config.project_root, &engineer)?;
        crate::team::task_cmd::cmd_review_structured(
            &board_dir, task_id, "approve", None, "telegram",
        )?;
        let test_summary = test_run.results.summary.clone().unwrap_or_else(|| {
            format!(
                "{} passed, {} failed",
                test_run.results.passed, test_run.results.failed
            )
        });
        Ok(format!(
            "Merged Task #{task_id} from {engineer}. Tests: {test_summary}"
        ))
    }

    pub(super) fn execute_telegram_kick_command(&mut self, member: &str) -> Result<String> {
        if self.active_task_id(member).is_some() {
            self.restart_member_with_task_context(member, "telegram kick")?;
            return Ok(format!("Restarted {member} with structured handoff."));
        }

        let mut saved = false;
        if self.member_uses_worktrees(member) {
            let worktree_dir = self.worktree_dir(member);
            if worktree_dir.exists() {
                saved = crate::team::task_loop::preserve_worktree_with_commit_for(
                    &worktree_dir,
                    "wip: auto-save before telegram kick [batty]",
                    std::time::Duration::from_secs(
                        self.config
                            .team_config
                            .workflow_policy
                            .graceful_shutdown_timeout_secs,
                    ),
                    "telegram kick",
                )?;
            }
        }

        let pane_id = self
            .config
            .pane_map
            .get(member)
            .ok_or_else(|| anyhow!("No pane registered for {member}."))?;
        crate::tmux::respawn_pane(pane_id, "bash")?;
        Ok(if saved {
            format!("Restarted {member}. Worktree auto-saved.")
        } else {
            format!("Restarted {member}.")
        })
    }

    pub(super) fn deliver_user_channel_inbox(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let user_roles: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .filter(|role| role.role_type == RoleType::User)
            .map(|role| role.name.clone())
            .collect();

        for user_name in &user_roles {
            let messages = match inbox::pending_messages(&root, user_name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(user = %user_name, error = %error, "failed to read user inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            for msg in &messages {
                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                let send_result = match self.channels.get(user_name) {
                    Some(channel) => channel.send(&formatted),
                    None => {
                        debug!(user = %user_name, "no channel for user role");
                        break;
                    }
                };
                if let Err(error) = send_result {
                    warn!(to = %user_name, error = %error, "failed to send via channel");
                    continue;
                }

                if let Err(error) = inbox::mark_delivered(&root, user_name, &msg.id) {
                    warn!(user = %user_name, id = %msg.id, error = %error, "failed to mark delivered");
                }

                self.record_message_routed(&msg.from, user_name);
            }
        }

        Ok(())
    }

    pub(crate) fn automation_sender_for(&self, recipient: &str) -> String {
        let recipient_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == recipient);

        if let Some(member) = recipient_member {
            if let Some(parent) = &member.reports_to {
                return parent.clone();
            }
        }

        if let Some(sender) = &self.config.team_config.automation_sender {
            return sender.clone();
        }

        "daemon".to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TelegramCommand {
    Status,
    Board { filter: Option<String> },
    Logs { member: String },
    Health,
    Assign { engineer: String, task: String },
    Merge { task_id: u32 },
    Kick { member: String },
    Pause,
    Resume,
    Goal { text: String },
    Task { title: String },
    Block { task_id: u32, reason: String },
    Stop { confirm: bool },
    Start,
    Help,
    Send { role: String, message: String },
}

fn parse_telegram_command(text: &str) -> Result<Option<TelegramCommand>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }

    let (name, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(name, rest)| (name, rest.trim()))
        .unwrap_or((trimmed, ""));

    match name {
        "/status" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Status))
            } else {
                Err(anyhow!("Usage: /status"))
            }
        }
        "/board" => {
            let filter = if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
            Ok(Some(TelegramCommand::Board { filter }))
        }
        "/logs" => {
            if rest.is_empty() {
                bail!("Usage: /logs <engineer>");
            }
            Ok(Some(TelegramCommand::Logs {
                member: rest.to_string(),
            }))
        }
        "/health" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Health))
            } else {
                Err(anyhow!("Usage: /health"))
            }
        }
        "/assign" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let engineer = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /assign <engineer> <task>"))?;
            let task = parts.next().unwrap_or("").trim();
            if task.is_empty() {
                bail!("Usage: /assign <engineer> <task>");
            }
            Ok(Some(TelegramCommand::Assign {
                engineer: engineer.to_string(),
                task: task.to_string(),
            }))
        }
        "/kick" => {
            if rest.is_empty() {
                bail!("Usage: /kick <engineer>");
            }
            Ok(Some(TelegramCommand::Kick {
                member: rest.to_string(),
            }))
        }
        "/pause" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Pause))
            } else {
                Err(anyhow!("Usage: /pause"))
            }
        }
        "/resume" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Resume))
            } else {
                Err(anyhow!("Usage: /resume"))
            }
        }
        "/goal" => {
            if rest.is_empty() {
                bail!("Usage: /goal <text>");
            }
            Ok(Some(TelegramCommand::Goal {
                text: rest.to_string(),
            }))
        }
        "/task" => {
            if rest.is_empty() {
                bail!("Usage: /task <title>");
            }
            Ok(Some(TelegramCommand::Task {
                title: rest.to_string(),
            }))
        }
        "/block" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let task_id = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /block <task> <reason>"))?
                .trim_start_matches('#')
                .parse::<u32>()
                .map_err(|_| anyhow!("Usage: /block <task> <reason>"))?;
            let reason = parts.next().unwrap_or("").trim();
            if reason.is_empty() {
                bail!("Usage: /block <task> <reason>");
            }
            Ok(Some(TelegramCommand::Block {
                task_id,
                reason: reason.to_string(),
            }))
        }
        "/merge" => {
            if rest.is_empty() {
                bail!("Usage: /merge <task>");
            }
            let task_id = rest
                .trim_start_matches('#')
                .parse::<u32>()
                .map_err(|_| anyhow!("Usage: /merge <task>"))?;
            Ok(Some(TelegramCommand::Merge { task_id }))
        }
        "/stop" => Ok(Some(TelegramCommand::Stop {
            confirm: rest == "confirm",
        })),
        "/start" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Start))
            } else {
                Err(anyhow!("Usage: /start"))
            }
        }
        "/help" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Help))
            } else {
                Err(anyhow!("Usage: /help"))
            }
        }
        "/send" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let role = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /send <role> <message>"))?;
            let message = parts.next().unwrap_or("").trim();
            if message.is_empty() {
                bail!("Usage: /send <role> <message>");
            }
            Ok(Some(TelegramCommand::Send {
                role: role.to_string(),
                message: message.to_string(),
            }))
        }
        _ => Err(anyhow!(
            "Unknown command. Supported: /status, /board, /logs, /health, /assign, /merge, /kick, /pause, /resume, /goal, /task, /block, /stop, /start, /help, /send"
        )),
    }
}

fn render_telegram_board_summary(
    project_root: &std::path::Path,
    filter: Option<&str>,
) -> Result<String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok("Board: no tasks found.".to_string());
    }

    let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
    if let Some(filter) = filter {
        let filtered: Vec<_> = tasks
            .iter()
            .filter(|task| task.status == filter)
            .take(8)
            .map(|task| match task.claimed_by.as_deref() {
                Some(owner) => format!("#{} {} ({owner})", task.id, preview_text(&task.title, 40)),
                None => format!("#{} {}", task.id, preview_text(&task.title, 40)),
            })
            .collect();
        let summary = if filtered.is_empty() {
            "none".to_string()
        } else {
            filtered.join("; ")
        };
        return Ok(format!("Board [{filter}]: {summary}"));
    }

    let mut counts = BTreeMap::<String, usize>::new();
    for task in &tasks {
        *counts.entry(task.status.clone()).or_insert(0) += 1;
    }

    let counts_summary = if counts.is_empty() {
        "empty".to_string()
    } else {
        counts
            .into_iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let (active_tasks, review_tasks) = crate::team::status::board_status_task_queues(project_root)?;
    let active_summary = summarize_status_entries(&active_tasks);
    let review_summary = summarize_status_entries(&review_tasks);

    Ok(format!(
        "Board: {counts_summary}\nActive: {active_summary}\nReview: {review_summary}"
    ))
}

fn render_telegram_logs(project_root: &std::path::Path, member: &str) -> Result<String> {
    let path = crate::team::shim_log_path(project_root, member);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("log not found for {member}: {}", path.display()))?;
    let lines: Vec<&str> = content.lines().rev().take(5).collect();
    if lines.is_empty() {
        return Ok(format!("{member}: no log output yet."));
    }
    let rendered = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    Ok(format!("{member} (last 5 lines)\n{rendered}"))
}

fn render_telegram_health_summary(
    project_root: &std::path::Path,
    members: &[crate::team::hierarchy::MemberInstance],
) -> Result<String> {
    let summary = crate::team::compute_session_summary(project_root);
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let metrics = crate::team::status::compute_metrics(&board_dir, members).unwrap_or_default();
    let lock_count = count_lock_files(project_root);
    Ok(format!(
        "Health\nRuntime: {}\nCompletions: {}\nMerges: {}\nReview queue: {}\nIn progress: {}\nRunnable: {}\nCargo locks: {}",
        summary
            .as_ref()
            .map(|s| s.runtime_secs.to_string())
            .unwrap_or_else(|| "0".to_string()),
        summary.as_ref().map(|s| s.tasks_completed).unwrap_or(0),
        summary.as_ref().map(|s| s.tasks_merged).unwrap_or(0),
        metrics.in_review_count,
        metrics.in_progress_count,
        metrics.runnable_count,
        lock_count,
    ))
}

fn count_lock_files(project_root: &std::path::Path) -> usize {
    let mut count = 0usize;
    let candidates = [
        project_root.join(".git").join("index.lock"),
        project_root
            .join(".batty")
            .join("shared-target")
            .join("Cargo.lock"),
    ];
    for path in candidates {
        if path.exists() {
            count += 1;
        }
    }
    count
}

fn write_telegram_goal(project_root: &std::path::Path, text: &str) -> Result<()> {
    let goal_path = project_root.join(".batty").join("goal.yaml");
    if let Some(parent) = goal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = format!("goal: {}\n", serde_yaml::to_string(text)?.trim());
    std::fs::write(goal_path, rendered)?;
    Ok(())
}

fn create_telegram_task(project_root: &std::path::Path, title: &str) -> Result<String> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let task_id = crate::team::board_cmd::create_task(
        &board_dir,
        title,
        "Created from Telegram.",
        Some("medium"),
        None,
        None,
    )
    .map_err(anyhow::Error::from)?;
    Ok(format!(
        "Created task #{task_id}: {}",
        preview_text(title, 120)
    ))
}

fn block_telegram_task(
    project_root: &std::path::Path,
    task_id: u32,
    reason: &str,
) -> Result<String> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let mut fields = std::collections::HashMap::new();
    fields.insert("blocked_on".to_string(), reason.to_string());
    crate::team::task_cmd::cmd_update(&board_dir, task_id, fields)?;
    Ok(format!("Blocked #{task_id}: {}", preview_text(reason, 160)))
}

fn parse_task_id_arg(text: &str) -> Option<u32> {
    text.trim().trim_start_matches('#').parse::<u32>().ok()
}

fn render_telegram_help() -> String {
    "/status, /board [status], /logs <eng>, /health\n/assign <eng> <task|id>, /merge <task>, /kick <eng>, /pause, /resume\n/goal <text>, /task <title>, /block <task> <reason>, /stop [confirm], /start, /help, /send <role> <message>".to_string()
}

fn summarize_status_entries(entries: &[crate::team::status::StatusTaskEntry]) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }

    entries
        .iter()
        .take(3)
        .map(|entry| {
            let base = match entry.claimed_by.as_deref() {
                Some(owner) => {
                    format!("#{} {} ({owner})", entry.id, preview_text(&entry.title, 32))
                }
                None => format!("#{} {}", entry.id, preview_text(&entry.title, 32)),
            };
            match entry.test_summary.as_deref() {
                Some(summary) => format!("{base} [{summary}]"),
                None => base,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn engineer_for_merge_task(project_root: &std::path::Path, task_id: u32) -> Result<String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        bail!("Board not found.");
    }

    let task = crate::task::load_tasks_from_dir(&tasks_dir)?
        .into_iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("Task #{task_id} not found."))?;

    let engineer = task
        .claimed_by
        .ok_or_else(|| anyhow!("Task #{task_id} is not assigned."))?;
    if !engineer.starts_with("eng-") {
        bail!("Task #{task_id} is assigned to {engineer}, not an engineer.");
    }

    Ok(engineer)
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::team::comms::Channel;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, OrchestratorPosition, RoleDef, StandupConfig,
        TeamConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::daemon::DaemonConfig;
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_helpers::daemon_config_with_roles;

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    fn backdate_idle_grace(daemon: &mut TeamDaemon, member_name: &str) {
        let grace = daemon.automation_idle_grace_duration() + Duration::from_secs(1);
        daemon
            .idle_started_at
            .insert(member_name.to_string(), Instant::now() - grace);
        if let Some(schedule) = daemon.nudges.get_mut(member_name) {
            schedule.idle_since = Some(Instant::now() - schedule.interval.max(grace));
        }
    }

    fn write_board_task(project_root: &std::path::Path, file_name: &str, body: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join(file_name), body).unwrap();
    }

    fn git_ok(repo: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn process_telegram_queue_delivers_pending_user_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: vec![RoleDef {
                    name: "human".to_string(),
                    role_type: RoleType::User,
                    agent: None,
                    auth_mode: None,
                    auth_env: vec![],
                    instances: 1,
                    prompt: None,
                    talks_to: vec!["architect".to_string()],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    barrier_group: None,
                    use_worktrees: false,
                    ..Default::default()
                }],
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("architect", "human", "Status update");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        daemon.process_telegram_queue().unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["--- Message from architect ---\nStatus update"]
        );
        assert!(inbox::pending_messages(&root, "human").unwrap().is_empty());
    }

    #[test]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "scientist".to_string(),
            role_name: "scientist".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut watchers = HashMap::new();
        let mut scientist_watcher = SessionWatcher::new("%9999999", "scientist", 300, None);
        scientist_watcher.confirm_ready();
        watchers.insert("scientist".to_string(), scientist_watcher);

        // Create a shim handle in Idle state so deliver_message returns LivePane
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "scientist".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        let mut shim_handles = HashMap::new();
        shim_handles.insert("scientist".to_string(), handle);

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![member],
                pane_map: HashMap::from([("scientist".to_string(), "%9999999".to_string())]),
            },
            watchers,
            states: HashMap::from([("scientist".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]),
            discord_bot: None,
            discord_event_cursor: 0,
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            last_main_smoke_check: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            current_tick_errors: Vec::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            recent_escalations: HashMap::new(),
            main_smoke_state: None,
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            backend_quota_retry_at: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            last_disk_hygiene_check: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            zero_diff_completion_counts: HashMap::new(),
            shim_handles,
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            planning_cycle_consecutive_empty: 0,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
            last_binary_freshness_check: Instant::now(),
            last_tiered_inbox_sweep: Instant::now(),
        };

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        // Shim-managed agents: state driven by shim events, not speculative mark_member_working.
        // mark_member_working is a no-op for shim agents, so state stays Idle and
        // nudge timers are not reset by update_automation_timers_for_state.
        assert_eq!(
            daemon.states.get("scientist"),
            Some(&MemberState::Idle),
            "shim-managed agent state stays Idle; real state comes from shim events"
        );
        let schedule = daemon.nudges.get("scientist").unwrap();
        // Nudge is NOT paused because mark_member_working is a no-op for shim agents
        assert!(!schedule.paused);
        // idle_since is still set (not cleared) for the same reason
        assert!(schedule.idle_since.is_some());
        // The nudge DID fire (delivered_live was true)
        assert!(schedule.fired_this_idle);
    }

    #[test]
    #[serial_test::serial]
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let session = format!("batty-test-triage-live-delivery-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut watchers = HashMap::new();
        let mut lead_watcher = SessionWatcher::new(&pane_id, "lead", 300, None);
        lead_watcher.confirm_ready();
        watchers.insert("lead".to_string(), lead_watcher);
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: session.clone(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), pane_id.clone())]),
            },
            watchers,
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            discord_bot: None,
            discord_event_cursor: 0,
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            last_main_smoke_check: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            current_tick_errors: Vec::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            recent_escalations: HashMap::new(),
            main_smoke_state: None,
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            backend_quota_retry_at: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            last_disk_hygiene_check: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            zero_diff_completion_counts: HashMap::new(),
            shim_handles: HashMap::new(),
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            planning_cycle_consecutive_empty: 0,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
            last_binary_freshness_check: Instant::now(),
            last_tiered_inbox_sweep: Instant::now(),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        if daemon.states.get("lead") == Some(&MemberState::Working) {
            let pane = (0..100)
                .find_map(|_| {
                    let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                    if pane.contains("batty send architect")
                        && pane.contains("next time you become idle")
                    {
                        Some(pane)
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                        None
                    }
                })
                .unwrap_or_else(|| tmux::capture_pane(&pane_id).unwrap_or_default());
            assert!(pane.contains("Triage backlog detected"));
            assert!(pane.contains("batty send architect"));
            assert!(pane.contains("next time you become idle"));
        } else {
            let pending = inbox::pending_messages(&root, "lead").unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].body.contains("batty inbox lead"));
        }

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn automation_sender_prefers_direct_manager_and_config_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: Some("human".to_string()),
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    grafana: Default::default(),
                    use_shim: false,
                    use_sdk_mode: false,
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    shim_working_state_timeout_secs: 1800,
                    pending_queue_max_age_secs: 600,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![
                    MemberInstance {
                        name: "architect".to_string(),
                        role_name: "architect".to_string(),
                        role_type: RoleType::Architect,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: None,
                        use_worktrees: false,
                        ..Default::default()
                    },
                    MemberInstance {
                        name: "lead".to_string(),
                        role_name: "lead".to_string(),
                        role_type: RoleType::Manager,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: Some("architect".to_string()),
                        use_worktrees: false,
                        ..Default::default()
                    },
                    MemberInstance {
                        name: "eng-1".to_string(),
                        role_name: "eng".to_string(),
                        role_type: RoleType::Engineer,
                        agent: Some("codex".to_string()),
                        prompt: None,
                        reports_to: Some("lead".to_string()),
                        use_worktrees: false,
                        ..Default::default()
                    },
                ],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            discord_bot: None,
            discord_event_cursor: 0,
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            last_main_smoke_check: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            current_tick_errors: Vec::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            recent_escalations: HashMap::new(),
            main_smoke_state: None,
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            backend_quota_retry_at: HashMap::new(),
            narration_tracker: Default::default(),
            context_pressure_tracker: Default::default(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now(),
            last_disk_hygiene_check: Instant::now(),
            pending_delivery_queue: HashMap::new(),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            zero_diff_completion_counts: HashMap::new(),
            shim_handles: HashMap::new(),
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            planning_cycle_consecutive_empty: 0,
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
            last_binary_freshness_check: Instant::now(),
            last_tiered_inbox_sweep: Instant::now(),
        };

        assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
        assert_eq!(daemon.automation_sender_for("lead"), "architect");
        assert_eq!(daemon.automation_sender_for("architect"), "human");

        daemon.config.team_config.automation_sender = None;
        assert_eq!(daemon.automation_sender_for("architect"), "daemon");
    }

    #[test]
    fn daemon_creates_telegram_bot_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            channel_config: Some(ChannelConfig {
                target: "12345".to_string(),
                provider: "telegram".to_string(),
                bot_token: Some("test-token-123".to_string()),
                allowed_user_ids: vec![42],
                events_channel_id: None,
                agents_channel_id: None,
                commands_channel_id: None,
                board_channel_id: None,
            }),
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.telegram_bot.is_some());
    }

    #[test]
    fn daemon_no_telegram_bot_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.telegram_bot.is_none());
    }

    // --- New tests for #255 ---

    #[test]
    fn build_telegram_bot_returns_none_when_no_user_role() {
        let roles = vec![RoleDef {
            name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn build_telegram_bot_returns_none_when_user_has_different_channel() {
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: Some("slack".to_string()),
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn build_telegram_bot_returns_none_when_channel_config_missing() {
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn process_telegram_queue_no_pending_messages_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon.process_telegram_queue().unwrap();
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn deliver_user_inbox_multiple_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "First message"),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("manager", "human", "Second message"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 2);
        // Order depends on filesystem listing — check both messages are present
        let combined: String = messages.join("\n");
        assert!(combined.contains("First message"));
        assert!(combined.contains("Second message"));
    }

    #[test]
    fn deliver_user_inbox_no_channel_skips_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        // Intentionally do NOT insert a channel

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "Test"),
        )
        .unwrap();

        // Should not panic — just skips delivery
        daemon.process_telegram_queue().unwrap();

        // Message should still be pending since it couldn't be delivered
        let pending = inbox::pending_messages(&root, "human").unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn automation_sender_for_unknown_recipient_uses_config_sender() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: Some("boss".to_string()),
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        // "nobody" is not a member → falls through to automation_sender config
        assert_eq!(daemon.automation_sender_for("nobody"), "boss");
    }

    #[test]
    fn automation_sender_for_unknown_recipient_no_config_defaults_to_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        assert_eq!(daemon.automation_sender_for("nobody"), "daemon");
    }

    #[test]
    fn deliver_user_inbox_marks_messages_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "Test delivery"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        // After delivery, no pending messages should remain
        let pending = inbox::pending_messages(&root, "human").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn deliver_user_inbox_formats_message_with_sender() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("engineer-1", "human", "Task done"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("--- Message from engineer-1 ---\n"));
        assert!(messages[0].contains("Task done"));
    }

    #[test]
    fn deliver_user_inbox_multiple_users() {
        let tmp = tempfile::tempdir().unwrap();
        let sent_alice = Arc::new(Mutex::new(Vec::new()));
        let sent_bob = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![
            RoleDef {
                name: "alice".to_string(),
                role_type: RoleType::User,
                agent: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec!["architect".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "bob".to_string(),
                role_type: RoleType::User,
                agent: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec!["architect".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
        ];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.channels.insert(
            "alice".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent_alice),
            }),
        );
        daemon.channels.insert(
            "bob".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent_bob),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "alice", "Hello Alice"),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "bob", "Hello Bob"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        assert_eq!(sent_alice.lock().unwrap().len(), 1);
        assert!(sent_alice.lock().unwrap()[0].contains("Hello Alice"));
        assert_eq!(sent_bob.lock().unwrap().len(), 1);
        assert!(sent_bob.lock().unwrap()[0].contains("Hello Bob"));
    }

    #[test]
    fn automation_sender_for_member_with_reports_to_returns_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: Some("default-sender".to_string()),
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: vec![MemberInstance {
                name: "mgr".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("boss".to_string()),
                use_worktrees: false,
                ..Default::default()
            }],
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        assert_eq!(daemon.automation_sender_for("mgr"), "boss");
    }

    #[test]
    fn parse_telegram_commands_supports_phone_friendly_forms() {
        assert_eq!(
            parse_telegram_command("/status").unwrap(),
            Some(TelegramCommand::Status)
        );
        assert_eq!(
            parse_telegram_command("/board").unwrap(),
            Some(TelegramCommand::Board { filter: None })
        );
        assert_eq!(
            parse_telegram_command("/board review").unwrap(),
            Some(TelegramCommand::Board {
                filter: Some("review".to_string()),
            })
        );
        assert_eq!(
            parse_telegram_command("/logs eng-1").unwrap(),
            Some(TelegramCommand::Logs {
                member: "eng-1".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/health").unwrap(),
            Some(TelegramCommand::Health)
        );
        assert_eq!(
            parse_telegram_command("/pause").unwrap(),
            Some(TelegramCommand::Pause)
        );
        assert_eq!(
            parse_telegram_command("/resume").unwrap(),
            Some(TelegramCommand::Resume)
        );
        assert_eq!(
            parse_telegram_command("/task Add remote control").unwrap(),
            Some(TelegramCommand::Task {
                title: "Add remote control".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/block 41 waiting on CI").unwrap(),
            Some(TelegramCommand::Block {
                task_id: 41,
                reason: "waiting on CI".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/stop confirm").unwrap(),
            Some(TelegramCommand::Stop { confirm: true })
        );
        assert_eq!(
            parse_telegram_command("/help").unwrap(),
            Some(TelegramCommand::Help)
        );
        assert_eq!(
            parse_telegram_command("/assign eng-1 Fix flaky test").unwrap(),
            Some(TelegramCommand::Assign {
                engineer: "eng-1".to_string(),
                task: "Fix flaky test".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/merge #41").unwrap(),
            Some(TelegramCommand::Merge { task_id: 41 })
        );
        assert_eq!(
            parse_telegram_command("/send architect Need review on task 41").unwrap(),
            Some(TelegramCommand::Send {
                role: "architect".to_string(),
                message: "Need review on task 41".to_string(),
            })
        );
    }

    #[test]
    fn parse_telegram_command_rejects_invalid_usage() {
        assert_eq!(parse_telegram_command("hello").unwrap(), None);
        assert!(parse_telegram_command("/assign eng-1").is_err());
        assert!(parse_telegram_command("/logs").is_err());
        assert!(parse_telegram_command("/merge nope").is_err());
        assert!(parse_telegram_command("/goal").is_err());
        assert!(parse_telegram_command("/task").is_err());
        assert!(parse_telegram_command("/block 41").is_err());
        assert!(parse_telegram_command("/send architect").is_err());
        assert!(parse_telegram_command("/unknown").is_err());
    }

    #[test]
    fn telegram_assign_command_delivers_assignment_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "human".to_string(),
                role_type: RoleType::User,
                agent: None,
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: vec!["eng".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
            RoleDef {
                name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: Vec::new(),
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let reply = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "Fix flaky test".to_string(),
            })
            .unwrap();

        assert!(reply.contains("Assigned eng"));
        let pending = inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Fix flaky test"));
    }

    #[test]
    fn telegram_assign_command_rejects_busy_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            instance_overrides: HashMap::new(),
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon
            .states
            .insert("eng".to_string(), MemberState::Working);

        let error = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "Fix flaky test".to_string(),
            })
            .unwrap_err();

        assert!(error.to_string().contains("not idle"));
    }

    #[test]
    fn telegram_assign_command_rejects_blocked_task() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            instance_overrides: HashMap::new(),
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.states.insert("eng".to_string(), MemberState::Idle);
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Waiting task\nstatus: todo\npriority: high\nblocked_on: waiting for auth\nclass: standard\n---\n",
        );

        let error = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "41".to_string(),
            })
            .unwrap_err();

        assert!(error.to_string().contains("blocked"));
    }

    #[test]
    fn telegram_send_command_delivers_message_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "human".to_string(),
                role_type: RoleType::User,
                agent: None,
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: vec!["architect".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: Vec::new(),
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let reply = daemon
            .execute_telegram_command(TelegramCommand::Send {
                role: "architect".to_string(),
                message: "Need a quick review".to_string(),
            })
            .unwrap();

        assert!(reply.contains("Sent to architect"));
        let pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "Need a quick review");
    }

    #[test]
    fn render_telegram_board_summary_reports_counts_and_queues() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-11.md",
            "---\nid: 11\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-12.md",
            "---\nid: 12\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-2\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-13.md",
            "---\nid: 13\ntitle: Todo task\nstatus: todo\npriority: low\nclass: standard\n---\n",
        );

        let summary = render_telegram_board_summary(tmp.path(), None).unwrap();
        assert!(summary.contains("in-progress=1"));
        assert!(summary.contains("review=1"));
        assert!(summary.contains("todo=1"));
        assert!(summary.contains("#11 Active task (eng-1)"));
        assert!(summary.contains("#12 Review task (eng-2)"));
    }

    #[test]
    fn engineer_for_merge_task_reads_claimed_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Merge me\nstatus: review\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );

        assert_eq!(engineer_for_merge_task(tmp.path(), 41).unwrap(), "eng-1");
    }

    #[test]
    fn telegram_merge_command_rejects_non_review_task() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Merge me\nstatus: todo\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );
        let mut daemon = TeamDaemon::new(daemon_config_with_roles(tmp.path(), Vec::new())).unwrap();

        let error = daemon
            .execute_telegram_command(TelegramCommand::Merge { task_id: 41 })
            .unwrap_err();

        assert!(error.to_string().contains("not in review"));
    }

    #[test]
    fn pause_and_resume_commands_toggle_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon::new(daemon_config_with_roles(tmp.path(), Vec::new())).unwrap();

        assert_eq!(
            daemon
                .execute_telegram_command(TelegramCommand::Pause)
                .unwrap(),
            "Automation paused."
        );
        assert!(crate::team::pause_marker_path(tmp.path()).exists());

        assert_eq!(
            daemon
                .execute_telegram_command(TelegramCommand::Resume)
                .unwrap(),
            "Automation resumed."
        );
        assert!(!crate::team::pause_marker_path(tmp.path()).exists());
    }

    #[test]
    fn telegram_kick_command_autosaves_dirty_worktree_before_restart() {
        let tmp = tempfile::tempdir().unwrap();
        git_ok(tmp.path(), &["init"]);
        git_ok(tmp.path(), &["config", "user.email", "test@example.com"]);
        git_ok(tmp.path(), &["config", "user.name", "Test User"]);
        std::fs::write(tmp.path().join("README.md"), "root\n").unwrap();
        git_ok(tmp.path(), &["add", "README.md"]);
        git_ok(tmp.path(), &["commit", "-m", "init"]);

        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        git_ok(&worktree_dir, &["init"]);
        git_ok(&worktree_dir, &["config", "user.email", "test@example.com"]);
        git_ok(&worktree_dir, &["config", "user.name", "Test User"]);
        std::fs::write(worktree_dir.join("README.md"), "worktree\n").unwrap();
        git_ok(&worktree_dir, &["add", "README.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "init"]);
        std::fs::write(worktree_dir.join("README.md"), "dirty\n").unwrap();

        let roles = vec![RoleDef {
            name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            instance_overrides: HashMap::new(),
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: true,
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: true,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let error = daemon
            .execute_telegram_command(TelegramCommand::Kick {
                member: "eng-1".to_string(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("No pane registered"));

        let status = Command::new("git")
            .args(["status", "--short"])
            .current_dir(&worktree_dir)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(String::from_utf8_lossy(&status.stdout).trim().is_empty());
    }

    #[test]
    fn block_command_updates_task_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-91.md",
            "---\nid: 91\ntitle: Needs dependency\nstatus: todo\npriority: medium\nclass: standard\n---\n",
        );

        let reply = block_telegram_task(tmp.path(), 91, "waiting for auth").unwrap();
        assert!(reply.contains("Blocked #91"));

        let task_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("task-91.md");
        let content = std::fs::read_to_string(task_path).unwrap();
        assert!(content.contains("blocked_on: waiting for auth"));
    }

    #[test]
    fn status_summary_reports_member_and_board_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "human".to_string(),
                role_type: RoleType::User,
                agent: None,
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: vec!["architect".to_string(), "eng".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: Vec::new(),
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
            RoleDef {
                name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                instance_overrides: HashMap::new(),
                talks_to: Vec::new(),
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon
            .states
            .insert("architect".to_string(), MemberState::Working);
        daemon.states.insert("eng".to_string(), MemberState::Idle);
        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("human", "architect", "Need input"),
        )
        .unwrap();
        write_board_task(
            tmp.path(),
            "task-50.md",
            "---\nid: 50\ntitle: In progress\nstatus: in-progress\npriority: high\nclaimed_by: eng\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-51.md",
            "---\nid: 51\ntitle: In review\nstatus: review\npriority: medium\nclaimed_by: eng\nclass: standard\n---\n",
        );

        let summary = daemon.render_telegram_status_summary();
        assert!(summary.contains("Members: idle=1, working=1"));
        assert!(summary.contains("Inbox: 1"));
        assert!(summary.contains("Board: active=1, review=1"));
    }
}
