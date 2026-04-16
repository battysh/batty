use std::time::Instant;

use anyhow::Result;
use tracing::warn;

use super::MessageDelivery;
use crate::team::daemon::TeamDaemon;
use crate::team::errors::DeliveryError;
use crate::team::inbox;
use crate::team::retry::{RetryConfig, retry_sync};

const TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
const TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN: std::time::Duration =
    std::time::Duration::from_secs(300);

impl TeamDaemon {
    pub(in crate::team) fn telegram_failure_key(recipient: &str) -> String {
        format!("telegram-delivery-failures::{recipient}")
    }

    pub(in crate::team) fn telegram_circuit_breaker_key(recipient: &str) -> String {
        format!("telegram-delivery-breaker::{recipient}")
    }

    pub(in crate::team) fn telegram_retry_config() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            jitter: false,
        }
    }

    pub(in crate::team) fn telegram_channel_paused(&self, recipient: &str) -> bool {
        self.intervention_cooldowns
            .get(&Self::telegram_circuit_breaker_key(recipient))
            .is_some_and(|opened_at| {
                opened_at.elapsed() < TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN
            })
    }

    pub(in crate::team) fn clear_telegram_delivery_failures(&mut self, recipient: &str) {
        self.retry_counts
            .remove(&Self::telegram_failure_key(recipient));
        self.intervention_cooldowns
            .remove(&Self::telegram_circuit_breaker_key(recipient));
    }

    pub(in crate::team) fn increment_telegram_delivery_failures(&mut self, recipient: &str) -> u32 {
        let failures = self
            .retry_counts
            .entry(Self::telegram_failure_key(recipient))
            .or_insert(0);
        *failures += 1;
        *failures
    }

    fn alert_telegram_delivery_paused(
        &mut self,
        recipient: &str,
        from: &str,
        detail: &str,
    ) -> Result<()> {
        let Some(manager) = self.failed_delivery_escalation_recipient(recipient) else {
            warn!(
                recipient,
                from, detail, "telegram delivery paused without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Telegram delivery paused after repeated failures.\nRecipient: {recipient}\nFrom: {from}\nLast error: {detail}"
        );
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send("daemon", &manager, &body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.record_message_routed("daemon", &manager);
        Ok(())
    }

    pub(in crate::team) fn deliver_channel_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let channel_type = self
            .channels
            .get(recipient)
            .map(|channel| channel.channel_type().to_string())
            .unwrap_or_default();

        if !channel_type.starts_with("telegram") {
            self.channels
                .get(recipient)
                .expect("channel presence checked by caller")
                .send(body)?;
            self.record_message_routed(from, recipient);
            return Ok(MessageDelivery::Channel);
        }

        if self.telegram_channel_paused(recipient) {
            return Err(DeliveryError::ChannelSend {
                recipient: recipient.to_string(),
                detail: "telegram delivery circuit breaker is open".to_string(),
            }
            .into());
        }

        let send_result = {
            let channel = self
                .channels
                .get(recipient)
                .expect("channel presence checked by caller");
            retry_sync(&Self::telegram_retry_config(), || channel.send(body))
        };

        match send_result {
            Ok(()) => {
                self.clear_telegram_delivery_failures(recipient);
                self.record_message_routed(from, recipient);
                Ok(MessageDelivery::Channel)
            }
            Err(error) => {
                let failure_count = self.increment_telegram_delivery_failures(recipient);
                if failure_count >= TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD {
                    self.intervention_cooldowns.insert(
                        Self::telegram_circuit_breaker_key(recipient),
                        Instant::now(),
                    );
                    self.alert_telegram_delivery_paused(recipient, from, &error.to_string())?;
                }
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::super::MessageDelivery;
    use super::TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN;
    use super::TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD;
    use crate::team::comms::Channel;
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::RoleType;
    use crate::team::config::{
        AutomationConfig, BoardConfig, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;

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

    struct FailingChannel;

    impl Channel for FailingChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            Err(DeliveryError::ChannelSend {
                recipient: "test-recipient".to_string(),
                detail: "synthetic channel failure".to_string(),
            })
        }

        fn channel_type(&self) -> &str {
            "test-failing"
        }
    }

    struct SequencedTelegramChannel {
        results: Arc<Mutex<VecDeque<std::result::Result<(), DeliveryError>>>>,
        attempts: Arc<Mutex<u32>>,
    }

    impl Channel for SequencedTelegramChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            *self.attempts.lock().unwrap() += 1;
            self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
        }

        fn channel_type(&self) -> &str {
            "telegram-test"
        }
    }

    fn empty_legacy_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: crate::team::config::TeamConfig {
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
            last_shim_health_check: Instant::now(),
            merge_queue: crate::team::daemon::MergeQueue::default(),
            last_binary_freshness_check: Instant::now(),
            last_tiered_inbox_sweep: Instant::now(),
        }
    }

    #[test]
    fn telegram_delivery_retries_transient_channel_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let attempts = Arc::new(Mutex::new(0));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from([
                    Err(DeliveryError::ChannelSend {
                        recipient: "human".to_string(),
                        detail: "429 too many requests".to_string(),
                    }),
                    Err(DeliveryError::ChannelSend {
                        recipient: "human".to_string(),
                        detail: "timeout while sending".to_string(),
                    }),
                    Ok(()),
                ]))),
                attempts: Arc::clone(&attempts),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(*attempts.lock().unwrap(), 3);
        assert_eq!(
            daemon
                .retry_counts
                .get(&TeamDaemon::telegram_failure_key("human")),
            None
        );
    }

    #[test]
    fn telegram_delivery_circuit_breaker_alerts_manager_after_repeated_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: crate::team::config::TeamConfig {
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
                members: vec![manager],
                pane_map: HashMap::new(),
            },
            ..empty_legacy_daemon(&tmp)
        };
        let attempts = Arc::new(Mutex::new(0));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from(
                    (0..32)
                        .map(|_| {
                            Err(DeliveryError::ChannelSend {
                                recipient: "human".to_string(),
                                detail: "429 too many requests".to_string(),
                            })
                        })
                        .collect::<Vec<_>>(),
                ))),
                attempts: Arc::clone(&attempts),
            }),
        );

        for _ in 0..TELEGRAM_DELIVERY_CIRCUIT_BREAKER_THRESHOLD {
            assert!(
                daemon
                    .queue_daemon_message("human", "Still failing")
                    .is_err()
            );
        }

        assert!(daemon.telegram_channel_paused("human"));
        let pending = inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Telegram delivery paused"));

        let before = *attempts.lock().unwrap();
        assert!(
            daemon
                .queue_daemon_message("human", "Breaker open")
                .is_err()
        );
        assert_eq!(*attempts.lock().unwrap(), before);
    }

    // --- Telegram circuit breaker key generation ---

    #[test]
    fn telegram_failure_key_contains_recipient() {
        let key = TeamDaemon::telegram_failure_key("human");
        assert_eq!(key, "telegram-delivery-failures::human");
    }

    #[test]
    fn telegram_circuit_breaker_key_contains_recipient() {
        let key = TeamDaemon::telegram_circuit_breaker_key("user-1");
        assert_eq!(key, "telegram-delivery-breaker::user-1");
    }

    // --- Telegram retry config ---

    #[test]
    fn telegram_retry_config_has_expected_defaults() {
        let config = TeamDaemon::telegram_retry_config();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.base_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 1_000);
        assert!(!config.jitter);
    }

    // --- Telegram channel paused ---

    #[test]
    fn telegram_channel_not_paused_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = empty_legacy_daemon(&tmp);
        assert!(!daemon.telegram_channel_paused("human"));
    }

    #[test]
    fn telegram_channel_paused_when_breaker_open() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now(),
        );
        assert!(daemon.telegram_channel_paused("human"));
    }

    #[test]
    fn telegram_channel_not_paused_after_cooldown_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now() - TELEGRAM_DELIVERY_CIRCUIT_BREAKER_COOLDOWN - Duration::from_secs(1),
        );
        assert!(!daemon.telegram_channel_paused("human"));
    }

    // --- Clear telegram delivery failures ---

    #[test]
    fn clear_telegram_delivery_failures_removes_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon
            .retry_counts
            .insert(TeamDaemon::telegram_failure_key("human"), 3);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("human"),
            Instant::now(),
        );

        daemon.clear_telegram_delivery_failures("human");

        assert!(
            !daemon
                .retry_counts
                .contains_key(&TeamDaemon::telegram_failure_key("human"))
        );
        assert!(
            !daemon
                .intervention_cooldowns
                .contains_key(&TeamDaemon::telegram_circuit_breaker_key("human"))
        );
    }

    // --- Increment telegram delivery failures ---

    #[test]
    fn increment_telegram_delivery_failures_starts_at_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let count = daemon.increment_telegram_delivery_failures("human");
        assert_eq!(count, 1);
    }

    #[test]
    fn increment_telegram_delivery_failures_accumulates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.increment_telegram_delivery_failures("human");
        daemon.increment_telegram_delivery_failures("human");
        let count = daemon.increment_telegram_delivery_failures("human");
        assert_eq!(count, 3);
    }

    // --- Non-telegram channel delivery ---

    #[test]
    fn deliver_channel_message_records_routing_event() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "user".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let result = daemon
            .deliver_channel_message("eng-1", "user", "Status update")
            .unwrap();
        assert_eq!(result, MessageDelivery::Channel);
        assert_eq!(sent.lock().unwrap().as_slice(), ["Status update"]);
    }

    // --- Deliver channel with failing non-telegram channel ---

    #[test]
    fn deliver_channel_message_failing_non_telegram_channel_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon
            .channels
            .insert("user".to_string(), Box::new(FailingChannel));

        let result = daemon.deliver_channel_message("eng-1", "user", "test");
        assert!(result.is_err());
    }

    // --- Telegram circuit breaker blocks further attempts ---

    #[test]
    fn telegram_delivery_blocked_when_circuit_breaker_open() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "user".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );
        // Simulate telegram channel type by using SequencedTelegramChannel
        daemon.channels.insert(
            "tg-user".to_string(),
            Box::new(SequencedTelegramChannel {
                results: Arc::new(Mutex::new(VecDeque::from([
                    Ok(()),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                ]))),
                attempts: Arc::new(Mutex::new(0)),
            }),
        );
        // Open circuit breaker for tg-user
        daemon.intervention_cooldowns.insert(
            TeamDaemon::telegram_circuit_breaker_key("tg-user"),
            Instant::now(),
        );

        let result = daemon.deliver_channel_message("eng-1", "tg-user", "blocked msg");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("circuit breaker is open"));
    }

    // --- Error path tests (Task #265) ---

    #[test]
    fn telegram_circuit_breaker_key_format() {
        let key = TeamDaemon::telegram_circuit_breaker_key("eng-1");
        assert!(key.contains("eng-1"));
        assert!(key.starts_with("telegram-delivery-breaker::"));
    }

    #[test]
    fn telegram_failure_key_format() {
        let key = TeamDaemon::telegram_failure_key("manager");
        assert!(key.contains("manager"));
        assert!(key.starts_with("telegram-delivery-failures::"));
    }

    #[test]
    fn telegram_retry_config_has_sensible_defaults() {
        let config = TeamDaemon::telegram_retry_config();
        assert!(config.max_retries >= 1);
        assert!(config.max_delay_ms > config.base_delay_ms);
    }

    #[test]
    fn telegram_channel_not_paused_initially() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = empty_legacy_daemon(&tmp);
        assert!(!daemon.telegram_channel_paused("eng-1"));
    }
}
