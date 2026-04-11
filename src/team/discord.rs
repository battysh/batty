//! Native Discord Bot API client for batty.
//!
//! Uses Discord's HTTP API directly for outbound embeds and command-channel
//! polling, keeping the implementation aligned with the existing Telegram
//! bridge's blocking request model.

use std::io::{self, Write as IoWrite};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use tracing::{debug, warn};

use crate::env_file;

use super::config::{ChannelConfig, RoleType, TeamConfig};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const MAX_EMBED_TITLE_LEN: usize = 256;
const MAX_EMBED_DESCRIPTION_LEN: usize = 4_000;
const MAX_EMBED_FIELD_NAME_LEN: usize = 256;
const MAX_EMBED_FIELD_VALUE_LEN: usize = 1_024;
const MAX_EMBED_FOOTER_LEN: usize = 2_048;
const MAX_EMBED_AUTHOR_NAME_LEN: usize = 256;
const MAX_EMBED_FIELDS: usize = 25;
const MAX_CONTENT_LEN: usize = 2_000;

/// A single key/value pair inside an embed. Matches Discord's
/// `embed.fields[]` element. Inline fields are shown side-by-side on
/// wide screens, non-inline fields stack vertically. Up to 25 fields
/// per embed. Names and values are truncated to Discord's limits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

impl EmbedField {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            inline: false,
        }
    }

    pub fn inline(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            inline: true,
        }
    }
}

/// Rich embed payload. Everything except `title` and `color` is
/// optional — builders that only care about title/description/color can
/// still default the rest. See `send_rich_embed` for the transport side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichEmbed {
    pub title: String,
    pub description: Option<String>,
    pub color: u32,
    pub url: Option<String>,
    /// Author block — shown in small type above the title. Commonly used
    /// to attribute an event to an agent (e.g. `eng-1-2` or `manager`).
    pub author_name: Option<String>,
    pub author_icon_url: Option<String>,
    pub author_url: Option<String>,
    /// Footer — shown below the embed body. Good place for provenance
    /// (daemon id, version, event id) and deep-links.
    pub footer: Option<String>,
    pub footer_icon_url: Option<String>,
    /// ISO 8601 timestamp for the embed. Discord renders this as a
    /// right-aligned relative time near the footer.
    pub timestamp: Option<String>,
    /// Right-hand thumbnail image (square, ~80x80).
    pub thumbnail_url: Option<String>,
    pub fields: Vec<EmbedField>,
}

