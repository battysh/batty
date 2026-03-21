use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::config::RoleType;
use super::daemon::TeamDaemon;
use super::inbox;
use super::message;
use crate::tmux;

pub(super) const DELIVERY_VERIFICATION_CAPTURE_LINES: u32 = 50;
pub(super) const FAILED_DELIVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
pub(super) const FAILED_DELIVERY_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub(super) struct FailedDelivery {
    pub(super) recipient: String,
    pub(super) from: String,
    pub(super) body: String,
    pub(super) attempts: u32,
    pub(super) last_attempt: Instant,
}

impl FailedDelivery {
    pub(super) fn new(recipient: &str, from: &str, body: &str) -> Self {
        Self {
            recipient: recipient.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            attempts: 1,
            last_attempt: Instant::now(),
        }
    }

    pub(super) fn message_marker(&self) -> String {
        message_delivery_marker(&self.from)
    }

    fn is_ready_for_retry(&self, now: Instant) -> bool {
        now.duration_since(self.last_attempt) >= FAILED_DELIVERY_RETRY_DELAY
    }

    fn has_attempts_remaining(&self) -> bool {
        self.attempts < FAILED_DELIVERY_MAX_ATTEMPTS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageDelivery {
    Channel,
    LivePane,
    InboxQueued,
    SkippedUnknownRecipient,
}

impl TeamDaemon {
    fn verify_message_content_in_pane(&self, pane_id: &str, message_marker: &str) -> bool {
        match tmux::capture_pane_recent(pane_id, DELIVERY_VERIFICATION_CAPTURE_LINES) {
            Ok(capture) => capture_contains_message_marker(&capture, message_marker),
            Err(error) => {
                warn!(
                    pane_id,
                    error = %error,
                    "failed to capture pane for content-based delivery verification"
                );
                false
            }
        }
    }

    fn record_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        if let Some(existing) = self.failed_deliveries.iter_mut().find(|delivery| {
            delivery.recipient == recipient && delivery.from == from && delivery.body == body
        }) {
            existing.last_attempt = Instant::now();
            return;
        }

        self.failed_deliveries
            .push(FailedDelivery::new(recipient, from, body));
        self.record_delivery_failed(recipient, from, "message delivery failed after retries");
    }

    fn clear_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        self.failed_deliveries.retain(|delivery| {
            delivery.recipient != recipient || delivery.from != from || delivery.body != body
        });
    }

