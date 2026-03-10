//! External communication channels for user roles.
//!
//! The `user` role type communicates via channels (Telegram, Slack, etc.)
//! instead of tmux panes. Each channel provider is a CLI tool that the
//! daemon invokes for outbound messages.

use anyhow::{Result, bail};
use tracing::{debug, warn};

use super::config::ChannelConfig;

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
            .args(["message", "send", "--to", &self.target, "--message", message])
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

/// Create a channel from config fields.
pub fn channel_from_config(
    channel_type: &str,
    config: &ChannelConfig,
) -> Result<Box<dyn Channel>> {
    match channel_type {
        "telegram" => Ok(Box::new(TelegramChannel::from_config(config))),
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
    fn channel_from_config_telegram() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
        };
        let ch = channel_from_config("telegram", &config).unwrap();
        assert_eq!(ch.channel_type(), "telegram");
    }

    #[test]
    fn channel_from_config_unknown_type() {
        let config = ChannelConfig {
            target: "x".into(),
            provider: "x".into(),
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
        assert!(result.unwrap_err().to_string().contains("failed to execute"));
    }
}
