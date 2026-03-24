use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::{MessageDelivery, PendingMessage};
use crate::team::config::RoleType;
use crate::team::daemon::TeamDaemon;
use crate::team::errors::DeliveryError;
use crate::team::inbox;
use crate::team::message;

impl TeamDaemon {
    /// Drain pending messages for an agent that just became ready.
    /// Called from `poll_watchers()` when `ready_confirmed` transitions to true.
    pub(in crate::team) fn drain_pending_queue(&mut self, recipient: &str) -> Result<()> {
        let messages = self
            .pending_delivery_queue
            .remove(recipient)
            .unwrap_or_default();
        if messages.is_empty() {
            return Ok(());
        }
        info!(
            recipient,
            count = messages.len(),
            "draining pending delivery queue after agent became ready"
        );
        for msg in messages {
            self.deliver_message(&msg.from, recipient, &msg.body)?;
        }
        Ok(())
    }

    pub(in crate::team) fn queue_daemon_message(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let visible_sender = self.automation_sender_for(recipient);
        self.deliver_message(&visible_sender, recipient, body)
    }

    pub(in crate::team) fn queue_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<()> {
        self.deliver_message(from, recipient, body).map(|_| ())
    }

    fn deliver_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if let Some(channel) = self.channels.get(recipient) {
            let _ = channel;
            return self.deliver_channel_message(from, recipient, body);
        }

        // Shim delivery path: when use_shim is enabled and we have a handle for
        // this recipient, deliver via the structured shim channel.
        if self.config.team_config.use_shim {
            if let Some(handle) = self.shim_handles.get_mut(recipient) {
                if handle.is_ready() {
                    match handle.send_message(from, body) {
                        Ok(()) => {
                            info!(from, to = recipient, "delivered message via shim channel");
                            self.record_message_routed(from, recipient);
                            return Ok(MessageDelivery::LivePane);
                        }
                        Err(error) => {
                            warn!(
                                from,
                                to = recipient,
                                error = %error,
                                "shim channel delivery failed; falling through to inbox"
                            );
                        }
                    }
                } else if !handle.is_terminal() {
                    info!(
                        from,
                        to = recipient,
                        state = %handle.state,
                        "shim agent not ready; deferring to pending queue"
                    );
                    self.pending_delivery_queue
                        .entry(recipient.to_string())
                        .or_default()
                        .push(PendingMessage {
                            from: from.to_string(),
                            body: body.to_string(),
                            queued_at: Instant::now(),
                        });
                    return Ok(MessageDelivery::DeferredPending);
                }
                // Terminal state falls through to inbox
            }
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

        if let Some(pane_id) = self.config.pane_map.get(recipient).cloned() {
            // Readiness gate: check for the agent prompt before injecting.
            if !self.check_agent_ready(recipient, &pane_id) {
                // If the agent has *never* been ready (still starting up), buffer
                // in the pending queue — these will be drained when readiness is
                // confirmed.  If the agent was previously ready, fall through to
                // inbox delivery (existing behaviour for transient unreadiness).
                let never_been_ready = self
                    .watchers
                    .get(recipient)
                    .is_some_and(|w| !w.is_ready_for_delivery());
                if never_been_ready {
                    info!(
                        from,
                        to = recipient,
                        pane_id = pane_id.as_str(),
                        "agent still starting; deferring to pending queue"
                    );
                    self.pending_delivery_queue
                        .entry(recipient.to_string())
                        .or_default()
                        .push(PendingMessage {
                            from: from.to_string(),
                            body: body.to_string(),
                            queued_at: Instant::now(),
                        });
                    return Ok(MessageDelivery::DeferredPending);
                }
                info!(
                    from,
                    to = recipient,
                    pane_id = pane_id.as_str(),
                    "agent not ready after timeout; deferring to inbox"
                );
                // Fall through to inbox delivery below.
            } else {
                match message::inject_message(&pane_id, from, body) {
                    Ok(()) => {
                        self.record_message_routed(from, recipient);
                        let capture_lines = self.delivery_capture_lines_for(recipient);
                        self.verify_message_delivered_with_lines(
                            from,
                            recipient,
                            body,
                            3,
                            true,
                            capture_lines,
                        );
                        return Ok(MessageDelivery::LivePane);
                    }
                    Err(error) => {
                        warn!(
                            from,
                            to = recipient,
                            pane_id = pane_id.as_str(),
                            error = %error,
                            "live message delivery failed; queueing to inbox"
                        );
                        let _typed_error = DeliveryError::PaneInject {
                            recipient: recipient.to_string(),
                            pane_id,
                            detail: error.to_string(),
                        };
                    }
                }
            }
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send(from, recipient, body);
        inbox::deliver_to_inbox(&root, &msg).map_err(|error| DeliveryError::InboxQueue {
            recipient: recipient.to_string(),
            detail: error.to_string(),
        })?;
        self.record_message_routed(from, recipient);
        Ok(MessageDelivery::InboxQueued)
    }

    pub(in crate::team) fn drain_legacy_command_queue(&mut self) -> Result<()> {
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
                            let _ = channel;
                            self.deliver_channel_message(from, to, &formatted)?;
                        }
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

    pub(in crate::team) fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            let is_ready = self
                .watchers
                .get(name)
                .map(|watcher| {
                    matches!(
                        watcher.state,
                        super::super::watcher::WatcherState::Ready
                            | super::super::watcher::WatcherState::Idle
                    )
                })
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
                        self.manual_assign_cooldowns
                            .insert(name.to_string(), Instant::now());
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::super::{MessageDelivery, PendingMessage};
    use crate::team::AssignmentResultStatus;
    use crate::team::comms::Channel;
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::RoleType;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode,
        WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::message;

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
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
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
            telegram_bot: None,
            failure_tracker: FailureTracker::new(20),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            is_multi_repo: false,
            sub_repo_names: Vec::new(),
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            last_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
            shim_handles: HashMap::new(),
            last_shim_health_check: Instant::now(),
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
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
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
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
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
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
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
                    auto_respawn_on_crash: false,
                    shim_health_check_interval_secs: 60,
                    shim_health_timeout_secs: 120,
                    shim_shutdown_timeout_secs: 30,
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
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
    fn external_sender_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);

