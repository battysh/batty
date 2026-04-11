//! Discord bridge orchestration for the daemon poll loop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use tracing::{debug, info, warn};

use super::telegram_bridge::TelegramCommand;
use super::*;
use crate::task::{Task, load_tasks_from_dir};
use crate::team::config::{ChannelConfig, RoleType, TeamConfig};
use crate::team::discord::{
    DiscordBot, EmbedField, RichEmbed, Severity, role_author_label, severity_for_event,
};
use crate::team::events::{TeamEvent, read_events};
use crate::team::inbox;

const DISCORD_BOARD_SYNC_INTERVAL: Duration = Duration::from_secs(30);
const DISCORD_BOARD_SYNC_KEY: &str = "discord::board_sync";
const DISCORD_BOARD_MAX_SECTION_LINES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscordShutdownSnapshot {
    pub(crate) tasks_completed: u32,
    pub(crate) tasks_merged: u32,
    pub(crate) runtime_secs: Option<u64>,
    pub(crate) in_progress: usize,
    pub(crate) todo: usize,
    pub(crate) review: usize,
    pub(crate) last_test_health: String,
}

pub(super) fn build_discord_bot(team_config: &TeamConfig) -> Option<DiscordBot> {
    team_config
        .roles
        .iter()
        .find(|role| role.role_type == RoleType::User && role.channel.as_deref() == Some("discord"))
        .and_then(|role| role.channel_config.as_ref())
        .and_then(DiscordBot::from_config)
}

impl TeamDaemon {
    pub(super) fn process_discord_queue(&mut self) -> Result<()> {
        self.poll_discord()?;
        self.sync_discord_events()?;
        self.sync_discord_board();
        self.deliver_user_channel_inbox()
    }

    fn poll_discord(&mut self) -> Result<()> {
        if self.discord_bot.is_none() {
            return Ok(());
        }

        let messages = match self
            .discord_bot
            .as_mut()
            .expect("checked discord bot presence")
            .poll_commands()
        {
            Ok(messages) => messages,
            Err(error) => {
                debug!(error = %error, "discord poll failed");
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
            .filter(|role| {
                role.role_type == RoleType::User && role.channel.as_deref() == Some("discord")
            })
            .flat_map(|role| role.talks_to.iter().cloned())
            .collect();

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "discord inbound"
            );

            if let Some(reply) = self.handle_discord_command(&msg.text) {
                if let Some(bot) = self.discord_bot.as_ref() {
                    if let Err(error) = bot.send_command_reply(&reply) {
                        warn!(error = %error, "failed to send discord command reply");
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
                        "failed to deliver discord message to inbox"
                    );
                }
            }

            self.record_message_routed("human", "discord");
        }

        Ok(())
    }

    fn handle_discord_command(&mut self, text: &str) -> Option<String> {
        let command = match parse_discord_command(text) {
            Ok(Some(command)) => command,
            Ok(None) => return None,
            Err(error) => return Some(error.to_string()),
        };

        Some(match self.execute_telegram_command(command) {
            Ok(reply) => reply,
            Err(error) => format!("Command failed: {error}"),
        })
    }

    fn sync_discord_events(&mut self) -> Result<()> {
        if self.discord_bot.is_none() {
            return Ok(());
        }

        let event_path = self.event_sink.path().to_path_buf();
        let events = read_events(&event_path)
            .with_context(|| format!("failed to read event log {}", event_path.display()))?;

        if events.len() < self.discord_event_cursor {
            self.discord_event_cursor = 0;
        }

        if self.discord_event_cursor >= events.len() {
            return Ok(());
        }

        // Rate limit: send at most 5 events per sync cycle to avoid Discord 429s.
        let batch_limit = 5;
        let mut sent = 0;
        for event in events.iter().skip(self.discord_event_cursor) {
            if sent >= batch_limit {
                break;
            }
            if is_telemetry_only_event(event) {
                sent += 1;
                continue;
            }
            if let Err(error) = self.send_discord_event(event) {
                tracing::warn!(error = %error, "discord event send failed; will retry next cycle");
                break;
            }
            sent += 1;
        }
        self.discord_event_cursor += sent;
        Ok(())
    }

    fn send_discord_event(&mut self, event: &TeamEvent) -> Result<()> {
        let Some(bot) = self.discord_bot.as_ref() else {
            return Ok(());
        };
        let Some(config) = discord_channel_config(&self.config.team_config) else {
            return Ok(());
        };
        // Skip noisy daemon internals — only send events humans care about.
        if is_noise_event(event) {
            return Ok(());
        }

        let Some(channel_id) = event_channel_id(config, event).map(str::to_string) else {
            return Ok(());
        };

        let embed = build_event_embed(event);
        bot.send_rich_embed(&channel_id, &embed)?;
        self.record_discord_event_sent(&channel_id, &event.event);
        Ok(())
    }

    fn sync_discord_board(&mut self) {
        let Some(bot) = self.discord_bot.as_ref() else {
            return;
        };
        let Some(config) = discord_channel_config(&self.config.team_config) else {
            return;
        };
        let Some(channel_id) = config.board_channel_id.as_deref() else {
            return;
        };
        if self
            .intervention_cooldowns
            .get(DISCORD_BOARD_SYNC_KEY)
            .is_some_and(|last| last.elapsed() < DISCORD_BOARD_SYNC_INTERVAL)
        {
            return;
        }

        let body = match build_board_message_body(
            &self.config.project_root,
            &self.config.members,
            &self.states,
            &self.backend_health,
        ) {
            Ok(body) => body,
            Err(error) => {
                warn!(error = %error, "failed to build Discord board payload");
                self.intervention_cooldowns
                    .insert(DISCORD_BOARD_SYNC_KEY.to_string(), Instant::now());
                return;
            }
        };

        let board_message_id_path = discord_board_message_id_path(&self.config.project_root);
        let stored_message_id = read_discord_board_message_id(&board_message_id_path);

        match stored_message_id {
            Some(message_id) => {
                if let Err(error) = bot.edit_message(channel_id, &message_id, &body) {
                    warn!(
                        channel_id,
                        message_id,
                        error = %error,
                        "failed to edit Discord board message; will retry next cycle"
                    );
                }
            }
            None => match bot.create_message(channel_id, &body) {
                Ok(message_id) => {
                    if let Err(error) =
                        write_discord_board_message_id(&board_message_id_path, &message_id)
                    {
                        warn!(error = %error, "failed to persist Discord board message id");
                    }
                    if let Err(error) = bot.pin_message(channel_id, &message_id) {
                        warn!(
                            channel_id,
                            message_id,
                            error = %error,
                            "failed to pin Discord board message"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        channel_id,
                        error = %error,
                        "failed to create Discord board message; will retry next cycle"
                    );
                }
            },
        }

        self.intervention_cooldowns
            .insert(DISCORD_BOARD_SYNC_KEY.to_string(), Instant::now());
    }
}

fn discord_board_message_id_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("discord_board_msg.txt")
}

