//! Team configuration parsed from `.batty/team_config/team.yaml`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::DEFAULT_EVENT_LOG_MAX_BYTES;
use super::TEAM_CONFIG_DIR;

#[derive(Debug, Clone, Deserialize)]
pub struct TeamConfig {
    pub name: String,
    #[serde(default = "default_workflow_mode")]
    pub workflow_mode: WorkflowMode,
    #[serde(default)]
    pub board: BoardConfig,
    #[serde(default)]
    pub standup: StandupConfig,
    #[serde(default)]
    pub automation: AutomationConfig,
    #[serde(default)]
    pub automation_sender: Option<String>,
    #[serde(default = "default_orchestrator_pane")]
    pub orchestrator_pane: bool,
    #[serde(default)]
    pub orchestrator_position: OrchestratorPosition,
    #[serde(default)]
    pub layout: Option<LayoutConfig>,
    #[serde(default)]
    pub workflow_policy: WorkflowPolicy,
    #[serde(default)]
    pub cost: CostConfig,
    #[serde(default = "default_event_log_max_bytes")]
    pub event_log_max_bytes: u64,
    pub roles: Vec<RoleDef>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CostConfig {
    #[serde(default)]
    pub models: HashMap<String, ModelPricing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    #[serde(default)]
    pub cached_input_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_creation_input_usd_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_creation_5m_input_usd_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_creation_1h_input_usd_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_read_input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    #[serde(default)]
    pub reasoning_output_usd_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkflowPolicy {
    #[serde(default)]
    pub wip_limit_per_engineer: Option<u32>,
    #[serde(default)]
    pub wip_limit_per_reviewer: Option<u32>,
    #[serde(default = "default_pipeline_starvation_threshold")]
    pub pipeline_starvation_threshold: Option<usize>,
    #[serde(default = "default_escalation_threshold_secs")]
    pub escalation_threshold_secs: u64,
    #[serde(default = "default_review_timeout_secs")]
    pub review_timeout_secs: u64,
    #[serde(default)]
    pub auto_archive_done_after_secs: Option<u64>,
    #[serde(default)]
    pub capability_overrides: HashMap<String, Vec<String>>,
}

impl Default for WorkflowPolicy {
    fn default() -> Self {
        Self {
            wip_limit_per_engineer: None,
            wip_limit_per_reviewer: None,
            pipeline_starvation_threshold: default_pipeline_starvation_threshold(),
            escalation_threshold_secs: default_escalation_threshold_secs(),
            review_timeout_secs: default_review_timeout_secs(),
            auto_archive_done_after_secs: None,
            capability_overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    #[default]
    Legacy,
    Hybrid,
    WorkflowFirst,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorPosition {
    #[default]
    Bottom,
    Left,
}

impl WorkflowMode {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn legacy_runtime_enabled(self) -> bool {
        matches!(self, Self::Legacy | Self::Hybrid)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn workflow_state_primary(self) -> bool {
        matches!(self, Self::WorkflowFirst)
    }

    pub fn enables_runtime_surface(self) -> bool {
        matches!(self, Self::Hybrid | Self::WorkflowFirst)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Hybrid => "hybrid",
            Self::WorkflowFirst => "workflow_first",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoardConfig {
    #[serde(default = "default_rotation_threshold")]
    pub rotation_threshold: u32,
    #[serde(default = "default_board_auto_dispatch")]
    pub auto_dispatch: bool,
    #[serde(default = "default_dispatch_stabilization_delay_secs")]
    pub dispatch_stabilization_delay_secs: u64,
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            rotation_threshold: default_rotation_threshold(),
            auto_dispatch: default_board_auto_dispatch(),
            dispatch_stabilization_delay_secs: default_dispatch_stabilization_delay_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StandupConfig {
    #[serde(default = "default_standup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_output_lines")]
    pub output_lines: u32,
}

impl Default for StandupConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_standup_interval(),
            output_lines: default_output_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AutomationConfig {
    #[serde(default = "default_enabled")]
    pub timeout_nudges: bool,
    #[serde(default = "default_enabled")]
    pub standups: bool,
    #[serde(default = "default_enabled")]
    pub failure_pattern_detection: bool,
    #[serde(default = "default_enabled")]
    pub triage_interventions: bool,
    #[serde(default = "default_enabled")]
    pub review_interventions: bool,
    #[serde(default = "default_enabled")]
    pub owned_task_interventions: bool,
    #[serde(default = "default_enabled")]
    pub manager_dispatch_interventions: bool,
    #[serde(default = "default_enabled")]
    pub architect_utilization_interventions: bool,
    #[serde(default)]
    pub replenishment_threshold: Option<usize>,
    #[serde(default = "default_intervention_idle_grace_secs")]
    pub intervention_idle_grace_secs: u64,
    #[serde(default = "default_intervention_cooldown_secs")]
    pub intervention_cooldown_secs: u64,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            timeout_nudges: default_enabled(),
            standups: default_enabled(),
            failure_pattern_detection: default_enabled(),
            triage_interventions: default_enabled(),
            review_interventions: default_enabled(),
            owned_task_interventions: default_enabled(),
            manager_dispatch_interventions: default_enabled(),
            architect_utilization_interventions: default_enabled(),
            replenishment_threshold: None,
            intervention_idle_grace_secs: default_intervention_idle_grace_secs(),
            intervention_cooldown_secs: default_intervention_cooldown_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutConfig {
    pub zones: Vec<ZoneDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneDef {
    pub name: String,
    pub width_pct: u32,
    #[serde(default)]
    pub split: Option<SplitDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SplitDef {
    pub horizontal: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoleDef {
    pub name: String,
    pub role_type: RoleType,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "default_instances")]
    pub instances: u32,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub talks_to: Vec<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub channel_config: Option<ChannelConfig>,
    #[serde(default)]
    pub nudge_interval_secs: Option<u64>,
    #[serde(default)]
    pub receives_standup: Option<bool>,
    #[serde(default)]
    pub standup_interval_secs: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)] // Parsed for future ownership semantics but not yet enforced.
    pub owns: Vec<String>,
    #[serde(default)]
    pub use_worktrees: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub target: String,
    pub provider: String,
    /// Telegram bot token for native API (optional; falls back to provider CLI).
    /// Can also be set via `BATTY_TELEGRAM_BOT_TOKEN` env var.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// Telegram user IDs allowed to send messages (access control).
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningDirectiveFile {
    ReplenishmentContext,
    ReviewPolicy,
    EscalationPolicy,
}

impl PlanningDirectiveFile {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::ReplenishmentContext => "replenishment_context.md",
            Self::ReviewPolicy => "review_policy.md",
            Self::EscalationPolicy => "escalation_policy.md",
        }
    }

    pub fn path_for(self, project_root: &Path) -> PathBuf {
        project_root
            .join(".batty")
            .join(TEAM_CONFIG_DIR)
            .join(self.file_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleType {
    User,
    Architect,
    Manager,
    Engineer,
}

fn default_rotation_threshold() -> u32 {
    20
}

fn default_workflow_mode() -> WorkflowMode {
    WorkflowMode::Legacy
}

fn default_board_auto_dispatch() -> bool {
    true
}

fn default_dispatch_stabilization_delay_secs() -> u64 {
    30
}

fn default_standup_interval() -> u64 {
    300
}

fn default_pipeline_starvation_threshold() -> Option<usize> {
    Some(1)
}

fn default_output_lines() -> u32 {
    30
}

fn default_instances() -> u32 {
    1
}

fn default_escalation_threshold_secs() -> u64 {
    3600
}

fn default_review_timeout_secs() -> u64 {
    7200
}

fn default_enabled() -> bool {
    true
}

fn default_orchestrator_pane() -> bool {
    true
}

fn default_intervention_idle_grace_secs() -> u64 {
    60
}

fn default_intervention_cooldown_secs() -> u64 {
    120
}

fn default_event_log_max_bytes() -> u64 {
    DEFAULT_EVENT_LOG_MAX_BYTES
}

impl TeamConfig {
    pub fn orchestrator_enabled(&self) -> bool {
        self.workflow_mode.enables_runtime_surface() && self.orchestrator_pane
    }

    /// Check if a role is allowed to send messages to another role.
    ///
    /// Uses `talks_to` if configured. If `talks_to` is empty for a role,
    /// falls back to the default hierarchy:
    /// - User ↔ Architect
    /// - Architect ↔ Manager
    /// - Manager ↔ Engineer
    ///
    /// The `from` and `to` are role definition names (not member instance names).
    /// "human" is always allowed to talk to any role.
    pub fn can_talk(&self, from_role: &str, to_role: &str) -> bool {
        // human (CLI user) can always send to anyone
        if from_role == "human" {
            return true;
        }
        // daemon-generated messages (standups, nudges) always allowed
        if from_role == "daemon" {
            return true;
        }

        let from_def = self.roles.iter().find(|r| r.name == from_role);
        let Some(from_def) = from_def else {
            return false;
        };

        // If talks_to is explicitly configured, use it
        if !from_def.talks_to.is_empty() {
            return from_def.talks_to.iter().any(|t| t == to_role);
        }

        // Default hierarchy: user↔architect, architect↔manager, manager↔engineer
        let to_def = self.roles.iter().find(|r| r.name == to_role);
        let Some(to_def) = to_def else {
            return false;
        };

        matches!(
            (from_def.role_type, to_def.role_type),
            (RoleType::User, RoleType::Architect)
                | (RoleType::Architect, RoleType::User)
                | (RoleType::Architect, RoleType::Manager)
                | (RoleType::Manager, RoleType::Architect)
                | (RoleType::Manager, RoleType::Engineer)
                | (RoleType::Engineer, RoleType::Manager)
        )
    }

    /// Load team config from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: TeamConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(config)
    }

    /// Validate the team config. Returns an error if invalid.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("team name cannot be empty");
        }

        if self.roles.is_empty() {
            bail!("team must have at least one role");
        }

        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            if !role_names.insert(&role.name) {
                bail!("duplicate role name: '{}'", role.name);
            }

            if role.role_type != RoleType::User && role.agent.is_none() {
                bail!(
                    "role '{}' is not a user but has no agent configured",
                    role.name
                );
            }

            if role.role_type == RoleType::User && role.agent.is_some() {
                bail!(
                    "role '{}' is a user but has an agent configured; users use channels instead",
                    role.name
                );
            }

            if role.instances == 0 {
                bail!("role '{}' has zero instances", role.name);
            }
        }

        // Validate talks_to references exist
        for role in &self.roles {
            for target in &role.talks_to {
                if !role_names.contains(target.as_str()) {
                    bail!("role '{}' talks_to unknown role '{}'", role.name, target);
                }
            }
        }

        if let Some(sender) = &self.automation_sender
            && !role_names.contains(sender.as_str())
            && sender != "human"
        {
            bail!("automation_sender references unknown role '{}'", sender);
        }

        // Validate layout zones if present
        if let Some(layout) = &self.layout {
            let total_pct: u32 = layout.zones.iter().map(|z| z.width_pct).sum();
            if total_pct > 100 {
                bail!("layout zone widths sum to {}%, exceeds 100%", total_pct);
            }
        }

        Ok(())
    }
}

pub fn load_planning_directive(
    project_root: &Path,
    directive: PlanningDirectiveFile,
    max_chars: usize,
) -> Result<Option<String>> {
    let path = directive.path_for(project_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }

            let total_chars = trimmed.chars().count();
            let truncated = trimmed.chars().take(max_chars).collect::<String>();
            if total_chars > max_chars {
                Ok(Some(format!(
                    "{truncated}\n\n[truncated to {max_chars} chars from {}]",
                    directive.file_name()
                )))
            } else {
                Ok(Some(truncated))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read planning directive {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [manager]
"#
    }

    #[test]
    fn parse_minimal_config() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert_eq!(config.name, "test-team");
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert_eq!(config.roles.len(), 3);
        assert_eq!(config.roles[0].role_type, RoleType::Architect);
        assert_eq!(config.roles[2].instances, 3);
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(config.orchestrator_pane);
        assert_eq!(config.event_log_max_bytes, DEFAULT_EVENT_LOG_MAX_BYTES);
    }

    #[test]
    fn parse_config_with_user_role() {
        let yaml = r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "12345"
      provider: openclaw
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [human]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.roles[0].role_type, RoleType::User);
        assert_eq!(config.roles[0].channel.as_deref(), Some("telegram"));
        assert_eq!(
            config.roles[0].channel_config.as_ref().unwrap().provider,
            "openclaw"
        );
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(config.orchestrator_pane);
    }

    #[test]
    fn planning_directive_path_uses_team_config_directory() {
        let root = Path::new("/tmp/project");

        assert_eq!(
            PlanningDirectiveFile::ReviewPolicy.path_for(root),
            PathBuf::from("/tmp/project/.batty/team_config/review_policy.md")
        );
    }

    #[test]
    fn load_planning_directive_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();

        let loaded =
            load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();

        assert_eq!(loaded, None);
    }

    #[test]
    fn load_planning_directive_truncates_long_content() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("review_policy.md"),
            "abcdefghijklmnopqrstuvwxyz",
        )
        .unwrap();

        let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 10)
            .unwrap()
            .unwrap();

        assert!(loaded.starts_with("abcdefghij"));
        assert!(loaded.contains("[truncated to 10 chars"));
    }

    #[test]
    fn parse_full_config_with_layout() {
        let yaml = r#"
name: mafia-solver
board:
  rotation_threshold: 20
  auto_dispatch: false
workflow_mode: hybrid
orchestrator_pane: false
standup:
  interval_secs: 1200
  output_lines: 30
automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
layout:
  zones:
    - name: architect
      width_pct: 15
    - name: managers
      width_pct: 25
      split: { horizontal: 3 }
    - name: engineers
      width_pct: 60
      split: { horizontal: 15 }
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
    talks_to: [manager]
    nudge_interval_secs: 1800
    owns: ["planning/**", "docs/**"]
  - name: manager
    role_type: manager
    agent: claude
    instances: 3
    prompt: manager.md
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 5
    prompt: engineer.md
    talks_to: [manager]
    use_worktrees: true
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "mafia-solver");
        assert_eq!(config.board.rotation_threshold, 20);
        assert!(!config.board.auto_dispatch);
        assert_eq!(config.workflow_mode, WorkflowMode::Hybrid);
        assert!(!config.orchestrator_pane);
        assert_eq!(config.standup.interval_secs, 1200);
        let layout = config.layout.as_ref().unwrap();
        assert_eq!(layout.zones.len(), 3);
        assert_eq!(layout.zones[0].width_pct, 15);
        assert_eq!(layout.zones[2].split.as_ref().unwrap().horizontal, 15);
        assert_eq!(config.event_log_max_bytes, DEFAULT_EVENT_LOG_MAX_BYTES);
    }

    #[test]
    fn defaults_applied() {
        let yaml = r#"
name: minimal
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert_eq!(config.board.rotation_threshold, 20);
        assert!(config.board.auto_dispatch);
        assert_eq!(config.standup.interval_secs, 300);
        assert_eq!(config.standup.output_lines, 30);
        assert!(config.automation.timeout_nudges);
        assert!(config.automation.standups);
        assert!(config.automation.failure_pattern_detection);
        assert!(config.automation.triage_interventions);
        assert_eq!(config.automation.intervention_idle_grace_secs, 60);
        assert!(config.cost.models.is_empty());
        assert_eq!(config.roles[0].instances, 1);
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(config.orchestrator_pane);
        assert_eq!(config.event_log_max_bytes, DEFAULT_EVENT_LOG_MAX_BYTES);
    }

    #[test]
    fn parse_event_log_max_bytes_override() {
        let yaml = r#"
name: test
event_log_max_bytes: 2048
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.event_log_max_bytes, 2048);
    }

