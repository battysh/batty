use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use uuid::Uuid;

use super::config::{
    AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, RoleType, StandupConfig,
    TeamConfig, WorkflowMode, WorkflowPolicy,
};
use super::daemon::{DaemonConfig, TeamDaemon};
use super::hierarchy::MemberInstance;
use super::inbox::{self, InboxMessage};
use super::standup::MemberState;

pub struct TestHarness {
    project_root: PathBuf,
    team_config: TeamConfig,
    session: String,
    members: Vec<MemberInstance>,
    pane_map: HashMap<String, String>,
    availability: HashMap<String, MemberState>,
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHarness {
    pub fn new() -> Self {
        let project_root =
            std::env::temp_dir().join(format!("batty-test-harness-{}", Uuid::new_v4()));
        std::fs::create_dir_all(super::team_config_dir(&project_root)).unwrap();
        Self {
            project_root,
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
                orchestrator_pane: false,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                event_log_max_bytes: super::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
            availability: HashMap::new(),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn board_tasks_dir(&self) -> PathBuf {
        super::team_config_dir(&self.project_root)
            .join("board")
            .join("tasks")
    }

    pub fn inbox_root(&self) -> PathBuf {
        inbox::inboxes_root(&self.project_root)
    }

    pub fn with_roles(mut self, roles: Vec<RoleDef>) -> Self {
        self.team_config.roles = roles;
        self
    }

    pub fn with_member(mut self, member: MemberInstance) -> Self {
        self.members.push(member);
        self
    }

    pub fn with_members(mut self, members: Vec<MemberInstance>) -> Self {
        self.members.extend(members);
        self
    }

    pub fn with_availability(mut self, availability: HashMap<String, MemberState>) -> Self {
        self.availability = availability;
        self
    }

    pub fn with_member_state(mut self, member: &str, state: MemberState) -> Self {
        self.availability.insert(member.to_string(), state);
        self
    }

    pub fn with_pane(mut self, member: &str, pane_id: &str) -> Self {
        self.pane_map
            .insert(member.to_string(), pane_id.to_string());
        self
    }

    pub fn with_board_task(
        self,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
    ) -> Self {
        let tasks_dir = self.board_tasks_dir();
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let mut content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\nclass: standard\n"
        );
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        content.push_str("---\n\nTask description.\n");

        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
        self
    }

    pub fn with_inbox_message(self, member: &str, message: InboxMessage, delivered: bool) -> Self {
        let inbox_root = self.inbox_root();
        inbox::init_inbox(&inbox_root, member).unwrap();
        let id = inbox::deliver_to_inbox(&inbox_root, &message).unwrap();
        if delivered {
            inbox::mark_delivered(&inbox_root, member, &id).unwrap();
        }
        self
    }

    pub fn availability_map(&self) -> HashMap<String, MemberState> {
        self.availability.clone()
    }

    pub fn pending_inbox_messages(&self, member: &str) -> Result<Vec<InboxMessage>> {
        inbox::pending_messages(&self.inbox_root(), member)
    }

    pub fn build_daemon(&self) -> Result<TeamDaemon> {
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: self.project_root.clone(),
            team_config: self.team_config.clone(),
            session: self.session.clone(),
            members: self.members.clone(),
            pane_map: self.pane_map.clone(),
        })?;
        daemon.states = self.availability.clone();
        // Test panes are assumed to be already running — pre-confirm readiness so
        // that delivery goes through the normal inject-then-inbox path rather than
        // the pending-delivery-queue path (which is only for freshly-spawned agents).
        for watcher in daemon.watchers.values_mut() {
            watcher.confirm_ready();
        }
        Ok(daemon)
    }

    pub fn daemon_member_count(&self, daemon: &TeamDaemon) -> usize {
        daemon.config.members.len()
    }

    pub fn daemon_state(&self, daemon: &TeamDaemon, member: &str) -> Option<MemberState> {
        daemon.states.get(member).copied()
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project_root);
    }
}

pub fn architect_member(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
    }
}

pub fn manager_member(name: &str, reports_to: Option<&str>) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: reports_to.map(str::to_string),
        use_worktrees: false,
    }
}

pub fn engineer_member(
    name: &str,
    reports_to: Option<&str>,
    use_worktrees: bool,
) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: reports_to.map(str::to_string),
        use_worktrees,
    }
}