fn read_discord_board_message_id(path: &Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn write_discord_board_message_id(path: &Path, message_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, format!("{message_id}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn build_board_message_body(
    project_root: &Path,
    members: &[MemberInstance],
    states: &HashMap<String, MemberState>,
    backend_health: &HashMap<String, crate::agent::BackendHealth>,
) -> Result<serde_json::Value> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks = load_tasks_from_dir(&board_dir.join("tasks"))?;
    let now_local = Local::now();

    let in_progress = summarize_in_progress_tasks(&tasks);
    let todo = summarize_todo_tasks(&tasks);
    let review = summarize_review_tasks(&tasks);
    let done_today = count_done_today(&tasks, now_local);
    let health_footer = build_health_footer(members, states, backend_health);

    let embeds = vec![
        serde_json::json!({
            "title": "Batty Board",
            "description": format!(
                "Live board dashboard for {}. Updates every 30 seconds.",
                project_root.display()
            ),
            "color": 0x334155u32,
        }),
        serde_json::json!({
            "title": format!("In Progress ({})", in_progress.total),
            "description": in_progress.rendered,
            "color": 0xE74C3Cu32,
        }),
        serde_json::json!({
            "title": format!("Todo ({})", todo.total),
            "description": todo.rendered,
            "color": 0x3498DBu32,
        }),
        serde_json::json!({
            "title": format!("Review ({})", review.total),
            "description": review.rendered,
            "color": 0xE67E22u32,
            "footer": {
                "text": format!(
                    "Done today: {} | {} | Updated {}",
                    done_today,
                    health_footer,
                    now_local.format("%Y-%m-%d %H:%M:%S %Z")
                )
            }
        }),
    ];

    Ok(serde_json::json!({
        "content": "",
        "embeds": embeds,
        "allowed_mentions": { "parse": [] }
    }))
}

struct BoardSection {
    total: usize,
    rendered: String,
}

fn summarize_in_progress_tasks(tasks: &[Task]) -> BoardSection {
    let mut tasks = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| (priority_rank(&task.priority), task.id));

    let lines = tasks
        .iter()
        .take(DISCORD_BOARD_MAX_SECTION_LINES)
        .map(|task| {
            let owner = task.claimed_by.as_deref().unwrap_or("unclaimed");
            let elapsed = task
                .claimed_at
                .as_deref()
                .and_then(format_elapsed_since_rfc3339)
                .unwrap_or_else(|| "-".to_string());
            format!(
                "#{} {} {} · {} · {}",
                task.id,
                priority_icon(&task.priority),
                task.title,
                owner,
                elapsed
            )
        })
        .collect::<Vec<_>>();

    BoardSection {
        total: tasks.len(),
        rendered: render_section_lines(lines, tasks.len()),
    }
}

fn summarize_todo_tasks(tasks: &[Task]) -> BoardSection {
    let mut tasks = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| (priority_rank(&task.priority), task.id));

    let lines = tasks
        .iter()
        .take(DISCORD_BOARD_MAX_SECTION_LINES)
        .map(|task| {
            format!(
                "#{} {} {} · {}",
                task.id,
                priority_icon(&task.priority),
                task.title,
                task.priority
            )
        })
        .collect::<Vec<_>>();

    BoardSection {
        total: tasks.len(),
        rendered: render_section_lines(lines, tasks.len()),
    }
}

fn summarize_review_tasks(tasks: &[Task]) -> BoardSection {
    let mut tasks = tasks
        .iter()
        .filter(|task| task.status == "review")
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| (priority_rank(&task.priority), task.id));

    let lines = tasks
        .iter()
        .take(DISCORD_BOARD_MAX_SECTION_LINES)
        .map(|task| {
            let owner = task.review_owner.as_deref().unwrap_or("unassigned");
            format!(
                "#{} {} {} · {}",
                task.id,
                priority_icon(&task.priority),
                task.title,
                owner
            )
        })
        .collect::<Vec<_>>();

    BoardSection {
        total: tasks.len(),
        rendered: render_section_lines(lines, tasks.len()),
    }
}

fn render_section_lines(lines: Vec<String>, total: usize) -> String {
    if lines.is_empty() {
        return "None".to_string();
    }

    let shown = lines.len();
    let mut rendered = lines.join("\n");
    if total > shown {
        rendered.push_str(&format!("\n... +{} more", total - shown));
    }
    rendered
}

fn build_health_footer(
    members: &[MemberInstance],
    states: &HashMap<String, MemberState>,
    backend_health: &HashMap<String, crate::agent::BackendHealth>,
) -> String {
    let (architect_active, architect_total) =
        role_activity_counts(members, states, RoleType::Architect);
    let (manager_active, manager_total) = role_activity_counts(members, states, RoleType::Manager);
    let (engineer_active, engineer_total) =
        role_activity_counts(members, states, RoleType::Engineer);
    let unhealthy = backend_health
        .values()
        .filter(|health| !health.is_healthy())
        .count();

    format!(
        "Architects: {}/{} active | Managers: {}/{} active | Engineers: {}/{} active | Backend warnings: {}",
        architect_active,
        architect_total,
        manager_active,
        manager_total,
        engineer_active,
        engineer_total,
        unhealthy
    )
}

