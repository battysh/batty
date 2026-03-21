//! Telemetry helpers for daemon event emission and orchestrator logging.
//!
//! This module keeps `daemon.rs` focused on control flow by centralizing the
//! structured events and append-only orchestrator logging that the daemon emits
//! while it runs.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use super::super::events::TeamEvent;
use super::super::failure_patterns;
use super::*;

impl TeamDaemon {
    pub(super) fn run_loop_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        if let Err(error) = action(self) {
            self.record_loop_step_error(step, &error.to_string());
        }
    }

    pub(super) fn emit_event(&mut self, event: TeamEvent) {
        self.failure_window.push(&event);
        if let Err(error) = self.event_sink.emit(event) {
            warn!(error = %error, "failed to write daemon event; continuing");
        }
    }

    pub(super) fn record_daemon_started(&mut self) {
        self.emit_event(TeamEvent::daemon_started());
    }

    pub(super) fn record_daemon_heartbeat(&mut self, uptime_secs: u64) {
        self.emit_event(TeamEvent::daemon_heartbeat(uptime_secs));
    }

    pub(super) fn record_daemon_stopped(&mut self, reason: &str, uptime_secs: u64) {
        self.emit_event(TeamEvent::daemon_stopped_with_reason(reason, uptime_secs));
        info!(reason = reason, uptime_secs = uptime_secs, "daemon stopped");
    }

    pub(super) fn record_loop_step_error(&mut self, step: &str, error: &str) {
        warn!(step, error = %error, "daemon loop step failed; continuing");
        self.emit_event(TeamEvent::loop_step_error(step, error));
    }

    pub(super) fn record_daemon_reloading(&mut self) {
        self.emit_event(TeamEvent::daemon_reloading());
    }

    pub(super) fn record_daemon_reloaded(&mut self) {
        self.emit_event(TeamEvent::daemon_reloaded());
    }

    pub(super) fn record_pattern_detected(&mut self, pattern_type: &str, frequency: u32) {
        self.emit_event(TeamEvent::pattern_detected(pattern_type, frequency));
    }

    pub(super) fn record_agent_spawned(&mut self, role: &str) {
        self.emit_event(TeamEvent::agent_spawned(role));
    }

    pub(super) fn record_member_crashed(&mut self, role: &str, restart: bool) {
        self.emit_event(TeamEvent::member_crashed(role, restart));
    }

    pub(super) fn record_agent_restarted(
        &mut self,
        role: &str,
        task: impl Into<String>,
        reason: &str,
        restart_count: u32,
    ) {
        let task = task.into();
        self.emit_event(TeamEvent::agent_restarted(
            role,
            &task,
            reason,
            restart_count,
        ));
    }

    pub(super) fn record_context_exhausted(
        &mut self,
        role: &str,
        task: Option<u32>,
        session_size_bytes: Option<u64>,
    ) {
        self.emit_event(TeamEvent::context_exhausted(role, task, session_size_bytes));
    }

    pub(crate) fn record_task_escalated(&mut self, role: &str, task: impl Into<String>) {
        let task = task.into();
        self.emit_event(TeamEvent::task_escalated(role, &task));
    }

    pub(super) fn record_task_unblocked(&mut self, role: &str, task: impl Into<String>) {
        let task = task.into();
        self.emit_event(TeamEvent::task_unblocked(role, &task));
    }

    pub(crate) fn record_task_completed(&mut self, role: &str) {
        self.emit_event(TeamEvent::task_completed(role));
    }

    pub(super) fn record_standup_generated(&mut self, recipient: &str) {
        self.emit_event(TeamEvent::standup_generated(recipient));
    }

    pub(super) fn record_retro_generated(&mut self) {
        self.emit_event(TeamEvent::retro_generated());
    }

    pub(crate) fn record_message_routed(&mut self, from: &str, to: &str) {
        self.emit_event(TeamEvent::message_routed(from, to));
    }

    pub(super) fn acknowledge_hot_reload_marker(&mut self) {
        if !consume_hot_reload_marker(&self.config.project_root) {
            return;
        }

        self.record_daemon_reloaded();
        self.record_orchestrator_action("runtime: daemon hot-reloaded");
        info!("daemon restarted via hot reload");
    }

    pub(super) fn maybe_notify_failure_patterns(&mut self) -> Result<()> {
        if !self.config.team_config.automation.failure_pattern_detection {
            return Ok(());
        }

        let patterns = self.failure_window.detect_failure_patterns();
        if patterns.is_empty() {
            return Ok(());
        }

        let notifications = failure_patterns::generate_pattern_notifications(&patterns, 3, 5);
        for (pattern, notification) in patterns
            .iter()
            .filter(|pattern| pattern.frequency >= 3)
            .zip(notifications)
        {
            let signature = format!(
                "{}:{}",
                pattern.pattern_type.as_str(),
                pattern.affected_entities.join(",")
            );
            let last_frequency = self
                .last_pattern_notifications
                .get(&signature)
                .copied()
                .unwrap_or(0);
            if notification.frequency <= last_frequency {
                continue;
            }
            self.last_pattern_notifications
                .insert(signature, notification.frequency);

            let managers: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Manager)
                .map(|member| member.name.clone())
                .collect();
            let architects: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Architect)
                .map(|member| member.name.clone())
                .collect();

            self.record_pattern_detected(
                notification.pattern_type.as_str(),
                notification.frequency,
            );

            if notification.notify_manager {
                for recipient in &managers {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }

            if notification.notify_architect {
                for recipient in &architects {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }
        }

        Ok(())
    }

    fn orchestrator_enabled(&self) -> bool {
        self.config.team_config.orchestrator_enabled()
    }

    pub(super) fn record_orchestrator_action(&self, action: impl AsRef<str>) {
        if !self.orchestrator_enabled() {
            return;
        }
        let path = super::super::orchestrator_log_path(&self.config.project_root);
        if let Err(error) = append_orchestrator_log_line(&path, action.as_ref()) {
            warn!(log = %path.display(), error = %error, "failed to append orchestrator log");
        }
    }
}