    #[test]
    fn parse_cost_config() {
        let yaml = r#"
name: test-team
cost:
  models:
    gpt-5.4:
      input_usd_per_mtok: 2.5
      cached_input_usd_per_mtok: 0.25
      output_usd_per_mtok: 15.0
    claude-opus-4-6:
      input_usd_per_mtok: 15.0
      cache_creation_5m_input_usd_per_mtok: 18.75
      cache_creation_1h_input_usd_per_mtok: 30.0
      cache_read_input_usd_per_mtok: 1.5
      output_usd_per_mtok: 75.0
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let gpt = config.cost.models.get("gpt-5.4").unwrap();
        assert_eq!(gpt.input_usd_per_mtok, 2.5);
        assert_eq!(gpt.cached_input_usd_per_mtok, 0.25);
        assert_eq!(gpt.output_usd_per_mtok, 15.0);

        let claude = config.cost.models.get("claude-opus-4-6").unwrap();
        assert_eq!(claude.input_usd_per_mtok, 15.0);
        assert_eq!(claude.cache_creation_5m_input_usd_per_mtok, Some(18.75));
        assert_eq!(claude.cache_creation_1h_input_usd_per_mtok, Some(30.0));
        assert_eq!(claude.cache_read_input_usd_per_mtok, 1.5);
        assert_eq!(claude.output_usd_per_mtok, 75.0);
    }

