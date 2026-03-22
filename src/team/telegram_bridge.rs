//! Telegram bridge orchestration for the daemon poll loop.

use anyhow::Result;
use tracing::{debug, info, warn};

use super::*;

pub(super) fn build_telegram_bot(
    team_config: &TeamConfig,
) -> Option<super::super::telegram::TelegramBot> {
    team_config
        .roles
        .iter()
        .find(|role| {
            role.role_type == RoleType::User && role.channel.as_deref() == Some("telegram")
        })
        .and_then(|role| role.channel_config.as_ref())
        .and_then(super::super::telegram::TelegramBot::from_config)
}

impl TeamDaemon {
    pub(super) fn process_telegram_queue(&mut self) -> Result<()> {
        self.poll_telegram()?;
        self.deliver_user_inbox()
    }

    fn poll_telegram(&mut self) -> Result<()> {
        let Some(bot) = &mut self.telegram_bot else {
            return Ok(());
        };

        let messages = match bot.poll_updates() {
            Ok(msgs) => msgs,
            Err(error) => {
                debug!(error = %error, "telegram poll failed");
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
            .find(|role| role.role_type == RoleType::User)
            .map(|role| role.talks_to.clone())
            .unwrap_or_default();

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "telegram inbound"
            );

            for target in &targets {
                let inbox_msg = inbox::InboxMessage::new_send("human", target, &msg.text);
                if let Err(error) = inbox::deliver_to_inbox(&root, &inbox_msg) {
                    warn!(
                        to = %target,
                        error = %error,
                        "failed to deliver telegram message to inbox"
                    );
                }
            }

            self.record_message_routed("human", "telegram");
        }

        Ok(())
    }

    fn deliver_user_inbox(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let user_roles: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .filter(|role| role.role_type == RoleType::User)
            .map(|role| role.name.clone())
            .collect();

        for user_name in &user_roles {
            let messages = match inbox::pending_messages(&root, user_name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(user = %user_name, error = %error, "failed to read user inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            for msg in &messages {
                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                let send_result = match self.channels.get(user_name) {
                    Some(channel) => channel.send(&formatted),
                    None => {
                        debug!(user = %user_name, "no channel for user role");
                        break;
                    }
                };
                if let Err(error) = send_result {
                    warn!(to = %user_name, error = %error, "failed to send via channel");
                    continue;
                }

                if let Err(error) = inbox::mark_delivered(&root, user_name, &msg.id) {
                    warn!(user = %user_name, id = %msg.id, error = %error, "failed to mark delivered");
                }

                self.record_message_routed(&msg.from, user_name);
            }
        }

        Ok(())
    }

    pub(crate) fn automation_sender_for(&self, recipient: &str) -> String {
        let recipient_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == recipient);

        if let Some(member) = recipient_member {
            if let Some(parent) = &member.reports_to {
                return parent.clone();
            }
        }

        if let Some(sender) = &self.config.team_config.automation_sender {
            return sender.clone();
        }

        "daemon".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::team::comms::Channel;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, OrchestratorPosition, RoleDef, StandupConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::failure_patterns::FailureTracker;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_helpers::daemon_config_with_roles;

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

    fn backdate_idle_grace(daemon: &mut TeamDaemon, member_name: &str) {
        let grace = daemon.automation_idle_grace_duration() + Duration::from_secs(1);
        daemon
            .idle_started_at
            .insert(member_name.to_string(), Instant::now() - grace);
        if let Some(schedule) = daemon.nudges.get_mut(member_name) {
            schedule.idle_since = Some(Instant::now() - schedule.interval.max(grace));
        }
    }

    #[test]
    fn process_telegram_queue_delivers_pending_user_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: vec![RoleDef {
                    name: "human".to_string(),
                    role_type: RoleType::User,
                    agent: None,
                    instances: 1,
                    prompt: None,
                    talks_to: vec!["architect".to_string()],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    use_worktrees: false,
                }],
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("architect", "human", "Status update");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        daemon.process_telegram_queue().unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["--- Message from architect ---\nStatus update"]
        );
        assert!(inbox::pending_messages(&root, "human").unwrap().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let session = format!("batty-test-nudge-live-delivery-{}", std::process::id());
        let mut delivered_live = false;

        for _attempt in 0..5 {
            let _ = crate::tmux::kill_session(&session);

            crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
            let pane_id = match crate::tmux::pane_id(&session) {
                Ok(pane_id) if !pane_id.is_empty() => pane_id,
                _ => {
                    let _ = crate::tmux::kill_session(&session);
                    std::thread::sleep(Duration::from_millis(150));
                    continue;
                }
            };
            std::thread::sleep(Duration::from_millis(250));

            let tmp = tempfile::tempdir().unwrap();
            let member = MemberInstance {
                name: "scientist".to_string(),
                role_name: "scientist".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            };
            let mut watchers = HashMap::new();
            let mut scientist_watcher = SessionWatcher::new(&pane_id, "scientist", 300, None);
            scientist_watcher.confirm_ready();
            watchers.insert("scientist".to_string(), scientist_watcher);
            let mut daemon = TeamDaemon {
                config: DaemonConfig {
                    project_root: tmp.path().to_path_buf(),
                    team_config: TeamConfig {
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
                        event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                        retro_min_duration_secs: 60,
                        roles: Vec::new(),
                    },
                    session: session.clone(),
                    members: vec![member],
                    pane_map: HashMap::from([("scientist".to_string(), pane_id.clone())]),
                },
                watchers,
                states: HashMap::from([("scientist".to_string(), MemberState::Idle)]),
                idle_started_at: HashMap::new(),
                active_tasks: HashMap::new(),
                retry_counts: HashMap::new(),
                dispatch_queue: Vec::new(),
                triage_idle_epochs: HashMap::new(),
                triage_interventions: HashMap::new(),
                owned_task_interventions: HashMap::new(),
                intervention_cooldowns: HashMap::new(),
                channels: HashMap::new(),
                nudges: HashMap::from([(
                    "scientist".to_string(),
                    NudgeSchedule {
                        text: "Please make progress.".to_string(),
                        interval: Duration::from_secs(1),
                        idle_since: Some(Instant::now() - Duration::from_secs(5)),
                        fired_this_idle: false,
                        paused: false,
                    },
                )]),
                telegram_bot: None,
                failure_tracker: FailureTracker::new(20),
                event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
                paused_standups: HashSet::new(),
                last_standup: HashMap::new(),
                last_board_rotation: Instant::now(),
                last_auto_dispatch: Instant::now(),
                pipeline_starvation_fired: false,
                pipeline_starvation_last_fired: None,
                retro_generated: false,
                failed_deliveries: Vec::new(),
                review_first_seen: HashMap::new(),
                review_nudge_sent: HashSet::new(),
                poll_interval: Duration::from_secs(5),
                is_git_repo: false,
                subsystem_error_counts: HashMap::new(),
                auto_merge_overrides: HashMap::new(),
                recent_dispatches: HashMap::new(),
                telemetry_db: None,
                backend_health: HashMap::new(),
                last_health_check: Instant::now(),
            };

            backdate_idle_grace(&mut daemon, "scientist");
            daemon.maybe_fire_nudges().unwrap();

            if daemon.states.get("scientist") == Some(&MemberState::Working) {
                let schedule = daemon.nudges.get("scientist").unwrap();
                assert!(schedule.paused);
                assert!(schedule.idle_since.is_none());
                assert!(!schedule.fired_this_idle);
                delivered_live = true;
                crate::tmux::kill_session(&session).unwrap();
                break;
            }

            crate::tmux::kill_session(&session).unwrap();
            std::thread::sleep(Duration::from_millis(150));
        }

        assert!(
            delivered_live,
            "expected at least one successful live nudge delivery"
        );
    }