fn role_activity_counts(
    members: &[MemberInstance],
    states: &HashMap<String, MemberState>,
    role_type: RoleType,
) -> (usize, usize) {
    let members = members
        .iter()
        .filter(|member| member.role_type == role_type)
        .collect::<Vec<_>>();
    let active = members
        .iter()
        .filter(|member| states.get(&member.name) == Some(&MemberState::Working))
        .count();
    (active, members.len())
}

fn count_done_today(tasks: &[Task], now: DateTime<Local>) -> usize {
    let today = now.date_naive();
    tasks
        .iter()
        .filter(|task| task.status == "done")
        .filter_map(|task| task.completed.as_deref())
        .filter_map(|completed| DateTime::parse_from_rfc3339(completed).ok())
        .filter(|completed| completed.with_timezone(&Local).date_naive() == today)
        .count()
}

fn format_elapsed_since_rfc3339(value: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    let elapsed = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
    if elapsed.num_seconds() < 0 {
        return None;
    }
    let secs = elapsed.num_seconds() as u64;
    Some(format_duration_compact(secs))
}

fn format_duration_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn priority_rank(priority: &str) -> u8 {
    match priority.to_ascii_lowercase().as_str() {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn priority_icon(priority: &str) -> &'static str {
    match priority.to_ascii_lowercase().as_str() {
        "critical" => "⚡",
        "high" => "🔧",
        "medium" => "📐",
        "low" => "📝",
        _ => "•",
    }
}

fn is_telemetry_only_event(event: &TeamEvent) -> bool {
    matches!(
        event.event.as_str(),
        "discord_event_sent" | "notification_delivery_sample"
    )
}

pub(crate) fn build_shutdown_snapshot(
    project_root: &Path,
    summary: Option<&crate::team::session::SessionSummary>,
) -> DiscordShutdownSnapshot {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap_or_default();

    DiscordShutdownSnapshot {
        tasks_completed: summary.map(|summary| summary.tasks_completed).unwrap_or(0),
        tasks_merged: summary.map(|summary| summary.tasks_merged).unwrap_or(0),
        runtime_secs: summary.map(|summary| summary.runtime_secs),
        in_progress: tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
            .count(),
        todo: tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
            .count(),
        review: tasks.iter().filter(|task| task.status == "review").count(),
        last_test_health: latest_test_health(&tasks),
    }
}

pub(crate) fn send_discord_shutdown_notice(
    team_config: &TeamConfig,
    snapshot: &DiscordShutdownSnapshot,
) -> Result<()> {
    let Some(bot) = build_discord_bot(team_config) else {
        return Ok(());
    };
    let Some(config) = discord_channel_config(team_config) else {
        return Ok(());
    };
    let Some(channel_id) = config.commands_channel_id.as_deref() else {
        return Ok(());
    };

    let runtime = snapshot
        .runtime_secs
        .map(crate::team::session::format_runtime)
        .unwrap_or_else(|| "unknown".to_string());
    let description = format!(
        "{} tasks in-progress, {} in review\nTasks completed: {}\nMerged: {}\nRuntime: {}",
        snapshot.in_progress,
        snapshot.review,
        snapshot.tasks_completed,
        snapshot.tasks_merged,
        runtime
    );
    bot.send_embed(channel_id, "🔴 Batty shutting down", &description, 0xDC2626)
}

pub(crate) fn send_discord_shutdown_summary(
    team_config: &TeamConfig,
    snapshot: &DiscordShutdownSnapshot,
) -> Result<()> {
    let Some(bot) = build_discord_bot(team_config) else {
        return Ok(());
    };
    let Some(config) = discord_channel_config(team_config) else {
        return Ok(());
    };
    let Some(channel_id) = config.events_channel_id.as_deref() else {
        return Ok(());
    };

    let runtime = snapshot
        .runtime_secs
        .map(crate::team::session::format_runtime)
        .unwrap_or_else(|| "unknown".to_string());
    let description = format!(
        "{} tasks completed, {} merged, runtime {}\nBoard: {} in-progress, {} todo, {} review\nTest health: {}",
        snapshot.tasks_completed,
        snapshot.tasks_merged,
        runtime,
        snapshot.in_progress,
        snapshot.todo,
        snapshot.review,
        snapshot.last_test_health
    );
    bot.send_embed(channel_id, "Session ended", &description, 0x2563EB)
}

fn discord_channel_config(team_config: &TeamConfig) -> Option<&ChannelConfig> {
    team_config
        .roles
        .iter()
        .find(|role| role.role_type == RoleType::User && role.channel.as_deref() == Some("discord"))
        .and_then(|role| role.channel_config.as_ref())
}

fn event_channel_id<'a>(config: &'a ChannelConfig, event: &TeamEvent) -> Option<&'a str> {
    // Route by event kind:
    //  - Agent lifecycle (spawned / started / stalled / context exhausted /
    //    pattern detected) → agents channel. These are "what are the
    //    members doing right now?" signals.
    //  - Everything else (task lifecycle, escalations, merges, verification,
    //    auto-doctor) → events channel. Yes, this includes alerts: the
    //    events channel is the main timeline and users filter by embed
    //    color. The commands channel is reserved for user-typed command
    //    responses so it stays scannable as a chat with the bot.
    //
    //  Prior routing sent "attention events" (escalations, errors) to the
    //  commands channel, which mixed alerts into command responses and
    //  broke the "this channel is my chat with the bot" model. Restored to
    //  events-channel routing as part of the Discord formatting overhaul.
    if is_agent_event(event) {
        config
            .agents_channel_id
            .as_deref()
            .or(config.events_channel_id.as_deref())
    } else {
        config.events_channel_id.as_deref()
    }
}