impl RichEmbed {
    pub fn new(title: impl Into<String>, color: u32) -> Self {
        Self {
            title: title.into(),
            color,
            ..Self::default()
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_author(mut self, name: impl Into<String>) -> Self {
        self.author_name = Some(name.into());
        self
    }

    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    pub fn with_timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = Some(timestamp.into());
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn push_field(mut self, field: EmbedField) -> Self {
        if self.fields.len() < MAX_EMBED_FIELDS {
            self.fields.push(field);
        }
        self
    }

    /// Serialize to a `serde_json::Value` suitable for nesting under an
    /// `embeds` array in a Discord message payload. Applies all of
    /// Discord's length limits via `truncate_for_discord`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut embed = serde_json::json!({
            "title": truncate_for_discord(&self.title, MAX_EMBED_TITLE_LEN),
            "color": self.color,
        });
        if let Some(description) = self.description.as_deref() {
            embed["description"] = serde_json::Value::String(truncate_for_discord(
                description,
                MAX_EMBED_DESCRIPTION_LEN,
            ));
        }
        if let Some(url) = self.url.as_deref() {
            embed["url"] = serde_json::Value::String(url.to_string());
        }
        if let Some(author_name) = self.author_name.as_deref() {
            let mut author = serde_json::json!({
                "name": truncate_for_discord(author_name, MAX_EMBED_AUTHOR_NAME_LEN),
            });
            if let Some(icon_url) = self.author_icon_url.as_deref() {
                author["icon_url"] = serde_json::Value::String(icon_url.to_string());
            }
            if let Some(author_url) = self.author_url.as_deref() {
                author["url"] = serde_json::Value::String(author_url.to_string());
            }
            embed["author"] = author;
        }
        if let Some(footer) = self.footer.as_deref() {
            let mut footer_obj = serde_json::json!({
                "text": truncate_for_discord(footer, MAX_EMBED_FOOTER_LEN),
            });
            if let Some(icon_url) = self.footer_icon_url.as_deref() {
                footer_obj["icon_url"] = serde_json::Value::String(icon_url.to_string());
            }
            embed["footer"] = footer_obj;
        }
        if let Some(timestamp) = self.timestamp.as_deref() {
            embed["timestamp"] = serde_json::Value::String(timestamp.to_string());
        }
        if let Some(thumbnail) = self.thumbnail_url.as_deref() {
            embed["thumbnail"] = serde_json::json!({ "url": thumbnail });
        }
        if !self.fields.is_empty() {
            let fields: Vec<serde_json::Value> = self
                .fields
                .iter()
                .take(MAX_EMBED_FIELDS)
                .map(|field| {
                    serde_json::json!({
                        "name": truncate_for_discord(&field.name, MAX_EMBED_FIELD_NAME_LEN),
                        "value": truncate_for_discord(&field.value, MAX_EMBED_FIELD_VALUE_LEN),
                        "inline": field.inline,
                    })
                })
                .collect();
            embed["fields"] = serde_json::Value::Array(fields);
        }
        embed
    }
}

