//! Native Discord Bot API client for batty.
//!
//! Uses Discord's HTTP API directly for outbound embeds and command-channel
//! polling, keeping the implementation aligned with the existing Telegram
//! bridge's blocking request model.

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, warn};

use super::config::ChannelConfig;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const MAX_EMBED_TITLE_LEN: usize = 256;
const MAX_EMBED_DESCRIPTION_LEN: usize = 4_000;
const MAX_CONTENT_LEN: usize = 2_000;

/// An inbound message received from Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub message_id: String,
    pub channel_id: String,
    pub from_user_id: i64,
    pub text: String,
}

/// Blocking Discord Bot API client.
pub struct DiscordBot {
    bot_token: String,
    allowed_user_ids: Vec<i64>,
    commands_channel_id: String,
    last_message_id: Option<String>,
}

impl DiscordBot {
    pub fn new(bot_token: String, allowed_user_ids: Vec<i64>, commands_channel_id: String) -> Self {
        Self {
            bot_token,
            allowed_user_ids,
            commands_channel_id,
            last_message_id: None,
        }
    }

    /// Build a `DiscordBot` from a `ChannelConfig`.
    ///
    /// Returns `None` if either the token or commands channel ID is missing.
    /// The token can be provided directly or via `BATTY_DISCORD_BOT_TOKEN`.
    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        let token = config
            .bot_token
            .clone()
            .or_else(|| std::env::var("BATTY_DISCORD_BOT_TOKEN").ok())?;
        let commands_channel_id = config.commands_channel_id.clone()?;
        Some(Self::new(
            token,
            config.allowed_user_ids.clone(),
            commands_channel_id,
        ))
    }

    pub fn commands_channel_id(&self) -> &str {
        &self.commands_channel_id
    }

    pub fn send_plain_message(&self, channel_id: &str, text: &str) -> Result<()> {
        let body = serde_json::json!({
            "content": truncate_for_discord(text, MAX_CONTENT_LEN),
            "allowed_mentions": { "parse": [] }
        });
        self.post_message(channel_id, &body)
    }

    pub fn send_embed(
        &self,
        channel_id: &str,
        title: &str,
        description: &str,
        color: u32,
    ) -> Result<()> {
        let body = serde_json::json!({
            "embeds": [{
                "title": truncate_for_discord(title, MAX_EMBED_TITLE_LEN),
                "description": truncate_for_discord(description, MAX_EMBED_DESCRIPTION_LEN),
                "color": color
            }],
            "allowed_mentions": { "parse": [] }
        });
        self.post_message(channel_id, &body)
    }

    pub fn send_command_reply(&self, text: &str) -> Result<()> {
        self.send_plain_message(&self.commands_channel_id, text)
    }

    pub fn send_formatted_message(&self, channel_id: &str, message: &str) -> Result<()> {
        let (title, description, color) = outbound_embed_parts(message);
        self.send_embed(channel_id, &title, &description, color)
    }

    pub fn poll_commands(&mut self) -> Result<Vec<InboundMessage>> {
        let url = match &self.last_message_id {
            Some(last_id) => format!(
                "{DISCORD_API_BASE}/channels/{}/messages?limit=100&after={last_id}",
                self.commands_channel_id
            ),
            None => format!(
                "{DISCORD_API_BASE}/channels/{}/messages?limit=100",
                self.commands_channel_id
            ),
        };

        let response = ureq::get(&url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .call();

        let json: serde_json::Value = match response {
            Ok(resp) => resp
                .into_json()
                .context("failed to parse Discord messages response")?,
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                warn!(status, detail = %detail, "Discord poll failed");
                bail!("Discord messages failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                warn!(error = %error, "Discord poll transport failed");
                bail!("Discord messages transport failed: {error}");
            }
        };

        let (messages, latest_message_id) = parse_messages_response(&json, &self.allowed_user_ids)?;
        if let Some(message_id) = latest_message_id {
            self.last_message_id = Some(message_id);
        }
        Ok(messages)
    }

    fn post_message(&self, channel_id: &str, body: &serde_json::Value) -> Result<()> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let response = ureq::post(&url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match response {
            Ok(resp) => {
                debug!(
                    status = resp.status(),
                    channel_id, "Discord message accepted"
                );
                Ok(())
            }
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                warn!(status, detail = %detail, channel_id, "Discord send failed");
                bail!("Discord send failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                warn!(error = %error, channel_id, "Discord send transport failed");
                bail!("Discord send transport failed: {error}");
            }
        }
    }
}

pub(super) fn outbound_embed_parts(message: &str) -> (String, String, u32) {
    let trimmed = message.trim();
    if let Some(rest) = trimmed.strip_prefix("--- Message from ") {
        if let Some((sender, body)) = rest.split_once("---\n") {
            let sender = sender.trim();
            return (
                format!("Message from {sender}"),
                body.trim().to_string(),
                color_for_role(sender),
            );
        }
    }

    (
        "Batty update".to_string(),
        trimmed.to_string(),
        color_for_role("system"),
    )
}