/// Events that are daemon internals — not interesting to a human reading Discord.
fn is_noise_event(event: &TeamEvent) -> bool {
    matches!(
        event.event.as_str(),
        "daemon_heartbeat"
            | "message_routed"
            | "state_reconciliation"
            | "task_claim_extended"
            | "task_claim_progress"
            | "task_claim_warning"
            | "loop_step_error"
            | "worktree_refreshed"
            | "board_task_archived" // dispatch_overlap_skipped is kept — explains why tasks aren't being assigned
    )
}

fn is_agent_event(event: &TeamEvent) -> bool {
    matches!(
        event.event.as_str(),
        "agent_spawned"
            | "daemon_started"
            | "daemon_stopped"
            | "context_exhausted"
            | "stall_detected"
            | "narration_rejection"
            | "pattern_detected"
            | "backend_quota_exhausted"
    )
}

/// Build a fully-structured [`RichEmbed`] for a `TeamEvent`.
///
/// This is the new canonical entrypoint — it replaces the old
/// title/description/color triple with an author block, severity-based
/// color, structured fields per event type, an embed-level timestamp,
/// and a provenance footer. Each event kind is handled in its own arm
/// so field layout can be tuned per type.
fn build_event_embed(event: &TeamEvent) -> RichEmbed {
    let severity = severity_for_event(&event.event);
    let color = severity.color();
    let title = event_title(event);
    let description = event_summary_line(event);
    let mut embed = RichEmbed::new(title, color).with_timestamp(event_timestamp_rfc3339(event));

    if let Some(description) = description {
        embed = embed.with_description(description);
    }

    if let Some(author) = event_author_label(event) {
        embed = embed.with_author(author);
    }

    for field in event_fields(event) {
        embed = embed.push_field(field);
    }

    embed = embed.with_footer(event_footer(event, severity));

    embed
}

/// Short, scannable title for an event embed. The old formatter
/// crammed `⚙️ System — 📌 Task Assigned` into every title; the new
/// format moves the role attribution into the author block and keeps
/// the title focused on "what happened". One leading emoji, a short
/// verb phrase, plus a task id when relevant.
fn event_title(event: &TeamEvent) -> String {
    let action = event_action_label(&event.event);
    if let Some(task) = event
        .task
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // task may be "409" or "409\nTask title\n..." — show just the id.
        let task_id = task.split_whitespace().next().unwrap_or(task);
        // Strip leading '#' if present so we don't render '##409'.
        let task_id = task_id.trim_start_matches('#');
        format!("{action} — #{task_id}")
    } else {
        action.to_string()
    }
}

/// Canonical `emoji + verb` action label for an event kind. Does NOT
/// include any role prefix — those live in the author block now.
fn event_action_label(event: &str) -> String {
    match event {
        "task_assigned" => "📌 Task Assigned".into(),
        "task_claim_created" => "✋ Task Claimed".into(),
        "task_escalated" => "🚨 Task Escalated".into(),
        "task_stale" => "⏰ Task Stale".into(),
        "task_auto_merged" | "task_manual_merged" | "merge_success" => "✅ Task Merged".into(),
        "verification_phase_changed" => "🔍 Verification".into(),
        "verification_evidence_collected" => "🧪 Tests Passed".into(),
        "verification_failed" => "❌ Verification Failed".into(),
        "agent_spawned" => "🚀 Agent Started".into(),
        "daemon_started" => "🟢 Batty Started".into(),
        "daemon_stopped" => "🔴 Batty Stopped".into(),
        "stall_detected" => "🐌 Agent Stalled".into(),
        "context_exhausted" => "🧠 Context Exhausted".into(),
        "narration_rejection" => "🚫 Narration Rejected".into(),
        "backend_quota_exhausted" => "💳 Quota Exhausted".into(),
        "auto_doctor_action" => "🩺 Auto-Doctor".into(),
        "pattern_detected" => "📊 Pattern Detected".into(),
        "dispatch_overlap_skipped" => "⏸️ Dispatch Skipped".into(),
        "scope_fence_violation" => "⛔ Scope Violation".into(),
        "shim_crash" | "pane_death" => "💥 Agent Crashed".into(),
        other => other.replace('_', " "),
    }
}

/// One- or two-sentence narrative description. Optional — not every
/// event has something useful to say beyond its structured fields.
fn event_summary_line(event: &TeamEvent) -> Option<String> {
    match event.event.as_str() {
        "task_assigned" => {
            let engineer = event_actor_label(event);
            let title = event
                .task
                .as_deref()
                .and_then(|t| t.split_once('\n').map(|(first, _)| first).or(Some(t)))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("new task");
            let title = truncate_plain(title, 120);
            Some(format!("**{engineer}** picked up **{title}**."))
        }
        "task_escalated" => {
            let from = event_actor_label(event);
            let reason = event.reason.as_deref().unwrap_or("no reason given");
            Some(format!("Escalated by **{from}** — {reason}."))
        }
        "task_stale" => {
            let role = event_actor_label(event);
            let reason = event.reason.as_deref().unwrap_or("no progress detected");
            Some(format!("**{role}** is stuck — {reason}."))
        }
        "agent_spawned" => {
            let role = event_actor_label(event);
            Some(format!("**{role}** is online and ready for work."))
        }
        "daemon_started" => {
            let uptime = event
                .uptime_secs
                .map(|s| format!(" (uptime {s}s)"))
                .unwrap_or_default();
            Some(format!("Team is running{uptime}."))
        }
        "daemon_stopped" => Some("Team session ended.".into()),
        "stall_detected" => {
            let role = event_actor_label(event);
            let reason = event.reason.as_deref().unwrap_or("unresponsive");
            Some(format!("**{role}** appears stuck — {reason}."))
        }
        "context_exhausted" => {
            let role = event_actor_label(event);
            Some(format!(
                "**{role}** hit the context limit. Restarting with handoff."
            ))
        }
        "narration_rejection" => {
            let role = event_actor_label(event);
            Some(format!(
                "**{role}** tried to narrate instead of code. Retrying."
            ))
        }
        "backend_quota_exhausted" => {
            let role = event_actor_label(event);
            let reason = event.reason.as_deref().unwrap_or("credits exhausted");
            Some(format!(
                "**{role}** hit backend quota. Agent paused — {reason}."
            ))
        }
        "pattern_detected" => {
            let pattern = event
                .details
                .as_deref()
                .or(event.reason.as_deref())
                .unwrap_or("rolling-window threshold tripped");
            Some(truncate_plain(pattern, 240))
        }
        "task_auto_merged" | "task_manual_merged" | "merge_success" => {
            let role = event_actor_label(event);
            Some(format!("**{role}** landed the change on main."))
        }
        "verification_phase_changed" => {
            let step = event.step.as_deref().unwrap_or("state change");
            Some(format!("Phase → **{step}**."))
        }
        "verification_evidence_collected" => Some(
            event
                .details
                .clone()
                .unwrap_or_else(|| "Tests passed.".into()),
        ),
        "dispatch_overlap_skipped" => {
            let blocking = event.reason.as_deref().unwrap_or("another in-flight task");
            Some(format!("Skipped — conflicts with {blocking}."))
        }
        "auto_doctor_action" => event.details.clone(),
        _ => None,
    }
}