/// An inbound message received from Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub message_id: String,
    pub channel_id: String,
    pub from_user_id: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotIdentity {
    pub user_id: String,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildSummary {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
    pub kind: u8,
    pub position: i64,
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
        self.post_message(channel_id, &body).map(|_| ())
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
        self.post_message(channel_id, &body).map(|_| ())
    }

    /// Post a single rich embed to a channel. Supports fields, footer,
    /// author, timestamp, URL and thumbnail — strictly a superset of
    /// `send_embed`. Use `RichEmbed::new(...).with_*(...).push_field(...)`
    /// to build the payload. All length limits are applied by
    /// `RichEmbed::to_json`.
    pub fn send_rich_embed(&self, channel_id: &str, embed: &RichEmbed) -> Result<()> {
        let body = serde_json::json!({
            "embeds": [embed.to_json()],
            "allowed_mentions": { "parse": [] }
        });
        self.post_message(channel_id, &body).map(|_| ())
    }

    pub fn send_command_reply(&self, text: &str) -> Result<()> {
        self.send_plain_message(&self.commands_channel_id, text)
    }

    pub fn send_formatted_message(&self, channel_id: &str, message: &str) -> Result<()> {
        let embed = outbound_embed(message);
        self.send_rich_embed(channel_id, &embed)
    }

    pub fn validate_token(&self) -> Result<BotIdentity> {
        let json = self.get_json(&format!("{DISCORD_API_BASE}/users/@me"))?;
        parse_bot_identity(&json)
    }

    pub fn list_guilds(&self) -> Result<Vec<GuildSummary>> {
        let json = self.get_json(&format!("{DISCORD_API_BASE}/users/@me/guilds"))?;
        parse_guilds_response(&json)
    }

    pub fn list_guild_channels(&self, guild_id: &str) -> Result<Vec<ChannelSummary>> {
        let json = self.get_json(&format!("{DISCORD_API_BASE}/guilds/{guild_id}/channels"))?;
        parse_channels_response(&json)
    }

    pub fn get_channel(&self, channel_id: &str) -> Result<ChannelSummary> {
        let json = self.get_json(&format!("{DISCORD_API_BASE}/channels/{channel_id}"))?;
        parse_channel_response(&json)
    }

    pub fn create_message(&self, channel_id: &str, body: &serde_json::Value) -> Result<String> {
        self.post_message(channel_id, body)
    }

    pub fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}");
        let response = ureq::request("PATCH", &url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match response {
            Ok(resp) => {
                debug!(
                    status = resp.status(),
                    channel_id, message_id, "Discord message edited"
                );
                Ok(())
            }
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                warn!(
                    status,
                    detail = %detail,
                    channel_id,
                    message_id,
                    "Discord edit failed"
                );
                bail!("Discord edit failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                warn!(
                    error = %error,
                    channel_id,
                    message_id,
                    "Discord edit transport failed"
                );
                bail!("Discord edit transport failed: {error}");
            }
        }
    }

    pub fn pin_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/pins/{message_id}");
        let response = ureq::request("PUT", &url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .call();

        match response {
            Ok(resp) => {
                debug!(
                    status = resp.status(),
                    channel_id, message_id, "Discord message pinned"
                );
                Ok(())
            }
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                warn!(
                    status,
                    detail = %detail,
                    channel_id,
                    message_id,
                    "Discord pin failed"
                );
                bail!("Discord pin failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                warn!(
                    error = %error,
                    channel_id,
                    message_id,
                    "Discord pin transport failed"
                );
                bail!("Discord pin transport failed: {error}");
            }
        }
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

    fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let response = ureq::get(url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .call();

        match response {
            Ok(resp) => resp.into_json().context("failed to parse Discord response"),
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                bail!("Discord request failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                bail!("Discord request transport failed: {error}");
            }
        }
    }

    fn post_message(&self, channel_id: &str, body: &serde_json::Value) -> Result<String> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let response = ureq::post(&url)
            .set("Authorization", &format!("Bot {}", self.bot_token))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match response {
            Ok(resp) => {
                let json: serde_json::Value = resp
                    .into_json()
                    .context("failed to parse Discord post-message response")?;
                let message_id = json
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow!("Discord post-message response missing id"))?
                    .to_string();
                debug!(channel_id, message_id, "Discord message accepted");
                Ok(message_id)
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

pub fn setup_discord(project_root: &Path) -> Result<()> {
    let config_path = project_root
        .join(".batty")
        .join("team_config")
        .join("team.yaml");
    if !config_path.exists() {
        bail!(
            "no team config found at {}; run `batty init` first",
            config_path.display()
        );
    }

    println!("Discord Bot Setup");
    println!("=================\n");

    println!("Step 1: Bot Token");
    println!("  Create a Discord bot in the Developer Portal and copy the bot token.");
    println!("  You can also export BATTY_DISCORD_BOT_TOKEN before running this wizard.\n");
    let bot_token = prompt_discord_token()?;
    let setup_bot = DiscordBot::new(bot_token.clone(), Vec::new(), String::new());
    let identity = setup_bot.validate_token()?;
    println!(
        "Bot validated: {} ({})\n",
        identity.username, identity.user_id
    );

    println!("Step 2: Pick A Server");
    let guilds = setup_bot.list_guilds()?;
    if guilds.is_empty() {
        bail!("the bot is not in any Discord servers; invite it first, then retry");
    }
    let guild_index = prompt_choice(
        "Select a server",
        &guilds.iter().map(|g| g.name.clone()).collect::<Vec<_>>(),
    )?;
    let guild = &guilds[guild_index];
    println!("Selected server: {}\n", guild.name);

    println!("Step 3: Pick Channels");
    let channels = setup_bot.list_guild_channels(&guild.id)?;
    if channels.is_empty() {
        bail!("no text channels found in '{}'", guild.name);
    }
    let commands_channel = prompt_channel_choice("commands", &channels, &[])?;
    let events_channel =
        prompt_channel_choice("events", &channels, &[commands_channel.id.as_str()])?;
    let agents_channel = prompt_channel_choice(
        "agents",
        &channels,
        &[commands_channel.id.as_str(), events_channel.id.as_str()],
    )?;
    println!();

    println!("Step 4: Allowed User IDs");
    println!("  Enter one or more Discord user IDs, separated by commas.");
    let allowed_user_ids = prompt_user_ids()?;

    println!("Step 5: Test Messages");
    let bot = DiscordBot::new(
        bot_token.clone(),
        allowed_user_ids.clone(),
        commands_channel.id.clone(),
    );
    send_setup_test_messages(
        &bot,
        commands_channel,
        events_channel,
        agents_channel,
        &guild.name,
    )?;
    println!("Test messages sent to all selected channels.\n");

    let env_path = project_root.join(".env");
    env_file::upsert_env_var(&env_path, "BATTY_DISCORD_BOT_TOKEN", &bot_token)?;
    update_team_yaml_for_discord(
        &config_path,
        &commands_channel.id,
        &events_channel.id,
        &agents_channel.id,
        &allowed_user_ids,
    )?;

    println!("Discord configured successfully.");
    println!("Saved BATTY_DISCORD_BOT_TOKEN to {}", env_path.display());
    println!("Restart the daemon with: batty stop && batty start");
    Ok(())
}

pub fn discord_status(project_root: &Path) -> Result<()> {
    let config_path = project_root
        .join(".batty")
        .join("team_config")
        .join("team.yaml");
    if !config_path.exists() {
        bail!(
            "no team config found at {}; run `batty init` first",
            config_path.display()
        );
    }

    let team_config = TeamConfig::load(&config_path)?;
    let Some(role) = team_config.roles.iter().find(|role| {
        role.role_type == RoleType::User && role.channel.as_deref() == Some("discord")
    }) else {
        println!("Discord is not configured in team.yaml.");
        return Ok(());
    };

    let Some(channel_config) = role.channel_config.as_ref() else {
        bail!("Discord user role exists but channel_config is missing");
    };
    let Some(bot) = DiscordBot::from_config(channel_config) else {
        bail!("Discord is configured but bot token or commands channel is missing");
    };

    let identity = bot.validate_token()?;
    let commands = channel_config
        .commands_channel_id
        .as_deref()
        .map(|id| bot.get_channel(id))
        .transpose()?;
    let events = channel_config
        .events_channel_id
        .as_deref()
        .map(|id| bot.get_channel(id))
        .transpose()?;
    let agents = channel_config
        .agents_channel_id
        .as_deref()
        .map(|id| bot.get_channel(id))
        .transpose()?;

    println!("Discord Status");
    println!("==============");
    println!("Role: {}", role.name);
    println!("Bot: {} ({})", identity.username, identity.user_id);
    println!(
        "Allowed Users: {}",
        channel_config
            .allowed_user_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Commands: {}",
        commands
            .as_ref()
            .map(format_channel_label)
            .unwrap_or_else(|| "not configured".to_string())
    );
    println!(
        "Events: {}",
        events
            .as_ref()
            .map(format_channel_label)
            .unwrap_or_else(|| "not configured".to_string())
    );
    println!(
        "Agents: {}",
        agents
            .as_ref()
            .map(format_channel_label)
            .unwrap_or_else(|| "not configured".to_string())
    );
    println!("Health: ok");
    Ok(())
}

pub(super) fn outbound_embed(message: &str) -> RichEmbed {
    let trimmed = message.trim();
    if let Some(rest) = trimmed.strip_prefix("--- Message from ") {
        if let Some((sender, body)) = rest.split_once("---\n") {
            let sender = sender.trim();
            return RichEmbed::new("💬 Command Update", color_for_role(sender))
                .with_author(role_author_label(sender))
                .with_description(body.trim())
                .with_footer("batty · command surface")
                .with_timestamp(Utc::now().to_rfc3339());
        }
    }

    RichEmbed::new("💬 Batty Update", color_for_role("system"))
        .with_description(trimmed)
        .with_footer("batty · command surface")
        .with_timestamp(Utc::now().to_rfc3339())
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

/// Severity classification for Discord embed colors. Derived from the
/// event type — NOT the sender role. Role-based coloring made success
/// and failure look identical whenever they came from the same
/// engineer; severity-based coloring matches the Discord brand palette
/// (green/blurple/yellow/red/dark-red) and is what users expect from
/// ops bots in 2025+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Success,
    Info,
    Warn,
    Error,
    Critical,
    Neutral,
}

impl Severity {
    /// Discord-brand-aligned hex color for the severity.
    pub fn color(self) -> u32 {
        match self {
            Severity::Success => 0x57F287,  // Discord Green
            Severity::Info => 0x5865F2,     // Discord Blurple
            Severity::Warn => 0xFEE75C,     // Discord Yellow
            Severity::Error => 0xED4245,    // Discord Red
            Severity::Critical => 0x992D22, // DarkRed
            Severity::Neutral => 0x99AAB5,  // Greyple
        }
    }
}

/// Map a `TeamEvent` kind (the `event` string) to a severity tier.
///
/// Keeps the classifier next to `color_for_role` so the Discord layer
/// has one place for "how should this look?" decisions. The match is
/// intentionally explicit — we'd rather a new event default to
/// `Neutral` than pick up the wrong color from a regex-ish fallback.
pub fn severity_for_event(event: &str) -> Severity {
    use Severity::*;
    match event {
        // Green — something good finished.
        "merge_success"
        | "task_auto_merged"
        | "task_manual_merged"
        | "verification_evidence_collected"
        | "daemon_started"
        | "agent_spawned"
        | "auto_doctor_action" => Success,

        // Blurple — routine operational information.
        "task_assigned"
        | "task_claim_created"
        | "verification_phase_changed"
        | "standup_posted"
        | "merge_confidence_scored" => Info,

        // Yellow — soft warning; needs attention soon but not broken.
        "task_stale"
        | "dispatch_overlap_skipped"
        | "pattern_detected"
        | "narration_rejection"
        | "review_aging" => Warn,

        // Red — something is broken or blocked and someone needs to act.
        "task_escalated"
        | "stall_detected"
        | "context_exhausted"
        | "verification_failed"
        | "merge_conflict"
        | "merge_failed"
        | "pane_death"
        | "scope_fence_violation" => Error,

        // DarkRed — critical; the daemon or a backend is out of service.
        "backend_quota_exhausted" | "daemon_stopped" | "loop_step_error" | "shim_crash" => Critical,

        // Everything else defaults to neutral grey.
        _ => Neutral,
    }
}

/// Convert a role string into a (prefix, emoji) pair for the embed
/// author block. Kept tiny so callers can embed it in a single line.
pub(super) fn role_author_label(role: &str) -> String {
    let role_lc = role.to_ascii_lowercase();
    if role_lc.contains("architect") {
        format!("🏗️ {role}")
    } else if role_lc.contains("manager") {
        format!("📋 {role}")
    } else if role_lc.starts_with("eng") || role_lc.contains("engineer") {
        format!("🔧 {role}")
    } else if role_lc.contains("human") || role_lc.contains("user") {
        format!("👤 {role}")
    } else if role_lc.contains("daemon") || role_lc.contains("system") || role_lc == "batty" {
        format!("⚙️ {role}")
    } else {
        role.to_string()
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

fn parse_bot_identity(json: &serde_json::Value) -> Result<BotIdentity> {
    let user_id = json
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("Discord identity missing id"))?;
    let username = json
        .get("username")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("Discord identity missing username"))?;

    Ok(BotIdentity {
        user_id: user_id.to_string(),
        username: username.to_string(),
    })
}

fn parse_guilds_response(json: &serde_json::Value) -> Result<Vec<GuildSummary>> {
    let guilds = json
        .as_array()
        .ok_or_else(|| anyhow!("Discord guilds response was not an array"))?;

    let mut parsed = guilds
        .iter()
        .filter_map(|guild| {
            Some(GuildSummary {
                id: guild.get("id")?.as_str()?.to_string(),
                name: guild.get("name")?.as_str()?.to_string(),
            })
        })
        .collect::<Vec<_>>();
    parsed.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(parsed)
}

fn parse_channels_response(json: &serde_json::Value) -> Result<Vec<ChannelSummary>> {
    let channels = json
        .as_array()
        .ok_or_else(|| anyhow!("Discord channels response was not an array"))?;

    let mut parsed = channels
        .iter()
        .filter_map(parse_channel_value)
        .filter(|channel| matches!(channel.kind, 0 | 5))
        .collect::<Vec<_>>();
    parsed.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(parsed)
}

fn parse_channel_response(json: &serde_json::Value) -> Result<ChannelSummary> {
    parse_channel_value(json).ok_or_else(|| anyhow!("Discord channel response missing fields"))
}

fn parse_channel_value(json: &serde_json::Value) -> Option<ChannelSummary> {
    Some(ChannelSummary {
        id: json.get("id")?.as_str()?.to_string(),
        name: json.get("name")?.as_str()?.to_string(),
        kind: json
            .get("type")?
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())?,
        position: json
            .get("position")
            .and_then(|value| value.as_i64())
            .unwrap_or(0),
    })
}