pub(super) fn color_for_role(role: &str) -> u32 {
    let role = role.to_ascii_lowercase();
    if role.contains("architect") {
        0x3B82F6
    } else if role.contains("manager") {
        0x22C55E
    } else if role.contains("engineer") || role.starts_with("eng-") {
        0xF97316
    } else if role.contains("human") || role.contains("user") {
        0x8B5CF6
    } else if role.contains("daemon") || role.contains("system") {
        0x64748B
    } else {
        0x0EA5E9
    }
}

fn truncate_for_discord(input: &str, limit: usize) -> String {
    let mut output = input.chars().take(limit).collect::<String>();
    if input.chars().count() > limit && limit > 3 {
        output.truncate(limit.saturating_sub(3));
        output.push_str("...");
    }
    output
}

fn parse_messages_response(
    json: &serde_json::Value,
    allowed_user_ids: &[i64],
) -> Result<(Vec<InboundMessage>, Option<String>)> {
    let messages = json
        .as_array()
        .ok_or_else(|| anyhow!("Discord messages response was not an array"))?;

    let mut inbound = Vec::new();
    let mut latest_message_id: Option<(u64, String)> = None;

    for message in messages {
        let message_id = match message.get("id").and_then(|value| value.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let channel_id = match message.get("channel_id").and_then(|value| value.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let content = match message.get("content").and_then(|value| value.as_str()) {
            Some(content) if !content.trim().is_empty() => content.trim().to_string(),
            _ => continue,
        };

        let author = match message.get("author") {
            Some(author) => author,
            None => continue,
        };
        if author.get("bot").and_then(|value| value.as_bool()) == Some(true) {
            continue;
        }

        let from_user_id = match author.get("id").and_then(|value| value.as_str()) {
            Some(raw) => raw
                .parse::<i64>()
                .with_context(|| format!("invalid Discord user id '{raw}'"))?,
            None => continue,
        };
        if !allowed_user_ids.contains(&from_user_id) {
            continue;
        }

        let snowflake = message_id
            .parse::<u64>()
            .with_context(|| format!("invalid Discord message id '{message_id}'"))?;
        match &latest_message_id {
            Some((latest, _)) if *latest >= snowflake => {}
            _ => latest_message_id = Some((snowflake, message_id.clone())),
        }

        inbound.push(InboundMessage {
            message_id,
            channel_id,
            from_user_id,
            text: content,
        });
    }

    inbound.sort_by_key(|message| message.message_id.parse::<u64>().unwrap_or(0));
    Ok((inbound, latest_message_id.map(|(_, id)| id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_embed_parts_extracts_sender_header() {
        let (title, description, color) =
            outbound_embed_parts("--- Message from architect ---\nFocus on tests");
        assert_eq!(title, "Message from architect");
        assert_eq!(description, "Focus on tests");
        assert_eq!(color, color_for_role("architect"));
    }

    #[test]
    fn outbound_embed_parts_falls_back_for_plain_text() {
        let (title, description, color) = outbound_embed_parts("plain message");
        assert_eq!(title, "Batty update");
        assert_eq!(description, "plain message");
        assert_eq!(color, color_for_role("system"));
    }

    #[test]
    fn parse_messages_response_filters_unauthorized_and_bot_messages() {
        let json = serde_json::json!([
            {
                "id": "1002",
                "channel_id": "55",
                "content": "$status",
                "author": {"id": "42", "bot": false}
            },
            {
                "id": "1001",
                "channel_id": "55",
                "content": "hello",
                "author": {"id": "999", "bot": false}
            },
            {
                "id": "1003",
                "channel_id": "55",
                "content": "ignore me",
                "author": {"id": "42", "bot": true}
            }
        ]);

        let (messages, latest_message_id) = parse_messages_response(&json, &[42]).unwrap();
        assert_eq!(
            messages,
            vec![InboundMessage {
                message_id: "1002".to_string(),
                channel_id: "55".to_string(),
                from_user_id: 42,
                text: "$status".to_string(),
            }]
        );
        assert_eq!(latest_message_id.as_deref(), Some("1002"));
    }

    #[test]
    fn parse_messages_response_sorts_by_message_id() {
        let json = serde_json::json!([
            {
                "id": "1009",
                "channel_id": "55",
                "content": "second",
                "author": {"id": "42", "bot": false}
            },
            {
                "id": "1008",
                "channel_id": "55",
                "content": "first",
                "author": {"id": "42", "bot": false}
            }
        ]);

        let (messages, latest_message_id) = parse_messages_response(&json, &[42]).unwrap();
        assert_eq!(messages[0].text, "first");
        assert_eq!(messages[1].text, "second");
        assert_eq!(latest_message_id.as_deref(), Some("1009"));
    }
}
