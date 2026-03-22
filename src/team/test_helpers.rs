use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::team::comms::Channel;
use crate::team::config::{
    AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, StandupConfig, TeamConfig,
    WorkflowMode, WorkflowPolicy,
};
use crate::team::daemon::{DaemonConfig, TeamDaemon};
use crate::team::errors::DeliveryError;
use crate::team::events::{EventSink, TeamEvent};
use crate::team::hierarchy::MemberInstance;

pub(crate) struct RecordingChannel {
    pub(crate) messages: Arc<Mutex<Vec<String>>>,
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

pub(crate) fn team_config_with_roles(roles: Vec<RoleDef>) -> TeamConfig {
    TeamConfig {
        name: "test".to_string(),
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
        roles,
    }
}

pub(crate) fn daemon_config(project_root: &Path, members: Vec<MemberInstance>) -> DaemonConfig {
    DaemonConfig {
        project_root: project_root.to_path_buf(),
        team_config: team_config_with_roles(Vec::new()),
        session: "test".to_string(),
        members,
        pane_map: HashMap::new(),
    }
}

pub(crate) fn daemon_config_with_roles(project_root: &Path, roles: Vec<RoleDef>) -> DaemonConfig {
    DaemonConfig {
        project_root: project_root.to_path_buf(),
        team_config: team_config_with_roles(roles),
        session: "test".to_string(),
        members: Vec::new(),
        pane_map: HashMap::new(),
    }
}

pub(crate) fn make_test_daemon(project_root: &Path, members: Vec<MemberInstance>) -> TeamDaemon {
    TeamDaemon::new(daemon_config(project_root, members)).unwrap()
}

pub(crate) fn write_event_log(project_root: &Path, events: &[TeamEvent]) {
    let events_path = project_root
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let mut sink = EventSink::new(&events_path).unwrap();
    for event in events {
        sink.emit(event.clone()).unwrap();
    }
}