fn prompt_discord_token() -> Result<String> {
    if let Ok(token) = std::env::var("BATTY_DISCORD_BOT_TOKEN")
        && !token.trim().is_empty()
    {
        println!("Found BATTY_DISCORD_BOT_TOKEN in the environment.");
        if prompt_yes_no("Use the environment token? [Y/n]: ", true)? {
            return Ok(token);
        }
        println!();
    }

    loop {
        let token = prompt("Enter your Discord bot token: ")?;
        if token.is_empty() {
            println!("Token cannot be empty. Try again.\n");
            continue;
        }
        return Ok(token);
    }
}

fn prompt_user_ids() -> Result<Vec<i64>> {
    loop {
        let input = prompt("Enter allowed Discord user IDs (comma-separated): ")?;
        let ids = input
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.parse::<i64>()
                    .with_context(|| format!("invalid Discord user id '{part}'"))
            })
            .collect::<Result<Vec<_>>>();
        match ids {
            Ok(ids) if !ids.is_empty() => return Ok(ids),
            Ok(_) => println!("Enter at least one Discord user ID.\n"),
            Err(error) => println!("{error}\n"),
        }
    }
}

fn prompt_choice(prompt_text: &str, options: &[String]) -> Result<usize> {
    println!("{prompt_text}:");
    for (index, option) in options.iter().enumerate() {
        println!("  {}) {}", index + 1, option);
    }

    loop {
        let input = prompt("Enter number: ")?;
        match input.parse::<usize>() {
            Ok(choice) if (1..=options.len()).contains(&choice) => return Ok(choice - 1),
            _ => println!("Invalid selection. Try again.\n"),
        }
    }
}