    fn failed_delivery_escalation_recipient(&self, recipient: &str) -> Option<String> {
        self.config
            .members
            .iter()
            .find(|member| member.name == recipient)
            .and_then(|member| member.reports_to.clone())
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| {
                        member.role_type == RoleType::Manager && member.name != recipient
                    })
                    .map(|member| member.name.clone())
            })
            .or_else(|| {
                let sender = self.automation_sender_for(recipient);
                (sender != recipient
                    && self
                        .config
                        .members
                        .iter()
                        .any(|member| member.name == sender))
                .then_some(sender)
            })
    }

    fn escalate_failed_delivery(&mut self, delivery: &FailedDelivery) -> Result<()> {
        let Some(manager) = self.failed_delivery_escalation_recipient(&delivery.recipient) else {
            warn!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                "failed delivery exhausted retries without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Live message delivery failed after {} attempts.\nRecipient: {}\nFrom: {}\nMarker: {}\nMessage body:\n{}",
            delivery.attempts,
            delivery.recipient,
            delivery.from,
            delivery.message_marker(),
            delivery.body
        );
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send("daemon", &manager, &body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.record_message_routed("daemon", &manager);
        warn!(
            recipient = %delivery.recipient,
            from = %delivery.from,
            escalation_target = %manager,
            attempts = delivery.attempts,
            "failed delivery escalated to manager inbox"
        );
        Ok(())
    }

    pub(super) fn retry_failed_deliveries(&mut self) -> Result<()> {
        if self.failed_deliveries.is_empty() {
            return Ok(());
        }

        let now = Instant::now();
        let pending = std::mem::take(&mut self.failed_deliveries);
        for mut delivery in pending {
            if !delivery.is_ready_for_retry(now) {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let is_ready = self
                .watchers
                .get(&delivery.recipient)
                .map(|watcher| matches!(watcher.state, super::watcher::WatcherState::Idle))
                .unwrap_or(true);
            if !is_ready {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&delivery.recipient).cloned() else {
                self.escalate_failed_delivery(&delivery)?;
                continue;
            };

            delivery.attempts += 1;
            delivery.last_attempt = now;
            info!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                attempts = delivery.attempts,
                "retrying failed live delivery"
            );

            let injected = match message::inject_message(&pane_id, &delivery.from, &delivery.body) {
                Ok(()) => true,
                Err(error) => {
                    warn!(
                        recipient = %delivery.recipient,
                        from = %delivery.from,
                        attempts = delivery.attempts,
                        error = %error,
                        "failed to re-inject message during delivery retry"
                    );
                    false
                }
            };

            if injected
                && self.verify_message_delivered(
                    &delivery.from,
                    &delivery.recipient,
                    &delivery.body,
                    3,
                    false,
                )
            {
                continue;
            }

            if delivery.has_attempts_remaining() {
                self.failed_deliveries.push(delivery);
            } else {
                self.escalate_failed_delivery(&delivery)?;
            }
        }

        Ok(())
    }

    fn verify_message_delivered(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
        max_attempts: u32,
        record_failure: bool,
    ) -> bool {
        let Some(pane_id) = self.config.pane_map.get(recipient).cloned() else {
            return true;
        };
        let message_marker = message_delivery_marker(from);

        for attempt in 1..=max_attempts {
            std::thread::sleep(Duration::from_secs(2));

            if self.verify_message_content_in_pane(&pane_id, &message_marker) {
                self.clear_failed_delivery(recipient, from, body);
                debug!(
                    recipient,
                    attempt,
                    marker = %message_marker,
                    "message delivery verified: marker found in pane"
                );
                return true;
            }

            warn!(
                recipient,
                attempt,
                marker = %message_marker,
                "message marker missing after injection; resending Enter"
            );
            if let Err(error) = tmux::send_keys(&pane_id, "", true) {
                warn!(recipient, error = %error, "failed to resend Enter");
            }
        }

        if record_failure {
            self.record_failed_delivery(recipient, from, body);
            warn!(
                recipient,
                max_attempts,
                marker = %message_marker,
                "message delivery failed after retries; queued for daemon retry"
            );
        }

        false
    }

    pub(super) fn queue_daemon_message(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let visible_sender = self.automation_sender_for(recipient);
        self.deliver_message(&visible_sender, recipient, body)
    }

    pub(super) fn queue_message(&mut self, from: &str, recipient: &str, body: &str) -> Result<()> {
        self.deliver_message(from, recipient, body).map(|_| ())
    }

    fn deliver_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if let Some(channel) = self.channels.get(recipient) {
            channel.send(body)?;
            self.record_message_routed(from, recipient);
            return Ok(MessageDelivery::Channel);
        }

        let known_recipient = self.config.pane_map.contains_key(recipient)
            || self
                .config
                .members
                .iter()
                .any(|member| member.name == recipient);
        if !known_recipient {
            debug!(from, recipient, "skipping message for unknown recipient");
            return Ok(MessageDelivery::SkippedUnknownRecipient);
        }

        if let Some(pane_id) = self.config.pane_map.get(recipient) {
            match message::inject_message(pane_id, from, body) {
                Ok(()) => {
                    self.record_message_routed(from, recipient);
                    self.verify_message_delivered(from, recipient, body, 3, true);
                    return Ok(MessageDelivery::LivePane);
                }
                Err(error) => {
                    warn!(
                        from,
                        to = recipient,
                        error = %error,
                        "live message delivery failed; queueing to inbox"
                    );
                }
            }
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send(from, recipient, body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.record_message_routed(from, recipient);
        Ok(MessageDelivery::InboxQueued)
    }

    pub(super) fn drain_legacy_command_queue(&mut self) -> Result<()> {
        let queue_path = message::command_queue_path(&self.config.project_root);
        let commands = message::read_command_queue(&queue_path)?;
        if commands.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let mut remaining_commands = Vec::new();
        for cmd in commands {
            let result: Result<()> = (|| match &cmd {
                message::QueuedCommand::Send {
                    from,
                    to,
                    message: msg,
                } => {
                    let is_user =
                        self.config.team_config.roles.iter().any(|role| {
                            role.name == to.as_str() && role.role_type == RoleType::User
                        });

                    if is_user {
                        if let Some(channel) = self.channels.get(to.as_str()) {
                            let formatted = format!("[From {from}]\n{msg}");
                            channel.send(&formatted)?;
                        }
                        self.record_message_routed(from, to);
                    } else {
                        let inbox_msg = inbox::InboxMessage::new_send(from, to, msg);
                        inbox::deliver_to_inbox(&root, &inbox_msg)?;
                        debug!(from, to, "legacy command routed to inbox");
                    }
                    Ok(())
                }
                message::QueuedCommand::Assign {
                    from,
                    engineer,
                    task,
                } => {
                    let msg = inbox::InboxMessage::new_assign(from, engineer, task);
                    inbox::deliver_to_inbox(&root, &msg)?;
                    debug!(engineer, "legacy assign routed to inbox");
                    Ok(())
                }
            })();

            if let Err(error) = result {
                warn!(error = %error, "failed to process legacy command; preserving in queue");
                remaining_commands.push(cmd);
            }
        }

        message::write_command_queue(&queue_path, &remaining_commands)?;
        Ok(())
    }

    pub(super) fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            let is_ready = self
                .watchers
                .get(name)
                .map(|watcher| matches!(watcher.state, super::watcher::WatcherState::Idle))
                .unwrap_or(true);

            if !is_ready {
                continue;
            }

            let messages = match inbox::pending_messages(&root, name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(member = %name, error = %error, "failed to read inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(name).cloned() else {
                continue;
            };

            let mut delivered_any = false;
            for msg in &messages {
                let from_role = self.resolve_role_name(&msg.from);
                let to_role = self.resolve_role_name(name);
                if !self.config.team_config.can_talk(&from_role, &to_role) {
                    warn!(
                        from = %msg.from, from_role, to = %name, to_role,
                        "blocked message: routing not allowed"
                    );
                    let _ = inbox::mark_delivered(&root, name, &msg.id);
                    continue;
                }

                let is_send = matches!(msg.msg_type, inbox::MessageType::Send);
                let delivery_result = match msg.msg_type {
                    inbox::MessageType::Send => {
                        info!(from = %msg.from, to = %name, id = %msg.id, "delivering inbox message");
                        message::inject_message(&pane_id, &msg.from, &msg.body)
                    }
                    inbox::MessageType::Assign => {
                        info!(to = %name, id = %msg.id, "delivering inbox assignment");
                        self.assign_task(name, &msg.body).map(|launch| {
                            self.record_assignment_success(name, &msg.id, &msg.body, &launch);
                            self.notify_assignment_sender_success(
                                &msg.from, name, &msg.id, &msg.body, &launch,
                            );
                        })
                    }
                };

                let mut mark_delivered = false;
                match delivery_result {
                    Ok(()) => {
                        delivered_any = true;
                        mark_delivered = true;
                        if is_send {
                            self.verify_message_delivered(&msg.from, name, &msg.body, 3, true);
                        }
                    }
                    Err(error) => {
                        warn!(
                            from = %msg.from,
                            to = %name,
                            id = %msg.id,
                            error = %error,
                            "failed to deliver inbox message"
                        );
                        if matches!(msg.msg_type, inbox::MessageType::Assign) {
                            mark_delivered = true;
                            self.record_assignment_failure(name, &msg.id, &msg.body, &error);
                            self.notify_assignment_sender_failure(
                                &msg.from, name, &msg.id, &msg.body, &error,
                            );
                        }
                    }
                }

                if !mark_delivered {
                    continue;
                }

                if let Err(error) = inbox::mark_delivered(&root, name, &msg.id) {
                    warn!(
                        member = %name,
                        id = %msg.id,
                        error = %error,
                        "failed to mark delivered"
                    );
                } else {
                    self.record_message_routed(&msg.from, name);
                }

                std::thread::sleep(Duration::from_secs(1));
            }

            if delivered_any {
                self.mark_member_working(name);
            }
        }

        Ok(())
    }

    fn resolve_role_name(&self, member_name: &str) -> String {
        if member_name == "human" || member_name == "daemon" {
            return member_name.to_string();
        }
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .map(|member| member.role_name.clone())
            .unwrap_or_else(|| member_name.to_string())
    }
}