pub(super) fn append_orchestrator_log_line(path: &Path, message: &str) -> Result<()> {
    use std::io::Write;

    let mut file = super::super::open_log_for_append(path)?;
    writeln!(file, "[{}] {}", now_unix(), message)?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::{EventSink, read_events};
    use crate::team::test_helpers::{RecordingChannel, daemon_config_with_roles};
    use serial_test::serial;

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
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
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::from_writer(
                tmp.path().join("broken-events.jsonl").as_path(),
                FailingWriter,
            ),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

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
    fn maybe_notify_failure_patterns_routes_severe_patterns_to_manager_and_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
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
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("human".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
            },
        ];

        let mut daemon = TeamDaemon::new(config).unwrap();
        for index in 0..5 {
            let mut event = TeamEvent::task_escalated("eng-1", &format!("{}", 100 + index));
            event.ts = index as u64 + 1;
            daemon.emit_event(event);
        }

        daemon.maybe_notify_failure_patterns().unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let manager_messages = inbox::pending_messages(&root, "manager").unwrap();
        let architect_messages = inbox::pending_messages(&root, "architect").unwrap();

        assert_eq!(manager_messages.len(), 1);
        assert_eq!(architect_messages.len(), 1);
        assert!(manager_messages[0].body.contains("Review blockers"));
        assert!(architect_messages[0].body.contains("Review blockers"));
    }

    #[test]
    fn append_orchestrator_log_line_writes_timestamped_activity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".batty").join("orchestrator.log");
        append_orchestrator_log_line(&path, "dispatch: assigned task #18").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("dispatch: assigned task #18"));
        assert!(content.starts_with('['));
    }

    #[test]
    fn record_orchestrator_action_is_noop_when_orchestrator_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
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
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.record_orchestrator_action("dispatch: no-op");

        assert!(!tmp.path().join(".batty").join("orchestrator.log").exists());
    }

    #[test]
    fn record_orchestrator_action_writes_when_orchestrator_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Hybrid,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.record_orchestrator_action("dispatch: active");

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("dispatch: active"));
    }

    #[test]
    fn hot_reload_acknowledgement_emits_event_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        write_hot_reload_marker(tmp.path()).unwrap();

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Hybrid,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.acknowledge_hot_reload_marker();

        let events = read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| event.event == "daemon_reloaded"));

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("daemon hot-reloaded"));
        assert!(!hot_reload_marker_path(tmp.path()).exists());
    }

    #[test]
    #[serial]
    fn poll_watchers_emits_context_exhausted_event() {
        let session = format!("batty-test-context-exhausted-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        tmux::send_keys(&pane_id, "Conversation is too long to continue.", true).unwrap();
        tmux::send_keys(&pane_id, "prompt is too long", true).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: session.clone(),
                members: vec![engineer],
                pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
            },
            watchers: HashMap::from([(
                "eng-1".to_string(),
                SessionWatcher::new(&pane_id, "eng-1", 300, None),
            )]),
            states: HashMap::from([("eng-1".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.poll_watchers().unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        let events = read_events(&events_path).unwrap();
        let event = events
            .iter()
            .find(|event| event.event == "context_exhausted")
            .unwrap();
        assert_eq!(event.role.as_deref(), Some("eng-1"));
        assert_eq!(event.task.as_deref(), Some("42"));

        let _ = crate::tmux::kill_session(&session);
    }
}