/// Structured fields per event type. This is where the bulk of the
/// useful information lives — one labelled inline field per key piece
/// of context.
fn event_fields(event: &TeamEvent) -> Vec<EmbedField> {
    let mut fields = Vec::new();

    match event.event.as_str() {
        "task_assigned" => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(engineer) = event.to.as_deref().or(event.recipient.as_deref()) {
                fields.push(EmbedField::inline("Engineer", engineer.to_string()));
            }
            if let Some(from) = event.from.as_deref() {
                fields.push(EmbedField::inline("Assigned By", from.to_string()));
            }
            if let Some(body) = task_body_preview(event) {
                fields.push(EmbedField::new("Task Body", body));
            }
        }
        "task_escalated" => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(from) = event.from.as_deref() {
                fields.push(EmbedField::inline("From", from.to_string()));
            }
            if let Some(to) = event.to.as_deref() {
                fields.push(EmbedField::inline("To", to.to_string()));
            }
            if let Some(reason) = event.reason.as_deref() {
                fields.push(EmbedField::new("Reason", format!("> {reason}")));
            }
        }
        "verification_phase_changed" => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Engineer", role.to_string()));
            }
            if let Some(step) = event.step.as_deref() {
                fields.push(EmbedField::inline("Phase", step.to_string()));
            }
        }
        "agent_spawned" | "daemon_started" | "daemon_stopped" => {
            if let Some(backend) = event.backend.as_deref() {
                fields.push(EmbedField::inline("Backend", backend.to_string()));
            }
            if let Some(restart) = event.restart {
                fields.push(EmbedField::inline(
                    "Restart",
                    if restart { "yes" } else { "no" }.to_string(),
                ));
            }
            if let Some(uptime) = event.uptime_secs {
                fields.push(EmbedField::inline("Uptime", format!("{uptime}s")));
            }
        }
        "stall_detected" | "context_exhausted" | "narration_rejection" => {
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Agent", role.to_string()));
            }
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(reason) = event.reason.as_deref() {
                fields.push(EmbedField::new("Details", format!("> {reason}")));
            }
        }
        "pattern_detected" => {
            if let Some(pattern) = event.reason.as_deref().or(event.details.as_deref()) {
                fields.push(EmbedField::new("Pattern", pattern.to_string()));
            }
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Agent", role.to_string()));
            }
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
        }
        "backend_quota_exhausted" => {
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Agent", role.to_string()));
            }
            if let Some(backend) = event.backend.as_deref() {
                fields.push(EmbedField::inline("Backend", backend.to_string()));
            }
            if let Some(reason) = event.reason.as_deref() {
                fields.push(EmbedField::new("Reason", format!("> {reason}")));
            }
        }
        "task_auto_merged" | "task_manual_merged" | "merge_success" => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Engineer", role.to_string()));
            }
            if let Some(mode) = event.merge_mode.as_deref() {
                fields.push(EmbedField::inline("Mode", mode.to_string()));
            }
        }
        "auto_doctor_action" => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Target", role.to_string()));
            }
            if let Some(details) = event.details.as_deref() {
                fields.push(EmbedField::new("Action", details.to_string()));
            }
        }
        _ => {
            if let Some(task_id) = event.task.as_deref().and_then(extract_task_id) {
                fields.push(EmbedField::inline("Task", format!("#{task_id}")));
            }
            if let Some(role) = event.role.as_deref() {
                fields.push(EmbedField::inline("Member", role.to_string()));
            }
            if let Some(reason) = event.reason.as_deref() {
                fields.push(EmbedField::new("Reason", format!("> {reason}")));
            }
            if let Some(error) = event.error.as_deref() {
                fields.push(EmbedField::new("Error", format!("⚠️ {error}")));
            }
        }
    }

    fields
}

/// Author label shown in the embed author block. Maps to role with a
/// consistent emoji prefix.
fn event_author_label(event: &TeamEvent) -> Option<String> {
    event
        .role
        .as_deref()
        .or(event.from.as_deref())
        .or(event.to.as_deref())
        .map(role_author_label)
}

/// Short label for the actor in narrative sentences — prefers the
/// most specific source available. Never returns `?`.
fn event_actor_label(event: &TeamEvent) -> String {
    event
        .to
        .as_deref()
        .or(event.role.as_deref())
        .or(event.from.as_deref())
        .map(|r| r.to_string())
        .unwrap_or_else(|| "system".into())
}