fn prompt_channel_choice<'a>(
    label: &str,
    channels: &'a [ChannelSummary],
    taken_ids: &[&str],
) -> Result<&'a ChannelSummary> {
    println!("Select the #{label} channel:");
    let options = channels
        .iter()
        .map(format_channel_label)
        .collect::<Vec<_>>();

    loop {
        let index = prompt_choice("Available channels", &options)?;
        let channel = &channels[index];
        if taken_ids.iter().any(|taken| *taken == channel.id) {
            println!("That channel is already assigned. Pick a different one.\n");
            continue;
        }
        return Ok(channel);
    }
}

fn format_channel_label(channel: &ChannelSummary) -> String {
    format!("#{} ({})", channel.name, channel.id)
}

fn send_setup_test_messages(
    bot: &DiscordBot,
    commands: &ChannelSummary,
    events: &ChannelSummary,
    agents: &ChannelSummary,
    guild_name: &str,
) -> Result<()> {
    bot.send_plain_message(
        &commands.id,
        &format!("Batty Discord setup complete for {guild_name}. Commands channel verified."),
    )?;
    bot.send_plain_message(
        &events.id,
        &format!("Batty Discord setup complete for {guild_name}. Events channel verified."),
    )?;
    bot.send_plain_message(
        &agents.id,
        &format!("Batty Discord setup complete for {guild_name}. Agents channel verified."),
    )?;
    Ok(())
}