        daemon.config.team_config.external_senders = vec!["email-router".to_string()];
        daemon.config.team_config.roles = vec![RoleDef {
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
        }];
        daemon.config.members = vec![MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }];

        daemon
            .queue_message("email-router", "manager", "New email from user@example.com")
            .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "email-router");
        assert!(messages[0].body.contains("New email from user@example.com"));

        assert!(
            daemon
                .config
                .team_config
                .can_talk("email-router", "manager")
        );
    }

    // --- Readiness gate tests ---

    #[test]
    fn deliver_inbox_skips_agents_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "message should remain pending for active agent"
        );
    }

    #[test]
    fn deliver_inbox_delivers_to_ready_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        let _ = pending;
    }

    // --- Delivery to unknown recipient ---

    #[test]
    fn deliver_message_skips_unknown_recipient() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon
            .deliver_message("manager", "nonexistent-role", "hello")
            .unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Delivery to member without pane falls back to inbox ---

    #[test]
    fn deliver_message_to_member_without_pane_goes_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.members.push(MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        });

        let result = daemon
            .deliver_message("manager", "eng-2", "Go fix the bug")
            .unwrap();
        assert_eq!(result, MessageDelivery::InboxQueued);

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-2").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "manager");
        assert!(messages[0].body.contains("Go fix the bug"));
    }

    // --- Queue daemon message uses automation sender ---

    #[test]
    fn queue_daemon_message_to_unknown_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon.queue_daemon_message("nobody", "test msg").unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Resolve role name ---

    #[test]
    fn resolve_role_name_returns_human_for_human() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("human"), "human");
    }

    #[test]
    fn resolve_role_name_returns_daemon_for_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("daemon"), "daemon");
    }

    #[test]
    fn resolve_role_name_maps_member_to_role_name() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("eng-1"), "eng");
    }

    #[test]
    fn resolve_role_name_returns_input_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("unknown-member"), "unknown-member");
    }

    // --- Pending delivery queue tests (Task #276) ---

    #[test]
    fn pending_queue_buffers_message_when_agent_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        let result = daemon
            .deliver_message("manager", "eng-1", "task assignment")
            .unwrap();

        assert_eq!(
            result,
            MessageDelivery::DeferredPending,
            "message to starting agent must be deferred to pending queue"
        );
        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].from, "manager");
        assert_eq!(queue[0].body, "task assignment");
    }

    #[test]
    fn drain_pending_queue_delivers_when_agent_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "manager".to_string(),
                body: "queued assignment".to_string(),
                queued_at: Instant::now(),
            });

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true),
            "pending queue must be empty after drain"
        );

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "queued assignment");
    }

    #[test]
    fn drain_pending_queue_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.drain_pending_queue("eng-1").unwrap();
        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn multiple_messages_queued_and_drained_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        daemon.watchers.insert("eng-1".to_string(), watcher);

        for i in 1..=3u32 {
            let result = daemon
                .deliver_message("manager", "eng-1", &format!("msg-{i}"))
                .unwrap();
            assert_eq!(result, MessageDelivery::DeferredPending);
        }

        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].body, "msg-1");
        assert_eq!(queue[1].body, "msg-2");
        assert_eq!(queue[2].body, "msg-3");

        daemon.watchers.get_mut("eng-1").unwrap().confirm_ready();
        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );

        let root = inbox::inboxes_root(tmp.path());
        let inbox_msgs = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(inbox_msgs.len(), 3, "all queued messages must be delivered");
        let mut bodies: Vec<&str> = inbox_msgs.iter().map(|m| m.body.as_str()).collect();
        bodies.sort();
        assert_eq!(bodies, vec!["msg-1", "msg-2", "msg-3"]);
    }

    // --- Full pending queue lifecycle test (#289) ---

    #[test]
    fn pending_queue_full_lifecycle_buffer_transition_drain_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        let result = daemon
            .deliver_message("manager", "eng-1", "Task #42: implement feature")
            .unwrap();
        assert_eq!(
            result,
            MessageDelivery::DeferredPending,
            "message to starting agent must be deferred"
        );
        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(
            queue.len(),
            1,
            "pending queue must contain exactly one message"
        );
        assert_eq!(queue[0].from, "manager");
        assert_eq!(queue[0].body, "Task #42: implement feature");

        daemon.watchers.get_mut("eng-1").unwrap().confirm_ready();
        assert!(
            daemon
                .watchers
                .get("eng-1")
                .unwrap()
                .is_ready_for_delivery()
        );

        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true),
            "pending queue must be empty after drain"
        );

        let root = inbox::inboxes_root(tmp.path());
        let inbox_msgs = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            inbox_msgs.len(),
            1,
            "message must arrive in inbox after drain"
        );
        assert_eq!(inbox_msgs[0].body, "Task #42: implement feature");
        assert_eq!(inbox_msgs[0].from, "manager");
    }

    // -- Shim delivery tests --

    #[test]
    fn shim_delivery_sends_via_channel_when_ready() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.use_shim = true;

        // Create a shim handle in idle state
        let (parent, mut child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message("manager", "eng-1", "do the thing");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::LivePane);

        // Verify the command arrived on the child side
        let cmd: crate::shim::protocol::Command = crate::shim::protocol::Channel::new(child)
            .recv()
            .unwrap()
            .unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { from, body, .. } => {
                assert_eq!(from, "manager");
                assert_eq!(body, "do the thing");
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn shim_delivery_defers_when_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.use_shim = true;

        // Create a shim handle still in Starting state
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message("manager", "eng-1", "wait for me");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::DeferredPending);

        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].body, "wait for me");
    }

    #[test]
    fn shim_delivery_falls_through_when_use_shim_false() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        // use_shim defaults to false — shim path should be skipped

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        // With use_shim=false, should skip the shim path entirely
        let result = daemon.deliver_message("manager", "eng-1", "hello");
        assert!(result.is_ok());
        // eng-1 is not in pane_map either, so it becomes unknown
        assert_eq!(result.unwrap(), MessageDelivery::SkippedUnknownRecipient);
    }
}