/// Extract a clean `\d+` task id from the `task` field, which is often
/// `"409"` but sometimes `"409\nTitle..."` or `"#409"`.
fn extract_task_id(raw: &str) -> Option<String> {
    let token = raw.split_whitespace().next()?.trim_start_matches('#');
    let digits: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Extract a short preview of the task body (the part after the first
/// line) for an embed field. Truncates to 900 chars so the field
/// stays well inside Discord's 1024-char field value limit.
fn task_body_preview(event: &TeamEvent) -> Option<String> {
    let task = event.task.as_deref()?;
    let (_, body) = task.split_once('\n')?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_plain(trimmed, 900))
}

/// UTF-8-safe truncation with an ellipsis suffix when we cut.
fn truncate_plain(input: &str, limit: usize) -> String {
    if input.chars().count() <= limit {
        return input.to_string();
    }
    let mut out: String = input.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Convert the event's unix-epoch `ts` to an ISO 8601 RFC 3339 string
/// so Discord can render it client-local next to the footer.
fn event_timestamp_rfc3339(event: &TeamEvent) -> String {
    DateTime::<Utc>::from_timestamp(event.ts as i64, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

/// Consistent footer: provenance + severity tag. Users scanning the
/// channel can tell "which service wrote this" without reading the
/// author line.
fn event_footer(event: &TeamEvent, severity: Severity) -> String {
    let tag = match severity {
        Severity::Success => "SUCCESS",
        Severity::Info => "INFO",
        Severity::Warn => "WARN",
        Severity::Error => "ERROR",
        Severity::Critical => "CRIT",
        Severity::Neutral => "INFO",
    };
    format!(
        "batty v{} · {} · {}",
        env!("CARGO_PKG_VERSION"),
        tag,
        event.event
    )
}

fn latest_test_health(tasks: &[Task]) -> String {
    let mut tasks = tasks.iter().collect::<Vec<_>>();
    tasks.sort_by(|left, right| right.last_progress_at.cmp(&left.last_progress_at));

    for task in tasks {
        let Ok(metadata) = crate::team::board::read_workflow_metadata(&task.source_path) else {
            continue;
        };
        if let Some(results) = metadata.test_results {
            return format!(
                "{}: {} passed, {} failed, {} ignored",
                results.framework, results.passed, results.failed, results.ignored
            );
        }
        if metadata.tests_passed == Some(true) {
            return "tests passing".to_string();
        }
        if metadata.tests_run == Some(true) && metadata.tests_passed == Some(false) {
            return "tests failing".to_string();
        }
    }

    "unknown".to_string()
}

fn parse_discord_command(text: &str) -> Result<Option<TelegramCommand>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('$') {
        return Ok(None);
    }

    let (name, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(name, rest)| (name, rest.trim()))
        .unwrap_or((trimmed, ""));

    match name {
        "$status" => Ok(Some(TelegramCommand::Status)),
        "$board" => Ok(Some(TelegramCommand::Board {
            filter: (!rest.is_empty()).then(|| rest.to_string()),
        })),
        "$logs" => {
            if rest.is_empty() {
                bail!("usage: $logs <member>");
            }
            Ok(Some(TelegramCommand::Logs {
                member: rest.to_string(),
            }))
        }
        "$health" => Ok(Some(TelegramCommand::Health)),
        "$assign" => {
            let (engineer, task) = split_two_part_command(rest, "$assign <engineer> <task>")?;
            Ok(Some(TelegramCommand::Assign { engineer, task }))
        }
        "$merge" => Ok(Some(TelegramCommand::Merge {
            task_id: parse_task_id_token(rest)?,
        })),
        "$kick" => {
            if rest.is_empty() {
                bail!("usage: $kick <member>");
            }
            Ok(Some(TelegramCommand::Kick {
                member: rest.to_string(),
            }))
        }
        "$pause" => Ok(Some(TelegramCommand::Pause)),
        "$resume" => Ok(Some(TelegramCommand::Resume)),
        "$goal" => {
            if rest.is_empty() {
                bail!("usage: $goal <text>");
            }
            Ok(Some(TelegramCommand::Goal {
                text: rest.to_string(),
            }))
        }
        "$task" => {
            if rest.is_empty() {
                bail!("usage: $task <title>");
            }
            Ok(Some(TelegramCommand::Task {
                title: rest.to_string(),
            }))
        }
        "$block" => {
            let (task, reason) = split_two_part_command(rest, "$block <task_id> <reason>")?;
            Ok(Some(TelegramCommand::Block {
                task_id: parse_task_id_token(&task)?,
                reason,
            }))
        }
        "$stop" => Ok(Some(TelegramCommand::Stop { confirm: true })),
        "$go" => Ok(Some(TelegramCommand::Start)),
        "$help" => Ok(Some(TelegramCommand::Help)),
        "$send" => {
            let (role, message) = split_two_part_command(rest, "$send <role> <message>")?;
            Ok(Some(TelegramCommand::Send { role, message }))
        }
        other => Err(anyhow!("unknown Discord command: {other}")),
    }
}

fn split_two_part_command(input: &str, usage: &str) -> Result<(String, String)> {
    let (first, rest) = input
        .split_once(char::is_whitespace)
        .map(|(first, rest)| (first.trim(), rest.trim()))
        .filter(|(first, rest)| !first.is_empty() && !rest.is_empty())
        .ok_or_else(|| anyhow!("usage: {usage}"))?;
    Ok((first.to_string(), rest.to_string()))
}

fn parse_task_id_token(input: &str) -> Result<u32> {
    let trimmed = input.trim().trim_start_matches('#');
    if trimmed.is_empty() {
        bail!("missing task id");
    }
    trimmed
        .parse::<u32>()
        .with_context(|| format!("invalid task id '{input}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_discord_command_supports_walkaway_aliases() {
        assert_eq!(
            parse_discord_command("$go").unwrap(),
            Some(TelegramCommand::Start)
        );
        assert_eq!(
            parse_discord_command("$status").unwrap(),
            Some(TelegramCommand::Status)
        );
        assert_eq!(
            parse_discord_command("$board review").unwrap(),
            Some(TelegramCommand::Board {
                filter: Some("review".to_string())
            })
        );
        assert_eq!(
            parse_discord_command("$stop").unwrap(),
            Some(TelegramCommand::Stop { confirm: true })
        );
    }

    #[test]
    fn parse_discord_command_parses_send_and_assign() {
        assert_eq!(
            parse_discord_command("$send architect Focus on stability").unwrap(),
            Some(TelegramCommand::Send {
                role: "architect".to_string(),
                message: "Focus on stability".to_string(),
            })
        );
        assert_eq!(
            parse_discord_command("$assign eng-1 Task #41: fix flakes").unwrap(),
            Some(TelegramCommand::Assign {
                engineer: "eng-1".to_string(),
                task: "Task #41: fix flakes".to_string(),
            })
        );
    }

    #[test]
    fn parse_discord_command_rejects_invalid_usage() {
        assert!(parse_discord_command("$assign eng-1").is_err());
        assert!(parse_discord_command("$merge nope").is_err());
        assert!(parse_discord_command("$unknown").is_err());
        assert_eq!(parse_discord_command("focus on quality").unwrap(), None);
    }

    #[test]
    fn event_channel_id_routes_agent_lifecycle_to_agents_and_everything_else_to_events() {
        // Regression test for the channel-routing fix: attention / error
        // events used to land in the commands channel, which mixed alerts
        // into user command responses. New rule is strictly two-way —
        // agent lifecycle → agents channel, everything else → events
        // channel. Commands channel is reserved for user command replies.
        let config = crate::team::config::ChannelConfig {
            target: String::new(),
            provider: String::new(),
            bot_token: Some("token".into()),
            allowed_user_ids: vec![42],
            events_channel_id: Some("events".into()),
            agents_channel_id: Some("agents".into()),
            commands_channel_id: Some("commands".into()),
            board_channel_id: Some("board".into()),
        };

        // Error / escalation events belong on the main events timeline,
        // NOT on the user-command channel.
        let mut error_event = TeamEvent::loop_step_error("poll", "boom");
        error_event.role = Some("manager".into());
        assert_eq!(event_channel_id(&config, &error_event), Some("events"));

        // Agent lifecycle events belong on the agents channel.
        let agent_event = TeamEvent::daemon_started();
        assert_eq!(event_channel_id(&config, &agent_event), Some("agents"));

        // Routine task events belong on the events channel.
        let board_event = TeamEvent::task_assigned("eng-1", "Task #42");
        assert_eq!(event_channel_id(&config, &board_event), Some("events"));

        // Task escalations are alerts — used to go to commands, must now
        // land on events so the commands channel stays as a chat surface.
        let escalation = TeamEvent::task_escalated("manager", "42", Some("stuck_task"));
        assert_eq!(event_channel_id(&config, &escalation), Some("events"));
    }

    #[test]
    fn build_event_embed_promotes_role_to_author_and_uses_fields() {
        // Regression test for the "wall of text" embed bug: task_assigned
        // embeds used to be title + 3800-char description with no
        // structure, so readers scrolled past a single mega-post per
        // task. The new builder moves the role to `author`, keeps the
        // description to a single short sentence, and exposes the task
        // id / engineer / body in structured fields.
        let mut event = TeamEvent::task_assigned(
            "alex-dev-1",
            "409\nBuild routing fixtures for the marketing pipeline\nThis task prepares the fixture tree used by the router tests.",
        );
        event.to = Some("alex-dev-1".into());
        event.from = Some("jordan-pm".into());

        let embed = build_event_embed(&event);

        // Severity maps to Info for task_assigned → Discord Blurple.
        assert_eq!(embed.color, Severity::Info.color());

        // Title should NOT carry the role prefix anymore; it only
        // describes the action + task id.
        assert!(embed.title.starts_with("📌 Task Assigned"));
        assert!(embed.title.contains("#409"));
        assert!(!embed.title.contains("System"));

        // Author block should carry the role attribution.
        let author = embed.author_name.as_deref().unwrap_or_default();
        assert!(
            author.contains("alex-dev-1") || author.contains("jordan-pm"),
            "author block should attribute the event, got {author:?}"
        );

        // Description should be a single short narrative sentence, not a
        // 3800-char dump of the task body.
        let description = embed.description.as_deref().unwrap_or_default();
        assert!(
            description.len() < 200,
            "description too long: {description:?}"
        );
        assert!(description.contains("alex-dev-1"));
        assert!(!description.contains("**?**"));

        // Fields should carry the structured data.
        let field_names: Vec<&str> = embed.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(field_names.contains(&"Task"));
        assert!(field_names.contains(&"Engineer"));
        assert!(field_names.contains(&"Assigned By"));
        assert!(field_names.contains(&"Task Body"));

        // Footer should include version + severity tag + event kind.
        let footer = embed.footer.as_deref().unwrap_or_default();
        assert!(footer.contains("batty v"));
        assert!(footer.contains("INFO"));
        assert!(footer.contains("task_assigned"));

        // Embed must have an ISO 8601 timestamp so Discord renders it
        // in the viewer's local timezone.
        let timestamp = embed.timestamp.as_deref().unwrap_or_default();
        assert!(
            timestamp.contains('T'),
            "timestamp should be RFC3339: {timestamp:?}"
        );
    }

    #[test]
    fn build_event_embed_task_escalated_uses_error_color_and_reason_field() {
        // Regression test for the `**?**` bug and the color/severity
        // taxonomy. task_escalated used to render red only because of a
        // generic "contains escalat" regex — and produced a description
        // that started with `**?** escalated **#NNN**` when the `from`
        // field was the only actor source. The new builder picks up
        // `from` explicitly, colors the embed with Severity::Error
        // (0xED4245), and puts the reason in its own field.
        let event = TeamEvent::task_escalated("jordan-pm", "256", Some("stuck_task"));

        let embed = build_event_embed(&event);

        assert_eq!(embed.color, Severity::Error.color());
        assert_eq!(embed.color, 0xED4245);
        assert!(embed.title.starts_with("🚨 Task Escalated"));
        assert!(embed.title.contains("#256"));
        let description = embed.description.as_deref().unwrap_or_default();
        assert!(
            !description.contains("**?**"),
            "description still has ? placeholder"
        );
        assert!(description.contains("jordan-pm"));

        let reason_field = embed
            .fields
            .iter()
            .find(|f| f.name == "Reason")
            .expect("escalation embed should carry a Reason field");
        assert!(reason_field.value.contains("stuck_task"));

        let footer = embed.footer.as_deref().unwrap_or_default();
        assert!(footer.contains("ERROR"));
    }

    #[test]
    fn build_event_embed_pattern_detected_uses_warn_color_not_plain_error() {
        // Pattern detection is advisory, not actionable — it should map
        // to Severity::Warn (yellow) so it visually separates from true
        // errors like task_escalated or stall_detected.
        let mut event = TeamEvent::pattern_detected("escalation_cluster", 5);
        event.role = Some("manager".into());

        let embed = build_event_embed(&event);
        assert_eq!(embed.color, Severity::Warn.color());
        assert_eq!(embed.color, 0xFEE75C);
        assert!(embed.title.starts_with("📊 Pattern Detected"));
        let footer = embed.footer.as_deref().unwrap_or_default();
        assert!(footer.contains("WARN"));
    }

    #[test]
    fn discord_board_message_id_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = discord_board_message_id_path(tmp.path());
        write_discord_board_message_id(&path, "123456789").unwrap();
        assert_eq!(
            read_discord_board_message_id(&path).as_deref(),
            Some("123456789")
        );
    }

    #[test]
    fn build_board_message_body_renders_sections_and_footer() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&board_dir).unwrap();
        std::fs::write(
            board_dir.join("561-live-board.md"),
            format!(
                "---\nid: 561\ntitle: Live board sync\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclaimed_at: {}\nclass: standard\n---\n\nTask.\n",
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();
        std::fs::write(
            board_dir.join("562-board-todo.md"),
            "---\nid: 562\ntitle: Board todo\nstatus: todo\npriority: critical\nclass: standard\n---\n\nTask.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("563-board-review.md"),
            "---\nid: 563\ntitle: Board review\nstatus: review\npriority: medium\nreview_owner: lead\nclass: standard\n---\n\nTask.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("564-board-done.md"),
            format!(
                "---\nid: 564\ntitle: Board done\nstatus: done\npriority: low\ncompleted: {}\nclass: standard\n---\n\nTask.\n",
                Local::now().to_rfc3339()
            ),
        )
        .unwrap();

        let members = vec![
            crate::team::harness::architect_member("architect"),
            crate::team::harness::manager_member("lead", Some("architect")),
            crate::team::harness::engineer_member("eng-1", Some("lead"), false),
        ];
        let states = HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Working),
        ]);
        let backend_health =
            HashMap::from([("eng-1".to_string(), crate::agent::BackendHealth::Healthy)]);

        let body =
            build_board_message_body(tmp.path(), &members, &states, &backend_health).unwrap();
        let embeds = body["embeds"].as_array().unwrap();
        assert_eq!(embeds.len(), 4);
        assert_eq!(embeds[0]["title"].as_str(), Some("Batty Board"));
        assert_eq!(embeds[1]["title"].as_str(), Some("In Progress (1)"));
        assert!(
            embeds[1]["description"]
                .as_str()
                .unwrap_or("")
                .contains("#561")
        );
        assert_eq!(embeds[2]["title"].as_str(), Some("Todo (1)"));
        assert!(
            embeds[2]["description"]
                .as_str()
                .unwrap_or("")
                .contains("critical")
        );
        assert_eq!(embeds[3]["title"].as_str(), Some("Review (1)"));
        let footer = embeds[3]["footer"]["text"].as_str().unwrap_or("");
        assert!(footer.contains("Done today: 1"));
        assert!(footer.contains("Engineers: 1/1 active"));
    }

    #[test]
    fn build_shutdown_snapshot_counts_board_state_and_test_health() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&board_dir).unwrap();
        std::fs::write(
            board_dir.join("567-stop.md"),
            format!(
                "---\nid: 567\ntitle: Stop flow\nstatus: in-progress\npriority: high\nlast_progress_at: {}\nclass: standard\ntests_run: true\ntests_passed: true\ntest_results:\n  framework: cargo\n  passed: 12\n  failed: 0\n  ignored: 1\n  failures: []\n---\n\nTask.\n",
                Local::now().to_rfc3339()
            ),
        )
        .unwrap();
        std::fs::write(
            board_dir.join("568-review.md"),
            "---\nid: 568\ntitle: Review flow\nstatus: review\npriority: medium\nclass: standard\n---\n\nTask.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("569-todo.md"),
            "---\nid: 569\ntitle: Todo flow\nstatus: todo\npriority: low\nclass: standard\n---\n\nTask.\n",
        )
        .unwrap();

        let summary = crate::team::session::SessionSummary {
            tasks_completed: 5,
            tasks_merged: 3,
            runtime_secs: 3600,
        };
        let snapshot = build_shutdown_snapshot(tmp.path(), Some(&summary));

        assert_eq!(snapshot.tasks_completed, 5);
        assert_eq!(snapshot.tasks_merged, 3);
        assert_eq!(snapshot.runtime_secs, Some(3600));
        assert_eq!(snapshot.in_progress, 1);
        assert_eq!(snapshot.todo, 1);
        assert_eq!(snapshot.review, 1);
        assert!(snapshot.last_test_health.contains("cargo: 12 passed"));
    }
}