fn update_team_yaml_for_discord(
    path: &Path,
    commands_channel_id: &str,
    events_channel_id: &str,
    agents_channel_id: &str,
    allowed_user_ids: &[i64],
) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let roles = doc
        .get_mut("roles")
        .and_then(|value| value.as_sequence_mut())
        .ok_or_else(|| anyhow!("no 'roles' sequence in team.yaml"))?;

    let user_role = roles.iter_mut().find(|role| {
        role.get("role_type")
            .and_then(|value| value.as_str())
            .map(|role_type| role_type == "user")
            .unwrap_or(false)
    });

    if let Some(role) = user_role {
        role["channel"] = serde_yaml::Value::String("discord".into());
        if role.get("channel_config").is_none()
            && let Some(role_map) = role.as_mapping_mut()
        {
            role_map.insert(
                serde_yaml::Value::String("channel_config".into()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }

        let channel_config = &mut role["channel_config"];
        let mapping = channel_config
            .as_mapping_mut()
            .ok_or_else(|| anyhow!("channel_config must be a mapping"))?;
        mapping.remove(serde_yaml::Value::String("target".into()));
        mapping.remove(serde_yaml::Value::String("provider".into()));
        mapping.remove(serde_yaml::Value::String("bot_token".into()));
        mapping.insert(
            "commands_channel_id".into(),
            serde_yaml::Value::String(commands_channel_id.into()),
        );
        mapping.insert(
            "events_channel_id".into(),
            serde_yaml::Value::String(events_channel_id.into()),
        );
        mapping.insert(
            "agents_channel_id".into(),
            serde_yaml::Value::String(agents_channel_id.into()),
        );
        mapping.insert(
            "allowed_user_ids".into(),
            serde_yaml::Value::Sequence(
                allowed_user_ids
                    .iter()
                    .copied()
                    .map(|id| serde_yaml::Value::Number(serde_yaml::Number::from(id)))
                    .collect(),
            ),
        );
    } else {
        let mut new_role = serde_yaml::Mapping::new();
        new_role.insert("name".into(), "human".into());
        new_role.insert("role_type".into(), "user".into());
        new_role.insert("channel".into(), "discord".into());

        let mut channel_config = serde_yaml::Mapping::new();
        channel_config.insert(
            "commands_channel_id".into(),
            serde_yaml::Value::String(commands_channel_id.into()),
        );
        channel_config.insert(
            "events_channel_id".into(),
            serde_yaml::Value::String(events_channel_id.into()),
        );
        channel_config.insert(
            "agents_channel_id".into(),
            serde_yaml::Value::String(agents_channel_id.into()),
        );
        channel_config.insert(
            "allowed_user_ids".into(),
            serde_yaml::Value::Sequence(
                allowed_user_ids
                    .iter()
                    .copied()
                    .map(|id| serde_yaml::Value::Number(serde_yaml::Number::from(id)))
                    .collect(),
            ),
        );
        new_role.insert(
            "channel_config".into(),
            serde_yaml::Value::Mapping(channel_config),
        );
        new_role.insert(
            "talks_to".into(),
            serde_yaml::Value::Sequence(vec!["architect".into()]),
        );
        roles.push(serde_yaml::Value::Mapping(new_role));
    }

    let output = serde_yaml::to_string(&doc)?;
    std::fs::write(path, output).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let input = prompt(message)?;
    if input.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(input.chars().next(), Some('y' | 'Y')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bot_identity_extracts_username_and_id() {
        let json = serde_json::json!({
            "id": "123456789012345678",
            "username": "batty-bot"
        });

        let identity = parse_bot_identity(&json).unwrap();
        assert_eq!(identity.user_id, "123456789012345678");
        assert_eq!(identity.username, "batty-bot");
    }

    #[test]
    fn parse_guilds_response_sorts_by_name() {
        let json = serde_json::json!([
            {"id": "2", "name": "Zulu"},
            {"id": "1", "name": "Alpha"}
        ]);

        let guilds = parse_guilds_response(&json).unwrap();
        assert_eq!(
            guilds,
            vec![
                GuildSummary {
                    id: "1".into(),
                    name: "Alpha".into(),
                },
                GuildSummary {
                    id: "2".into(),
                    name: "Zulu".into(),
                }
            ]
        );
    }

    #[test]
    fn parse_channels_response_filters_non_text_channels() {
        let json = serde_json::json!([
            {"id": "10", "name": "voice", "type": 2, "position": 0},
            {"id": "11", "name": "commands", "type": 0, "position": 2},
            {"id": "12", "name": "events", "type": 5, "position": 1}
        ]);

        let channels = parse_channels_response(&json).unwrap();
        assert_eq!(
            channels,
            vec![
                ChannelSummary {
                    id: "12".into(),
                    name: "events".into(),
                    kind: 5,
                    position: 1,
                },
                ChannelSummary {
                    id: "11".into(),
                    name: "commands".into(),
                    kind: 0,
                    position: 2,
                }
            ]
        );
    }

    #[test]
    fn outbound_embed_extracts_sender_header() {
        let embed = outbound_embed("--- Message from architect ---\nFocus on tests");
        assert_eq!(embed.title, "💬 Command Update");
        assert_eq!(embed.description.as_deref(), Some("Focus on tests"));
        assert_eq!(embed.color, color_for_role("architect"));
        assert_eq!(embed.author_name.as_deref(), Some("🏗️ architect"));
        assert_eq!(embed.footer.as_deref(), Some("batty · command surface"));
        assert!(
            embed
                .timestamp
                .as_deref()
                .is_some_and(|ts| ts.contains('T'))
        );
    }

    #[test]
    fn outbound_embed_falls_back_for_plain_text() {
        let embed = outbound_embed("plain message");
        assert_eq!(embed.title, "💬 Batty Update");
        assert_eq!(embed.description.as_deref(), Some("plain message"));
        assert_eq!(embed.color, color_for_role("system"));
        assert_eq!(embed.author_name, None);
        assert_eq!(embed.footer.as_deref(), Some("batty · command surface"));
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

    #[test]
    fn update_team_yaml_for_discord_updates_existing_user_role() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(
            &path,
            r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "placeholder"
      provider: openclaw
    talks_to: [architect]
"#,
        )
        .unwrap();

        update_team_yaml_for_discord(&path, "cmd-1", "evt-1", "agt-1", &[111, 222]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("channel: discord"));
        assert!(content.contains("commands_channel_id: cmd-1"));
        assert!(content.contains("events_channel_id: evt-1"));
        assert!(content.contains("agents_channel_id: agt-1"));
        assert!(!content.contains("bot_token"));
        assert!(!content.contains("target: placeholder"));
        assert!(!content.contains("provider: openclaw"));
    }

    #[test]
    fn update_team_yaml_for_discord_creates_user_role_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(
            &path,
            r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();

        update_team_yaml_for_discord(&path, "cmd-1", "evt-1", "agt-1", &[111]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("name: human"));
        assert!(content.contains("role_type: user"));
        assert!(content.contains("channel: discord"));
        assert!(content.contains("commands_channel_id: cmd-1"));
        assert!(!content.contains("bot_token"));
    }

    #[test]
    fn setup_discord_bails_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let result = setup_discord(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("batty init"));
    }
}