    #[test]
    #[serial_test::serial]
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let session = format!("batty-test-triage-live-delivery-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
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
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut watchers = HashMap::new();
        let mut lead_watcher = SessionWatcher::new(&pane_id, "lead", 300, None);
        lead_watcher.confirm_ready();
        watchers.insert("lead".to_string(), lead_watcher);
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
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
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: session.clone(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), pane_id.clone())]),
            },
            watchers,
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
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
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            backend_health: HashMap::new(),
            last_health_check: Instant::now(),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        if daemon.states.get("lead") == Some(&MemberState::Working) {
            let pane = (0..100)
                .find_map(|_| {
                    let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                    if pane.contains("batty send architect")
                        && pane.contains("next time you become idle")
                    {
                        Some(pane)
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                        None
                    }
                })
                .unwrap_or_else(|| tmux::capture_pane(&pane_id).unwrap_or_default());
            assert!(pane.contains("Triage backlog detected"));
            assert!(pane.contains("batty send architect"));
            assert!(pane.contains("next time you become idle"));
        } else {
            let pending = inbox::pending_messages(&root, "lead").unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].body.contains("batty inbox lead"));
        }

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn automation_sender_prefers_direct_manager_and_config_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    agent: None,
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: Some("human".to_string()),
                    external_senders: Vec::new(),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    cost: Default::default(),
                    event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                    retro_min_duration_secs: 60,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![
                    MemberInstance {
                        name: "architect".to_string(),
                        role_name: "architect".to_string(),
                        role_type: RoleType::Architect,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: None,
                        use_worktrees: false,
                    },
                    MemberInstance {
                        name: "lead".to_string(),
                        role_name: "lead".to_string(),
                        role_type: RoleType::Manager,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: Some("architect".to_string()),
                        use_worktrees: false,
                    },
                    MemberInstance {
                        name: "eng-1".to_string(),
                        role_name: "eng".to_string(),
                        role_type: RoleType::Engineer,
                        agent: Some("codex".to_string()),
                        prompt: None,
                        reports_to: Some("lead".to_string()),
                        use_worktrees: false,
                    },
                ],
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
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo: false,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db: None,
            backend_health: HashMap::new(),
            last_health_check: Instant::now(),
        };

        assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
        assert_eq!(daemon.automation_sender_for("lead"), "architect");
        assert_eq!(daemon.automation_sender_for("architect"), "human");

        daemon.config.team_config.automation_sender = None;
        assert_eq!(daemon.automation_sender_for("architect"), "daemon");
    }

    #[test]
    fn daemon_creates_telegram_bot_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            channel_config: Some(ChannelConfig {
                target: "12345".to_string(),
                provider: "telegram".to_string(),
                bot_token: Some("test-token-123".to_string()),
                allowed_user_ids: vec![42],
            }),
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.telegram_bot.is_some());
    }

    #[test]
    fn daemon_no_telegram_bot_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.telegram_bot.is_none());
    }
}