    #[test]
    fn parse_workflow_mode_legacy_when_absent() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();

        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(config.workflow_mode.legacy_runtime_enabled());
        assert!(!config.workflow_mode.workflow_state_primary());
    }

    #[test]
    fn parse_workflow_mode_hybrid_from_yaml() {
        let yaml = format!("workflow_mode: hybrid\n{}", minimal_yaml());
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(config.workflow_mode, WorkflowMode::Hybrid);
        assert!(config.workflow_mode.legacy_runtime_enabled());
        assert!(!config.workflow_mode.workflow_state_primary());
    }

    #[test]
    fn parse_workflow_mode_workflow_first_from_yaml() {
        let yaml = format!("workflow_mode: workflow_first\n{}", minimal_yaml());
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(config.workflow_mode, WorkflowMode::WorkflowFirst);
        assert!(!config.workflow_mode.legacy_runtime_enabled());
        assert!(config.workflow_mode.workflow_state_primary());
    }

    #[test]
    fn parse_explicit_automation_config() {
        let yaml = r#"
name: test
automation:
  timeout_nudges: false
  standups: true
  failure_pattern_detection: false
  triage_interventions: true
  review_interventions: false
  owned_task_interventions: true
  manager_dispatch_interventions: false
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 90
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.automation.timeout_nudges);
        assert!(config.automation.standups);
        assert!(!config.automation.failure_pattern_detection);
        assert!(config.automation.triage_interventions);
        assert!(!config.automation.review_interventions);
        assert!(config.automation.owned_task_interventions);
        assert!(!config.automation.manager_dispatch_interventions);
        assert!(config.automation.architect_utilization_interventions);
        assert_eq!(config.automation.intervention_idle_grace_secs, 90);
    }

    #[test]
    fn parse_workflow_mode_variants() {
        let legacy: TeamConfig = serde_yaml::from_str(
            r#"
name: test
workflow_mode: legacy
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();
        assert_eq!(legacy.workflow_mode, WorkflowMode::Legacy);

        let hybrid: TeamConfig = serde_yaml::from_str(
            r#"
name: test
workflow_mode: hybrid
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();
        assert_eq!(hybrid.workflow_mode, WorkflowMode::Hybrid);

        let workflow_first: TeamConfig = serde_yaml::from_str(
            r#"
name: test
workflow_mode: workflow_first
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();
        assert_eq!(workflow_first.workflow_mode, WorkflowMode::WorkflowFirst);
    }

    #[test]
    fn orchestrator_enabled_respects_mode_and_pane_flag() {
        let legacy: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(!legacy.orchestrator_enabled());

        let hybrid_enabled: TeamConfig = serde_yaml::from_str(&format!(
            "workflow_mode: hybrid\norchestrator_pane: true\n{}",
            minimal_yaml()
        ))
        .unwrap();
        assert!(hybrid_enabled.orchestrator_enabled());

        let hybrid_disabled: TeamConfig = serde_yaml::from_str(&format!(
            "workflow_mode: hybrid\norchestrator_pane: false\n{}",
            minimal_yaml()
        ))
        .unwrap();
        assert!(!hybrid_disabled.orchestrator_enabled());

        let workflow_first_enabled: TeamConfig = serde_yaml::from_str(&format!(
            "workflow_mode: workflow_first\norchestrator_pane: true\n{}",
            minimal_yaml()
        ))
        .unwrap();
        assert!(workflow_first_enabled.orchestrator_enabled());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let yaml = r#"
name: ""
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_rejects_duplicate_role_names() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn validate_rejects_non_user_without_agent() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("no agent"));
    }

    #[test]
    fn validate_rejects_user_with_agent() {
        let yaml = r#"
name: test
roles:
  - name: human
    role_type: user
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("user") && err.contains("agent"));
    }

    #[test]
    fn validate_rejects_unknown_talks_to() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    talks_to: [nonexistent]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("unknown role"));
    }

    #[test]
    fn validate_rejects_unknown_automation_sender() {
        let yaml = r#"
name: test
automation_sender: nonexistent
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("automation_sender"));
        assert!(err.contains("unknown role"));
    }

    #[test]
    fn validate_accepts_known_automation_sender() {
        let yaml = r#"
name: test
automation_sender: human
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "12345"
      provider: openclaw
  - name: architect
    role_type: architect
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_layout_over_100_pct() {
        let yaml = r#"
name: test
layout:
  zones:
    - name: a
      width_pct: 60
    - name: b
      width_pct: 50
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("100%"));
    }

    #[test]
    fn validate_accepts_minimal_config() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn load_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(&path, minimal_yaml()).unwrap();
        let config = TeamConfig::load(&path).unwrap();
        assert_eq!(config.name, "test-team");
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    }

    #[test]
    fn can_talk_default_hierarchy() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();

        // Default: architect↔manager, manager↔engineer
        assert!(config.can_talk("architect", "manager"));
        assert!(config.can_talk("manager", "architect"));
        assert!(config.can_talk("manager", "engineer"));
        assert!(config.can_talk("engineer", "manager"));

        // architect↔engineer blocked by default
        assert!(!config.can_talk("architect", "engineer"));
        assert!(!config.can_talk("engineer", "architect"));

        // human can talk to anyone
        assert!(config.can_talk("human", "architect"));
        assert!(config.can_talk("human", "engineer"));

        // daemon can talk to anyone
        assert!(config.can_talk("daemon", "engineer"));
    }

    #[test]
    fn can_talk_explicit_talks_to() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    talks_to: [manager, engineer]
  - name: manager
    role_type: manager
    agent: claude
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [manager]
"#,
        )
        .unwrap();

        // Explicit: architect→engineer allowed
        assert!(config.can_talk("architect", "engineer"));
        // But engineer→architect still blocked (not in engineer's talks_to)
        assert!(!config.can_talk("engineer", "architect"));
    }

    #[test]
    fn validate_rejects_zero_instances() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: 0
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("zero instances"));
    }

    #[test]
    fn parse_rejects_malformed_yaml_missing_colon() {
        let yaml = r#"
name test
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

        let err = serde_yaml::from_str::<TeamConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_rejects_malformed_yaml_bad_indentation() {
        let yaml = r#"
name: test
roles:
- name: worker
   role_type: engineer
   agent: codex
"#;

        let err = serde_yaml::from_str::<TeamConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_rejects_missing_name_field() {
        let yaml = r#"
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

        let err = serde_yaml::from_str::<TeamConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("name"));
    }

    #[test]
    fn parse_rejects_missing_roles_field() {
        let yaml = r#"
name: test
"#;

        let err = serde_yaml::from_str::<TeamConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("roles"));
    }

    #[test]
    fn legacy_mode_with_orchestrator_pane_true_disables_orchestrator_surface() {
        let yaml = r#"
name: test
workflow_mode: legacy
orchestrator_pane: true
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        assert!(config.orchestrator_pane);
        assert!(!config.orchestrator_enabled());
    }

    #[test]
    fn parse_all_automation_flags_false() {
        let yaml = r#"
name: test
automation:
  timeout_nudges: false
  standups: false
  failure_pattern_detection: false
  triage_interventions: false
  review_interventions: false
  owned_task_interventions: false
  manager_dispatch_interventions: false
  architect_utilization_interventions: false
  replenishment_threshold: 1
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.automation.timeout_nudges);
        assert!(!config.automation.standups);
        assert!(!config.automation.failure_pattern_detection);
        assert!(!config.automation.triage_interventions);
        assert!(!config.automation.review_interventions);
        assert!(!config.automation.owned_task_interventions);
        assert!(!config.automation.manager_dispatch_interventions);
        assert!(!config.automation.architect_utilization_interventions);
        assert_eq!(config.automation.replenishment_threshold, Some(1));
    }

    #[test]
    fn automation_replenishment_threshold_defaults_to_none() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert_eq!(config.automation.replenishment_threshold, None);
    }

    #[test]
    fn parse_standup_interval_zero() {
        let yaml = r#"
name: test
standup:
  interval_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.standup.interval_secs, 0);
    }

    #[test]
    fn parse_standup_interval_u64_max() {
        let yaml = format!(
            r#"
name: test
standup:
  interval_secs: {}
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
            u64::MAX
        );

        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config.standup.interval_secs, u64::MAX);
    }

    #[test]
    fn parse_ignores_unknown_top_level_fields_for_forward_compatibility() {
        let yaml = r#"
name: test
future_flag: true
future_section:
  nested_value: 42
roles:
  - name: worker
    role_type: engineer
    agent: codex
    extra_role_setting: keep-going
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "test");
        assert_eq!(config.roles.len(), 1);
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_duplicate_role_names_with_mixed_role_types() {
        let yaml = r#"
name: test
roles:
  - name: lead
    role_type: architect
    agent: claude
  - name: lead
    role_type: manager
    agent: claude
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate role name"));
    }

    #[test]
    fn validate_rejects_talks_to_reference_to_missing_role() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    talks_to: [manager]
"#;

        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("talks_to unknown role"));
    }
}
