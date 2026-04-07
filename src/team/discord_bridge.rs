//! Discord bridge orchestration for the daemon poll loop.

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

use super::telegram_bridge::TelegramCommand;
use super::*;
use crate::team::config::{ChannelConfig, RoleType, TeamConfig};
use crate::team::discord::{DiscordBot, color_for_role};
use crate::team::events::{TeamEvent, read_events};
use crate::team::inbox;

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
            if let Err(error) = self.send_discord_event(event) {
                tracing::warn!(error = %error, "discord event send failed; will retry next cycle");
                break;
            }
            sent += 1;
        }
        self.discord_event_cursor += sent;
        Ok(())
    }

    fn send_discord_event(&self, event: &TeamEvent) -> Result<()> {
        let Some(bot) = self.discord_bot.as_ref() else {
            return Ok(());
        };
        let Some(config) = discord_channel_config(&self.config.team_config) else {
            return Ok(());
        };
        let Some(channel_id) = event_channel_id(config, event) else {
            return Ok(());
        };

        let title = event_title(event);
        let description = event_description(event);
        let color = event_color(event);
        bot.send_embed(channel_id, &title, &description, color)
    }
}

fn discord_channel_config(team_config: &TeamConfig) -> Option<&ChannelConfig> {
    team_config
        .roles
        .iter()
        .find(|role| role.role_type == RoleType::User && role.channel.as_deref() == Some("discord"))
        .and_then(|role| role.channel_config.as_ref())
}

fn event_channel_id<'a>(config: &'a ChannelConfig, event: &TeamEvent) -> Option<&'a str> {
    if is_attention_event(event) {
        config
            .commands_channel_id
            .as_deref()
            .or(config.events_channel_id.as_deref())
    } else if is_agent_event(event) {
        config
            .agents_channel_id
            .as_deref()
            .or(config.events_channel_id.as_deref())
    } else {
        config.events_channel_id.as_deref()
    }
}

fn is_attention_event(event: &TeamEvent) -> bool {
    let name = event.event.as_str();
    name.contains("error")
        || name.contains("failed")
        || name.contains("panic")
        || name.contains("escalat")
        || name.contains("blocked")
}

fn is_agent_event(event: &TeamEvent) -> bool {
    matches!(
        event.event.as_str(),
        "daemon_started"
            | "daemon_stopped"
            | "daemon_reloading"
            | "daemon_reloaded"
            | "context_exhausted"
    ) || event.event.starts_with("agent_")
}

fn event_title(event: &TeamEvent) -> String {
    event.event.replace('_', " ")
}

fn event_description(event: &TeamEvent) -> String {
    let mut lines = Vec::new();
    if let Some(role) = &event.role {
        lines.push(format!("Role: {role}"));
    }
    if let Some(task) = &event.task {
        lines.push(format!("Task: {task}"));
    }
    if let Some(from) = &event.from {
        lines.push(format!("From: {from}"));
    }
    if let Some(to) = &event.to {
        lines.push(format!("To: {to}"));
    }
    if let Some(recipient) = &event.recipient {
        lines.push(format!("Recipient: {recipient}"));
    }
    if let Some(reason) = &event.reason {
        lines.push(format!("Reason: {reason}"));
    }
    if let Some(step) = &event.step {
        lines.push(format!("Step: {step}"));
    }
    if let Some(error) = &event.error {
        lines.push(format!("Error: {error}"));
    }
    if let Some(details) = &event.details {
        lines.push(format!("Details: {details}"));
    }
    if let Some(backend) = &event.backend {
        lines.push(format!("Backend: {backend}"));
    }
    if let Some(success) = event.success {
        lines.push(format!("Success: {success}"));
    }
    if let Some(uptime_secs) = event.uptime_secs {
        lines.push(format!("Uptime: {uptime_secs}s"));
    }
    if lines.is_empty() {
        format!("Timestamp: {}", event.ts)
    } else {
        lines.join("\n")
    }
}

fn event_color(event: &TeamEvent) -> u32 {
    if is_attention_event(event) {
        0xDC2626
    } else if let Some(role) = event.role.as_deref() {
        color_for_role(role)
    } else if let Some(from) = event.from.as_deref() {
        color_for_role(from)
    } else {
        color_for_role("system")
    }
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
    fn event_channel_id_routes_attention_and_agent_events() {
        let config = crate::team::config::ChannelConfig {
            target: String::new(),
            provider: String::new(),
            bot_token: Some("token".into()),
            allowed_user_ids: vec![42],
            events_channel_id: Some("events".into()),
            agents_channel_id: Some("agents".into()),
            commands_channel_id: Some("commands".into()),
        };

        let mut error_event = TeamEvent::loop_step_error("poll", "boom");
        error_event.role = Some("manager".into());
        assert_eq!(event_channel_id(&config, &error_event), Some("commands"));

        let agent_event = TeamEvent::daemon_started();
        assert_eq!(event_channel_id(&config, &agent_event), Some("agents"));

        let board_event = TeamEvent::task_assigned("eng-1", "Task #42");
        assert_eq!(event_channel_id(&config, &board_event), Some("events"));
    }
}
