//! External communication channels for user roles.
//!
//! The `user` role type communicates via channels (Telegram, Slack, etc.)
//! instead of tmux panes. Each channel provider is a CLI tool that the
//! daemon invokes for outbound messages.

use anyhow::{Result, bail};
use tracing::{debug, warn};

use super::config::ChannelConfig;
use super::telegram::TelegramBot;

/// Trait for outbound message delivery to external channels.
pub trait Channel: Send + Sync {
    /// Send a text message to the channel destination.
    fn send(&self, message: &str) -> Result<()>;
    /// Channel type identifier (e.g., "telegram").
    fn channel_type(&self) -> &str;
}

/// Telegram channel via openclaw (or any CLI provider).
pub struct TelegramChannel {
    target: String,
    provider: String,
}

impl TelegramChannel {
    pub fn new(target: String, provider: String) -> Self {
        Self { target, provider }
    }

    pub fn from_config(config: &ChannelConfig) -> Self {
        Self::new(config.target.clone(), config.provider.clone())
    }
}

impl Channel for TelegramChannel {
    fn send(&self, message: &str) -> Result<()> {
        debug!(target = %self.target, provider = %self.provider, len = message.len(), "sending via telegram channel");

        let output = std::process::Command::new(&self.provider)
            .args([
                "message",
                "send",
                "--to",
                &self.target,
                "--message",
                message,
            ])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                debug!("telegram message sent successfully");
                Ok(())
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(status = ?out.status, stderr = %stderr, "telegram send failed");
                bail!("channel send failed: {stderr}")
            }
            Err(e) => {
                warn!(error = %e, provider = %self.provider, "failed to execute channel provider");
                bail!("failed to execute provider '{}': {e}", self.provider)
            }
        }
    }

    fn channel_type(&self) -> &str {
        "telegram"
    }
}

/// Native Telegram channel using the Bot API directly (no CLI provider).
pub struct NativeTelegramChannel {
    bot: TelegramBot,
    target: String,
}

impl NativeTelegramChannel {
    pub fn new(bot: TelegramBot, target: String) -> Self {
        Self { bot, target }
    }

    /// Build from a `ChannelConfig`, returning `None` if no bot token is available.
    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        TelegramBot::from_config(config).map(|bot| Self::new(bot, config.target.clone()))
    }
}

impl Channel for NativeTelegramChannel {
    fn send(&self, message: &str) -> Result<()> {
        debug!(target = %self.target, len = message.len(), "sending via native telegram channel");
        self.bot.send_message(&self.target, message)
    }

    fn channel_type(&self) -> &str {
        "telegram-native"
    }
}

/// Create a channel from config fields.
pub fn channel_from_config(channel_type: &str, config: &ChannelConfig) -> Result<Box<dyn Channel>> {
    match channel_type {
        "telegram" => {
            if let Some(native) = NativeTelegramChannel::from_config(config) {
                Ok(Box::new(native))
            } else {
                Ok(Box::new(TelegramChannel::from_config(config)))
            }
        }
        other => bail!("unsupported channel type: '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_channel_type() {
        let ch = TelegramChannel::new("12345".into(), "openclaw".into());
        assert_eq!(ch.channel_type(), "telegram");
    }

    #[test]
    fn native_telegram_channel_type() {
        let bot = TelegramBot::new("test-token".into(), vec![]);
        let ch = NativeTelegramChannel::new(bot, "12345".into());
        assert_eq!(ch.channel_type(), "telegram-native");
    }

    #[test]
    fn channel_from_config_telegram() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: None,
            allowed_user_ids: vec![],
        };
        // Without bot_token (and assuming env var is not set), falls back to CLI channel.
        if std::env::var("BATTY_TELEGRAM_BOT_TOKEN").is_err() {
            let ch = channel_from_config("telegram", &config).unwrap();
            assert_eq!(ch.channel_type(), "telegram");
        }
    }

    #[test]
    fn channel_from_config_telegram_with_bot_token() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: Some("test-bot-token".into()),
            allowed_user_ids: vec![],
        };
        let ch = channel_from_config("telegram", &config).unwrap();
        assert_eq!(ch.channel_type(), "telegram-native");
    }

    #[test]
    fn channel_from_config_telegram_without_bot_token() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: None,
            allowed_user_ids: vec![],
        };
        // Only assert CLI fallback when the env var is also absent.
        if std::env::var("BATTY_TELEGRAM_BOT_TOKEN").is_err() {
            let ch = channel_from_config("telegram", &config).unwrap();
            assert_eq!(ch.channel_type(), "telegram");
        }
    }

    #[test]
    fn channel_from_config_unknown_type() {
        let config = ChannelConfig {
            target: "x".into(),
            provider: "x".into(),
            bot_token: None,
            allowed_user_ids: vec![],
        };
        match channel_from_config("slack", &config) {
            Err(e) => assert!(e.to_string().contains("unsupported")),
            Ok(_) => panic!("expected error for unsupported channel"),
        }
    }

    #[test]
    fn telegram_send_fails_gracefully_with_missing_provider() {
        let ch = TelegramChannel::new("12345".into(), "/nonexistent/binary".into());
        let result = ch.send("hello");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to execute")
        );
    }
}