pub(super) fn message_delivery_marker(sender: &str) -> String {
    format!("--- Message from {sender} ---")
}

pub(super) fn capture_contains_message_marker(capture: &str, message_marker: &str) -> bool {
    capture.contains(message_marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::sync::{Arc, Mutex};

    use crate::team::AssignmentResultStatus;
    use crate::team::comms::Channel;
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode,
        WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> Result<()> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    struct FailingChannel;

    impl Channel for FailingChannel {
        fn send(&self, _message: &str) -> Result<()> {
            bail!("synthetic channel failure")
        }

        fn channel_type(&self) -> &str {
            "test-failing"
        }
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    fn empty_legacy_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
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
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        }
    }

    fn failed_delivery_test_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };

        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![architect, manager, engineer],
                pane_map: HashMap::from([("eng-1".to_string(), "%9999999".to_string())]),
            },
            ..empty_legacy_daemon(tmp)
        }
    }

    #[test]
    fn queue_daemon_message_routes_to_channel_for_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(sent.lock().unwrap().as_slice(), ["Assignment delivered."]);
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.event_sink = EventSink::from_writer(
            tmp.path().join("broken-events.jsonl").as_path(),
            FailingWriter,
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Event sink can fail without breaking delivery.")
            .unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Event sink can fail without breaking delivery."]
        );
    }

    #[test]
    fn drain_legacy_command_queue_preserves_failed_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue_path = message::command_queue_path(tmp.path());
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Send {
                from: "architect".into(),
                to: "human".into(),
                message: "status".into(),
            },
        )
        .unwrap();
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #7: recover".into(),
            },
        )
        .unwrap();

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    roles: vec![RoleDef {
                        name: "human".to_string(),
                        role_type: RoleType::User,
                        agent: None,
                        instances: 1,
                        prompt: None,
                        talks_to: vec![],
                        channel: Some("telegram".to_string()),
                        channel_config: Some(ChannelConfig {
                            target: "123".to_string(),
                            provider: "fake".to_string(),
                            bot_token: None,
                            allowed_user_ids: vec![],
                        }),
                        nudge_interval_secs: None,
                        receives_standup: None,
                        standup_interval_secs: None,
                        owns: Vec::new(),
                        use_worktrees: false,
                    }],
                },
                session: "test".to_string(),
                members: vec![MemberInstance {
                    name: "eng-1".to_string(),
                    role_name: "eng-1".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                }],
                pane_map: HashMap::new(),
            },
            channels: HashMap::from([(
                "human".to_string(),
                Box::new(FailingChannel) as Box<dyn Channel>,
            )]),
            ..empty_legacy_daemon(&tmp)
        };

        daemon.drain_legacy_command_queue().unwrap();

        let remaining = message::read_command_queue(&queue_path).unwrap();
        assert_eq!(remaining.len(), 1);
        match &remaining[0] {
            message::QueuedCommand::Send { to, message, .. } => {
                assert_eq!(to, "human");
                assert_eq!(message, "status");
            }
            other => panic!("expected failed send command to remain queued, got {other:?}"),
        }

        let engineer_pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        assert_eq!(engineer_pending[0].from, "manager");
        assert!(engineer_pending[0].body.contains("Task #7: recover"));
    }

    #[test]
    fn deliver_inbox_messages_reports_failed_assignment_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
        ];

        let mut pane_map = HashMap::new();
        pane_map.insert("eng-1".to_string(), "%999".to_string());

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: super::super::config::TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map,
        })
        .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let assign = inbox::InboxMessage::new_assign("manager", "eng-1", "Task #13: fix it");
        let id = inbox::deliver_to_inbox(&root, &assign).unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(engineer_pending.is_empty());

        let engineer_all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            engineer_all
                .iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );

        let manager_pending = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(manager_pending.len(), 1);
        assert_eq!(manager_pending[0].from, "daemon");
        assert!(manager_pending[0].body.contains("Assignment failed."));
        assert!(manager_pending[0].body.contains("Engineer: eng-1"));
        assert!(manager_pending[0].body.contains("Message ID:"));
        let result = crate::team::load_assignment_result(tmp.path(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(result.status, AssignmentResultStatus::Failed);
        assert_eq!(result.engineer, "eng-1");
        assert_eq!(daemon.states.get("eng-1"), None);
    }

    #[test]
    fn queue_message_falls_back_to_inbox_when_live_delivery_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: super::super::config::TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![manager],
                pane_map: HashMap::from([("manager".to_string(), "%999".to_string())]),
            },
            ..empty_legacy_daemon(&tmp)
        };

        daemon
            .queue_message("eng-1", "manager", "Need review on merge handling.")
            .unwrap();

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "eng-1");
        assert!(messages[0].body.contains("Need review on merge handling."));
    }

    #[test]
    fn delivery_confirm_marker_detection_matches_captured_text() {
        let marker = message_delivery_marker("manager");
        let capture = format!("prompt\n{marker}\nbody\n");
        assert!(capture_contains_message_marker(&capture, &marker));
        assert!(!capture_contains_message_marker("prompt only", &marker));
    }

    #[test]
    fn delivery_confirm_marker_generation_uses_sender_header() {
        assert_eq!(
            message_delivery_marker("eng-1-4"),
            "--- Message from eng-1-4 ---"
        );
    }

    #[test]
    fn failed_delivery_new_sets_expected_fields() {
        let delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        assert_eq!(delivery.recipient, "eng-1");
        assert_eq!(delivery.from, "manager");
        assert_eq!(delivery.body, "Please retry this.");
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.message_marker(), "--- Message from manager ---");
        assert!(delivery.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_emits_single_health_event_per_unique_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");
        daemon.record_failed_delivery("eng-1", "manager", "Please retry this.");

        let events = super::super::events::read_events(&tmp.path().join("events.jsonl")).unwrap();
        let delivery_failed = events
            .into_iter()
            .filter(|event| event.event == "delivery_failed")
            .collect::<Vec<_>>();
        assert_eq!(delivery_failed.len(), 1);
        assert_eq!(delivery_failed[0].role.as_deref(), Some("eng-1"));
        assert_eq!(delivery_failed[0].from.as_deref(), Some("manager"));
    }

    #[test]
    fn failed_delivery_retry_requeues_before_attempt_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert_eq!(daemon.failed_deliveries.len(), 1);
        assert_eq!(daemon.failed_deliveries[0].attempts, 2);
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn failed_delivery_retry_respects_attempt_cap_and_escalates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert!(daemon.failed_deliveries.is_empty());
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(
            messages[0]
                .body
                .contains("Live message delivery failed after 3 attempts.")
        );
        assert!(messages[0].body.contains("Recipient: eng-1"));
    }
}
