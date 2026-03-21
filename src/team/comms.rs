//! External communication channels for user roles.
//!
//! The `user` role type communicates via channels (Telegram, Slack, etc.)
//! instead of tmux panes. Each channel provider is a CLI tool that the
//! daemon invokes for outbound messages.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::config::ChannelConfig;
use super::errors::DeliveryError;
use super::telegram::TelegramBot;

const TELEGRAM_DEDUP_TTL: Duration = Duration::from_secs(300);
const TELEGRAM_DEDUP_CAPACITY: usize = 512;

#[derive(Debug)]
struct RecentTelegramSends {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<VecDeque<(u64, Instant)>>,
}

impl RecentTelegramSends {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    fn prune_expired(&self, now: Instant, entries: &mut VecDeque<(u64, Instant)>) {
        while entries
            .front()
            .is_some_and(|(_, sent_at)| now.duration_since(*sent_at) > self.ttl)
        {
            entries.pop_front();
        }
        while entries.len() > self.capacity {
            entries.pop_front();
        }
    }

    fn contains_recent(&self, message_id: u64) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        self.prune_expired(now, &mut entries);
        entries.iter().any(|(id, _)| *id == message_id)
    }

    fn record(&self, message_id: u64) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        self.prune_expired(now, &mut entries);
        entries.push_back((message_id, now));
        self.prune_expired(now, &mut entries);
    }
}

fn telegram_message_id(target: &str, message: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut hasher);
    message.hash(&mut hasher);
    hasher.finish()
}

/// Trait for outbound message delivery to external channels.
pub trait Channel: Send + Sync {
    /// Send a text message to the channel destination.
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError>;
    /// Channel type identifier (e.g., "telegram").
    #[allow(dead_code)] // Reserved for diagnostics and provider-specific routing.
    fn channel_type(&self) -> &str;
}

/// Telegram channel via openclaw (or any CLI provider).
pub struct TelegramChannel {
    target: String,
    provider: String,
    recent_sends: RecentTelegramSends,
}

impl TelegramChannel {
    pub fn new(target: String, provider: String) -> Self {
        Self::with_dedup_settings(
            target,
            provider,
            TELEGRAM_DEDUP_TTL,
            TELEGRAM_DEDUP_CAPACITY,
        )
    }

    pub fn from_config(config: &ChannelConfig) -> Self {
        Self::new(config.target.clone(), config.provider.clone())
    }

    fn with_dedup_settings(
        target: String,
        provider: String,
        ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            target,
            provider,
            recent_sends: RecentTelegramSends::new(ttl, capacity),
        }
    }
}

impl Channel for TelegramChannel {
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
        let message_id = telegram_message_id(&self.target, message);
        if self.recent_sends.contains_recent(message_id) {
            debug!(target = %self.target, message_id, "suppressing duplicate telegram message");
            return Ok(());
        }

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
                self.recent_sends.record(message_id);
                debug!("telegram message sent successfully");
                Ok(())
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(status = ?out.status, stderr = %stderr, "telegram send failed");
                Err(DeliveryError::ChannelSend {
                    recipient: self.target.clone(),
                    detail: stderr.to_string(),
                })
            }
            Err(e) => {
                warn!(error = %e, provider = %self.provider, "failed to execute channel provider");
                Err(DeliveryError::ProviderExec {
                    provider: self.provider.clone(),
                    source: e,
                })
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
    recent_sends: RecentTelegramSends,
}

impl NativeTelegramChannel {
    pub fn new(bot: TelegramBot, target: String) -> Self {
        Self::with_dedup_settings(target, bot, TELEGRAM_DEDUP_TTL, TELEGRAM_DEDUP_CAPACITY)
    }

    /// Build from a `ChannelConfig`, returning `None` if no bot token is available.
    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        TelegramBot::from_config(config).map(|bot| Self::new(bot, config.target.clone()))
    }

    fn with_dedup_settings(
        target: String,
        bot: TelegramBot,
        ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            bot,
            target,
            recent_sends: RecentTelegramSends::new(ttl, capacity),
        }
    }
}

impl Channel for NativeTelegramChannel {
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
        let message_id = telegram_message_id(&self.target, message);
        if self.recent_sends.contains_recent(message_id) {
            debug!(
                target = %self.target,
                message_id,
                "suppressing duplicate native telegram message"
            );
            return Ok(());
        }

        debug!(target = %self.target, len = message.len(), "sending via native telegram channel");
        self.bot
            .send_message(&self.target, message)
            .map(|_| {
                self.recent_sends.record(message_id);
            })
            .map_err(|error| DeliveryError::ChannelSend {
                recipient: self.target.clone(),
                detail: error.to_string(),
            })
    }

    fn channel_type(&self) -> &str {
        "telegram-native"
    }
}

/// Create a channel from config fields.
pub fn channel_from_config(
    channel_type: &str,
    config: &ChannelConfig,
) -> std::result::Result<Box<dyn Channel>, DeliveryError> {
    match channel_type {
        "telegram" => {
            if let Some(native) = NativeTelegramChannel::from_config(config) {
                Ok(Box::new(native))
            } else {
                Ok(Box::new(TelegramChannel::from_config(config)))
            }
        }
        other => Err(DeliveryError::UnsupportedChannel {
            channel_type: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    #[test]
    fn telegram_message_id_changes_with_target_and_body() {
        let first = telegram_message_id("12345", "hello");
        let second = telegram_message_id("12345", "hello again");
        let third = telegram_message_id("67890", "hello");
        assert_ne!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn telegram_recent_sends_respects_ttl() {
        let cache = RecentTelegramSends::new(Duration::from_millis(50), 16);
        let id = telegram_message_id("12345", "hello");
        assert!(!cache.contains_recent(id));
        cache.record(id);
        assert!(cache.contains_recent(id));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!cache.contains_recent(id));
    }

    #[test]
    fn telegram_channel_suppresses_duplicate_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let script_path = tmp.path().join("fake-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
                log_path.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&script_path, perms).unwrap();

        let ch = TelegramChannel::with_dedup_settings(
            "12345".into(),
            script_path.display().to_string(),
            Duration::from_secs(60),
            16,
        );
        ch.send("hello").unwrap();
        ch.send("hello").unwrap();

        let lines = fs::read_to_string(&log_path).unwrap();
        assert_eq!(lines.lines().count(), 1);
    }

    #[test]
    fn telegram_channel_allows_unique_messages_and_retries_after_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let script_path = tmp.path().join("fake-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
                log_path.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&script_path, perms).unwrap();

        let ch = TelegramChannel::with_dedup_settings(
            "12345".into(),
            script_path.display().to_string(),
            Duration::from_millis(5),
            16,
        );
        ch.send("first").unwrap();
        ch.send("second").unwrap();
        std::thread::sleep(Duration::from_millis(10));
        ch.send("first").unwrap();

        let lines = fs::read_to_string(&log_path).unwrap();
        assert_eq!(lines.lines().count(), 3);
    }
}
