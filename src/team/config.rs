//! Team configuration parsed from `.batty/team_config/team.yaml`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::DEFAULT_EVENT_LOG_MAX_BYTES;
use super::TEAM_CONFIG_DIR;
use crate::agent;

#[derive(Debug, Clone, Deserialize)]
pub struct TeamConfig {
    pub name: String,
    /// Team-level default agent backend. Individual roles can override this
    /// with their own `agent` field. Resolution order:
    /// role-level agent > team-level agent > "claude" (hardcoded default).
    #[serde(default)]
    pub agent: Option<String>,
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
    /// External senders (e.g. email-router, slack-bridge) that are allowed to
    /// message any role even though they are not team members.
    #[serde(default)]
    pub external_senders: Vec<String>,
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
    #[serde(default = "default_retro_min_duration_secs")]
    pub retro_min_duration_secs: u64,
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
    #[serde(default = "default_review_nudge_threshold_secs")]
    pub review_nudge_threshold_secs: u64,
    #[serde(default = "default_review_timeout_secs")]
    pub review_timeout_secs: u64,
    #[serde(default)]
    pub review_timeout_overrides: HashMap<String, ReviewTimeoutOverride>,
    #[serde(default)]
    pub auto_archive_done_after_secs: Option<u64>,
    #[serde(default)]
    pub capability_overrides: HashMap<String, Vec<String>>,
    #[serde(default = "default_stall_threshold_secs")]
    pub stall_threshold_secs: u64,
    #[serde(default = "default_max_stall_restarts")]
    pub max_stall_restarts: u32,
    #[serde(default = "default_health_check_interval_secs")]
    pub health_check_interval_secs: u64,
    #[serde(default = "default_uncommitted_warn_threshold")]
    pub uncommitted_warn_threshold: usize,
    #[serde(default)]
    pub auto_merge: AutoMergePolicy,
}

/// Per-priority override for review timeout thresholds.
/// When a task's priority matches a key in `review_timeout_overrides`,
/// these values replace the global defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewTimeoutOverride {
    /// Nudge threshold override (seconds). Falls back to global if absent.
    pub review_nudge_threshold_secs: Option<u64>,
    /// Escalation threshold override (seconds). Falls back to global if absent.
    pub review_timeout_secs: Option<u64>,
}

impl Default for WorkflowPolicy {
    fn default() -> Self {
        Self {
            wip_limit_per_engineer: None,
            wip_limit_per_reviewer: None,
            pipeline_starvation_threshold: default_pipeline_starvation_threshold(),
            escalation_threshold_secs: default_escalation_threshold_secs(),
            review_nudge_threshold_secs: default_review_nudge_threshold_secs(),
            review_timeout_secs: default_review_timeout_secs(),
            review_timeout_overrides: HashMap::new(),
            auto_archive_done_after_secs: None,
            capability_overrides: HashMap::new(),
            stall_threshold_secs: default_stall_threshold_secs(),
            max_stall_restarts: default_max_stall_restarts(),
            health_check_interval_secs: default_health_check_interval_secs(),
            uncommitted_warn_threshold: default_uncommitted_warn_threshold(),
            auto_merge: AutoMergePolicy::default(),
        }
    }
}

fn default_sensitive_paths() -> Vec<String> {
    vec![
        "Cargo.toml".to_string(),
        "team.yaml".to_string(),
        ".env".to_string(),
    ]
}

#[derive(Debug, Clone, Deserialize)]
pub struct AutoMergePolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: usize,
    #[serde(default = "default_max_files_changed")]
    pub max_files_changed: usize,
    #[serde(default = "default_max_modules_touched")]
    pub max_modules_touched: usize,
    #[serde(default = "default_sensitive_paths")]
    pub sensitive_paths: Vec<String>,
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: f64,
    #[serde(default = "default_require_tests_pass")]
    pub require_tests_pass: bool,
}

fn default_max_diff_lines() -> usize {
    200
}
fn default_max_files_changed() -> usize {
    5
}
fn default_max_modules_touched() -> usize {
    2
}
fn default_confidence_threshold() -> f64 {
    0.8
}
fn default_require_tests_pass() -> bool {
    true
}

impl Default for AutoMergePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_diff_lines: default_max_diff_lines(),
            max_files_changed: default_max_files_changed(),
            max_modules_touched: default_max_modules_touched(),
            sensitive_paths: default_sensitive_paths(),
            confidence_threshold: default_confidence_threshold(),
            require_tests_pass: default_require_tests_pass(),
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
    #[serde(default = "default_dispatch_dedup_window_secs")]
    pub dispatch_dedup_window_secs: u64,
    #[serde(default = "default_dispatch_manual_cooldown_secs")]
    pub dispatch_manual_cooldown_secs: u64,
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            rotation_threshold: default_rotation_threshold(),
            auto_dispatch: default_board_auto_dispatch(),
            dispatch_stabilization_delay_secs: default_dispatch_stabilization_delay_secs(),
            dispatch_dedup_window_secs: default_dispatch_dedup_window_secs(),
            dispatch_manual_cooldown_secs: default_dispatch_manual_cooldown_secs(),
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
    #[serde(default = "default_utilization_recovery_interval_secs")]
    pub utilization_recovery_interval_secs: u64,
    #[serde(default = "default_enabled")]
    pub commit_before_reset: bool,
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
            utilization_recovery_interval_secs: default_utilization_recovery_interval_secs(),
            commit_before_reset: default_enabled(),
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

fn default_dispatch_dedup_window_secs() -> u64 {
    60
}

fn default_dispatch_manual_cooldown_secs() -> u64 {
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

fn default_review_nudge_threshold_secs() -> u64 {
    1800
}

fn default_review_timeout_secs() -> u64 {
    7200
}

fn default_stall_threshold_secs() -> u64 {
    300
}

fn default_max_stall_restarts() -> u32 {
    2
}

fn default_health_check_interval_secs() -> u64 {
    60
}

fn default_uncommitted_warn_threshold() -> usize {
    200
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

fn default_utilization_recovery_interval_secs() -> u64 {
    1200
}

fn default_event_log_max_bytes() -> u64 {
    DEFAULT_EVENT_LOG_MAX_BYTES
}

fn default_retro_min_duration_secs() -> u64 {
    60
}

impl TeamConfig {
    pub fn orchestrator_enabled(&self) -> bool {
        self.workflow_mode.enables_runtime_surface() && self.orchestrator_pane
    }

    /// Resolve the effective agent for a role.
    ///
    /// Resolution order: role-level agent > team-level agent > "claude".
    pub fn resolve_agent(&self, role: &RoleDef) -> Option<String> {
        if role.role_type == RoleType::User {
            return None;
        }
        Some(
            role.agent
                .clone()
                .or_else(|| self.agent.clone())
                .unwrap_or_else(|| "claude".to_string()),
        )
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
        // external senders (e.g. email-router, slack-bridge) can send to anyone
        if self.external_senders.iter().any(|s| s == from_role) {
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

        let valid_agents = agent::KNOWN_AGENT_NAMES.join(", ");

        // Validate team-level agent if specified.
        if let Some(team_agent) = self.agent.as_deref() {
            if agent::adapter_from_name(team_agent).is_none() {
                bail!(
                    "unknown team-level agent '{}'; valid agents: {}",
                    team_agent,
                    valid_agents
                );
            }
        }

        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            if role.name.is_empty() {
                bail!("role has empty name — every role requires a non-empty 'name' field");
            }

            if !role_names.insert(&role.name) {
                bail!("duplicate role name: '{}'", role.name);
            }

            // Non-user roles need an agent — either their own or the team default.
            if role.role_type != RoleType::User && role.agent.is_none() && self.agent.is_none() {
                bail!(
                    "role '{}' has no agent configured — \
                     set a role-level 'agent' field or a team-level 'agent' default; \
                     valid agents: {}",
                    role.name,
                    valid_agents
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

            if let Some(agent_name) = role.agent.as_deref()
                && agent::adapter_from_name(agent_name).is_none()
            {
                bail!(
                    "role '{}' uses unknown agent '{}'; valid agents: {}",
                    role.name,
                    agent_name,
                    valid_agents
                );
            }
        }

        // Validate talks_to references exist
        let all_role_names: Vec<&str> = role_names.iter().copied().collect();
        for role in &self.roles {
            for target in &role.talks_to {
                if !role_names.contains(target.as_str()) {
                    bail!(
                        "role '{}' references unknown role '{}' in talks_to; \
                         defined roles: {}",
                        role.name,
                        target,
                        all_role_names.join(", ")
                    );
                }
            }
        }

        if let Some(sender) = &self.automation_sender
            && !role_names.contains(sender.as_str())
            && sender != "human"
        {
            bail!(
                "automation_sender references unknown role '{}'; \
                 defined roles: {}",
                sender,
                all_role_names.join(", ")
            );
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

    /// Run all validation checks, collecting results for each check.
    /// Returns a list of (check_name, passed, detail) tuples.
    pub fn validate_verbose(&self) -> Vec<ValidationCheck> {
        let mut checks = Vec::new();

        // 1. Team name
        let name_ok = !self.name.is_empty();
        checks.push(ValidationCheck {
            name: "team_name".to_string(),
            passed: name_ok,
            detail: if name_ok {
                format!("team name: '{}'", self.name)
            } else {
                "team name is empty".to_string()
            },
        });

        // 2. Roles present
        let roles_ok = !self.roles.is_empty();
        checks.push(ValidationCheck {
            name: "roles_present".to_string(),
            passed: roles_ok,
            detail: if roles_ok {
                format!("{} role(s) defined", self.roles.len())
            } else {
                "no roles defined".to_string()
            },
        });

        if !roles_ok {
            return checks;
        }

        // 3. Team-level agent
        let team_agent_ok = match self.agent.as_deref() {
            Some(name) => agent::adapter_from_name(name).is_some(),
            None => true,
        };
        checks.push(ValidationCheck {
            name: "team_agent".to_string(),
            passed: team_agent_ok,
            detail: match self.agent.as_deref() {
                Some(name) if team_agent_ok => format!("team agent: '{name}'"),
                Some(name) => format!("unknown team agent: '{name}'"),
                None => "no team-level agent (roles must set their own)".to_string(),
            },
        });

        // 4. Per-role checks
        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            let unique = role_names.insert(&role.name);
            checks.push(ValidationCheck {
                name: format!("role_unique:{}", role.name),
                passed: unique,
                detail: if unique {
                    format!("role '{}' is unique", role.name)
                } else {
                    format!("duplicate role name: '{}'", role.name)
                },
            });

            let has_agent =
                role.role_type == RoleType::User || role.agent.is_some() || self.agent.is_some();
            checks.push(ValidationCheck {
                name: format!("role_agent:{}", role.name),
                passed: has_agent,
                detail: if has_agent {
                    let effective = role
                        .agent
                        .as_deref()
                        .or(self.agent.as_deref())
                        .unwrap_or("(user)");
                    format!("role '{}' agent: {effective}", role.name)
                } else {
                    format!("role '{}' has no agent", role.name)
                },
            });

            if let Some(agent_name) = role.agent.as_deref() {
                let valid = agent::adapter_from_name(agent_name).is_some();
                checks.push(ValidationCheck {
                    name: format!("role_agent_valid:{}", role.name),
                    passed: valid,
                    detail: if valid {
                        format!("role '{}' agent '{}' is valid", role.name, agent_name)
                    } else {
                        format!("role '{}' uses unknown agent '{}'", role.name, agent_name)
                    },
                });
            }

            let instances_ok = role.instances > 0;
            checks.push(ValidationCheck {
                name: format!("role_instances:{}", role.name),
                passed: instances_ok,
                detail: format!("role '{}' instances: {}", role.name, role.instances),
            });
        }

        // 5. talks_to references
        for role in &self.roles {
            for target in &role.talks_to {
                let valid = role_names.contains(target.as_str());
                checks.push(ValidationCheck {
                    name: format!("talks_to:{}→{}", role.name, target),
                    passed: valid,
                    detail: if valid {
                        format!("role '{}' → '{}' is valid", role.name, target)
                    } else {
                        format!(
                            "role '{}' references unknown role '{}' in talks_to",
                            role.name, target
                        )
                    },
                });
            }
        }

        // 6. automation_sender
        if let Some(sender) = &self.automation_sender {
            let valid = role_names.contains(sender.as_str()) || sender == "human";
            checks.push(ValidationCheck {
                name: "automation_sender".to_string(),
                passed: valid,
                detail: if valid {
                    format!("automation_sender '{sender}' is valid")
                } else {
                    format!("automation_sender references unknown role '{sender}'")
                },
            });
        }

        // 7. Layout zones
        if let Some(layout) = &self.layout {
            let total_pct: u32 = layout.zones.iter().map(|z| z.width_pct).sum();
            let valid = total_pct <= 100;
            checks.push(ValidationCheck {
                name: "layout_zones".to_string(),
                passed: valid,
                detail: if valid {
                    format!("layout zones sum to {total_pct}%")
                } else {
                    format!("layout zones sum to {total_pct}%, exceeds 100%")
                },
            });
        }

        checks
    }
}

/// A single validation check result.
#[derive(Debug, Clone)]
pub struct ValidationCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
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
    fn validate_accepts_kiro_agent() {
        let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: kiro
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unknown_agent() {
        let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: mystery
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("unknown agent 'mystery'"));
        assert!(error.contains("valid agents:"));
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
        assert!(err.contains("unknown role 'manager'"));
        assert!(err.contains("talks_to"));
    }

    #[test]
    fn external_sender_can_talk_to_any_role() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
external_senders:
  - email-router
  - slack-bridge
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

        assert!(config.can_talk("email-router", "manager"));
        assert!(config.can_talk("email-router", "architect"));
        assert!(config.can_talk("email-router", "engineer"));
        assert!(config.can_talk("slack-bridge", "manager"));
        assert!(config.can_talk("slack-bridge", "engineer"));
    }

    #[test]
    fn unknown_sender_blocked() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
external_senders:
  - email-router
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();

        // "random-sender" is not in external_senders and not a known role
        assert!(!config.can_talk("random-sender", "manager"));
        assert!(!config.can_talk("random-sender", "engineer"));
    }

    #[test]
    fn parse_review_timeout_overrides_from_yaml() {
        let yaml = r#"
name: test-team
workflow_policy:
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  review_timeout_overrides:
    critical:
      review_nudge_threshold_secs: 300
      review_timeout_secs: 600
    high:
      review_timeout_secs: 3600
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let policy = &config.workflow_policy;

        // Global defaults
        assert_eq!(policy.review_nudge_threshold_secs, 1800);
        assert_eq!(policy.review_timeout_secs, 7200);

        // Critical override — both fields set
        let critical = policy.review_timeout_overrides.get("critical").unwrap();
        assert_eq!(critical.review_nudge_threshold_secs, Some(300));
        assert_eq!(critical.review_timeout_secs, Some(600));

        // High override — only escalation set, nudge absent
        let high = policy.review_timeout_overrides.get("high").unwrap();
        assert_eq!(high.review_nudge_threshold_secs, None);
        assert_eq!(high.review_timeout_secs, Some(3600));

        // No override for medium
        assert!(!policy.review_timeout_overrides.contains_key("medium"));
    }

    #[test]
    fn empty_overrides_when_absent_in_yaml() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(config.workflow_policy.review_timeout_overrides.is_empty());
    }

    // --- Mixed-backend / team-level agent tests ---

    #[test]
    fn team_level_agent_parsed() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: worker
    role_type: engineer
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.as_deref(), Some("codex"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn team_level_agent_absent_defaults_to_none() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(config.agent.is_none());
    }

    #[test]
    fn resolve_agent_role_overrides_team() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: worker
    role_type: engineer
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let architect = &config.roles[0];
        let worker = &config.roles[1];
        // Role-level agent overrides team-level
        assert_eq!(config.resolve_agent(architect).as_deref(), Some("claude"));
        // No role-level agent, falls back to team-level
        assert_eq!(config.resolve_agent(worker).as_deref(), Some("codex"));
    }

    #[test]
    fn resolve_agent_defaults_to_claude_when_nothing_set() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        // Override the role to have no agent for testing
        let mut role = config.roles[0].clone();
        role.agent = None;
        let mut config_no_team = config.clone();
        config_no_team.agent = None;
        assert_eq!(
            config_no_team.resolve_agent(&role).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn resolve_agent_returns_none_for_user() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: human
    role_type: user
  - name: worker
    role_type: engineer
    instances: 1
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let user = &config.roles[0];
        assert!(config.resolve_agent(user).is_none());
    }

    #[test]
    fn validate_team_level_agent_rejects_unknown() {
        let yaml = r#"
name: test
agent: mystery
roles:
  - name: worker
    role_type: engineer
    instances: 1
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("team-level agent"));
        assert!(err.contains("mystery"));
    }

    #[test]
    fn validate_accepts_team_level_agent_without_role_agent() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
  - name: manager
    role_type: manager
  - name: engineer
    role_type: engineer
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_no_agent_at_any_level() {
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
    fn validate_mixed_backend_team() {
        let yaml = r#"
name: mixed
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: eng-claude
    role_type: engineer
    agent: claude
    instances: 2
    talks_to: [manager]
  - name: eng-codex
    role_type: engineer
    instances: 2
    talks_to: [manager]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_ok());
        // eng-claude has explicit agent
        assert_eq!(config.roles[2].agent.as_deref(), Some("claude"));
        // eng-codex inherits team default
        assert!(config.roles[3].agent.is_none());
        assert_eq!(
            config.resolve_agent(&config.roles[3]).as_deref(),
            Some("codex")
        );
    }

    // --- Edge case tests: missing fields ---

    #[test]
    fn validate_rejects_empty_roles_list() {
        let yaml = r#"
name: test
roles: []
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("at least one role"));
    }

    #[test]
    fn validate_rejects_role_with_empty_name() {
        let yaml = r#"
name: test
roles:
  - name: ""
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("empty name"));
    }

    #[test]
    fn validate_rejects_two_roles_with_empty_names() {
        let yaml = r#"
name: test
roles:
  - name: ""
    role_type: engineer
    agent: codex
  - name: ""
    role_type: manager
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        // Now fails at the first empty name before reaching duplicate check
        assert!(err.contains("empty name"));
    }

    // --- Edge case tests: wrong types in YAML ---

    #[test]
    fn parse_rejects_string_for_instances() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: many
"#;
        assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
    }

    #[test]
    fn parse_rejects_boolean_for_name() {
        let yaml = r#"
name: true
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        // YAML coerces true to "true" string — should parse but name is "true"
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "true");
    }

    #[test]
    fn parse_null_name_deserializes_as_literal_string() {
        let yaml = r#"
name: null
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        // serde_yaml 0.9 deserializes YAML null as literal "null" for String fields
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "null");
    }

    #[test]
    fn parse_tilde_name_deserializes_as_tilde_string() {
        let yaml = r#"
name: ~
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        // serde_yaml 0.9 coerces ~ (YAML null) to "~" for non-Option String fields
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "~");
    }

    #[test]
    fn parse_rejects_invalid_role_type() {
        let yaml = r#"
name: test
roles:
  - name: wizard
    role_type: wizard
    agent: claude
"#;
        let err = serde_yaml::from_str::<TeamConfig>(yaml)
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }

    #[test]
    fn parse_rejects_invalid_workflow_mode() {
        let yaml = r#"
name: test
workflow_mode: turbo
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
    }

    #[test]
    fn parse_rejects_invalid_orchestrator_position() {
        let yaml = r#"
name: test
orchestrator_position: top
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
    }

    #[test]
    fn parse_rejects_negative_instances() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: -1
"#;
        assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
    }

    #[test]
    fn parse_rejects_string_for_interval_secs() {
        let yaml = r#"
name: test
standup:
  interval_secs: forever
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
    }

    // --- Edge case tests: hierarchy and talks_to ---

    #[test]
    fn can_talk_role_to_self_via_talks_to() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
roles:
  - name: solo
    role_type: architect
    agent: claude
    talks_to: [solo]
"#,
        )
        .unwrap();
        // Self-referencing talks_to — allowed by current rules, role can talk to itself
        assert!(config.can_talk("solo", "solo"));
        config.validate().unwrap(); // Should not crash
    }

    #[test]
    fn can_talk_nonexistent_sender_returns_false() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(!config.can_talk("ghost", "architect"));
    }

    #[test]
    fn can_talk_nonexistent_target_returns_false() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(!config.can_talk("architect", "ghost"));
    }

    #[test]
    fn validate_accepts_single_user_only_team() {
        let yaml = r#"
name: test
roles:
  - name: human
    role_type: user
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        // User roles don't need agents — should be valid
        config.validate().unwrap();
    }

    #[test]
    fn validate_accepts_multiple_user_roles() {
        let yaml = r#"
name: test
roles:
  - name: alice
    role_type: user
  - name: bob
    role_type: user
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    // --- Edge case tests: boundary values ---

    #[test]
    fn validate_accepts_large_instance_count() {
        let yaml = r#"
name: test
roles:
  - name: army
    role_type: engineer
    agent: codex
    instances: 100
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.roles[0].instances, 100);
        config.validate().unwrap();
    }

    #[test]
    fn parse_workflow_policy_zero_wip_limits() {
        let yaml = r#"
name: test
workflow_policy:
  wip_limit_per_engineer: 0
  wip_limit_per_reviewer: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.workflow_policy.wip_limit_per_engineer, Some(0));
        assert_eq!(config.workflow_policy.wip_limit_per_reviewer, Some(0));
    }

    #[test]
    fn parse_workflow_policy_defaults_all_applied() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let p = &config.workflow_policy;
        assert!(p.wip_limit_per_engineer.is_none());
        assert!(p.wip_limit_per_reviewer.is_none());
        assert_eq!(p.pipeline_starvation_threshold, Some(1));
        assert_eq!(p.escalation_threshold_secs, 3600);
        assert_eq!(p.review_nudge_threshold_secs, 1800);
        assert_eq!(p.review_timeout_secs, 7200);
        assert!(p.review_timeout_overrides.is_empty());
        assert!(p.auto_archive_done_after_secs.is_none());
        assert!(p.capability_overrides.is_empty());
        assert_eq!(p.stall_threshold_secs, 300);
        assert_eq!(p.max_stall_restarts, 2);
        assert_eq!(p.health_check_interval_secs, 60);
        assert_eq!(p.uncommitted_warn_threshold, 200);
    }

    #[test]
    fn parse_workflow_policy_zero_escalation_threshold() {
        let yaml = r#"
name: test
workflow_policy:
  escalation_threshold_secs: 0
  stall_threshold_secs: 0
  max_stall_restarts: 0
  health_check_interval_secs: 0
  uncommitted_warn_threshold: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.workflow_policy.escalation_threshold_secs, 0);
        assert_eq!(config.workflow_policy.stall_threshold_secs, 0);
        assert_eq!(config.workflow_policy.max_stall_restarts, 0);
        assert_eq!(config.workflow_policy.health_check_interval_secs, 0);
        assert_eq!(config.workflow_policy.uncommitted_warn_threshold, 0);
    }

    #[test]
    fn validate_layout_zones_exactly_100_pct() {
        let yaml = r#"
name: test
layout:
  zones:
    - name: left
      width_pct: 50
    - name: right
      width_pct: 50
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_layout_empty_zones_accepted() {
        let yaml = r#"
name: test
layout:
  zones: []
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_layout_zone_width_zero() {
        let yaml = r#"
name: test
layout:
  zones:
    - name: invisible
      width_pct: 0
    - name: full
      width_pct: 100
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
    }

    // --- Edge case tests: auto-merge policy ---

    #[test]
    fn parse_auto_merge_policy_defaults() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let am = &config.workflow_policy.auto_merge;
        assert!(!am.enabled);
        assert_eq!(am.max_diff_lines, 200);
        assert_eq!(am.max_files_changed, 5);
        assert_eq!(am.max_modules_touched, 2);
        assert_eq!(am.confidence_threshold, 0.8);
        assert!(am.require_tests_pass);
        assert!(!am.sensitive_paths.is_empty());
    }

    #[test]
    fn parse_auto_merge_policy_custom() {
        let yaml = r#"
name: test
workflow_policy:
  auto_merge:
    enabled: true
    max_diff_lines: 50
    max_files_changed: 2
    max_modules_touched: 1
    confidence_threshold: 0.95
    require_tests_pass: false
    sensitive_paths: ["secrets.yaml"]
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let am = &config.workflow_policy.auto_merge;
        assert!(am.enabled);
        assert_eq!(am.max_diff_lines, 50);
        assert_eq!(am.max_files_changed, 2);
        assert_eq!(am.max_modules_touched, 1);
        assert_eq!(am.confidence_threshold, 0.95);
        assert!(!am.require_tests_pass);
        assert_eq!(am.sensitive_paths, vec!["secrets.yaml"]);
    }

    #[test]
    fn parse_auto_merge_zero_thresholds() {
        let yaml = r#"
name: test
workflow_policy:
  auto_merge:
    enabled: true
    max_diff_lines: 0
    max_files_changed: 0
    max_modules_touched: 0
    confidence_threshold: 0.0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let am = &config.workflow_policy.auto_merge;
        assert_eq!(am.max_diff_lines, 0);
        assert_eq!(am.max_files_changed, 0);
        assert_eq!(am.max_modules_touched, 0);
        assert_eq!(am.confidence_threshold, 0.0);
    }

    // --- Edge case tests: cost config ---

    #[test]
    fn parse_cost_config_empty_models_map() {
        let yaml = r#"
name: test
cost:
  models: {}
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.cost.models.is_empty());
    }

    #[test]
    fn parse_cost_config_zero_pricing() {
        let yaml = r#"
name: test
cost:
  models:
    free-model:
      input_usd_per_mtok: 0.0
      output_usd_per_mtok: 0.0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let model = config.cost.models.get("free-model").unwrap();
        assert_eq!(model.input_usd_per_mtok, 0.0);
        assert_eq!(model.output_usd_per_mtok, 0.0);
    }

    // --- Edge case tests: orchestrator position ---

    #[test]
    fn parse_orchestrator_position_left() {
        let yaml = r#"
name: test
orchestrator_position: left
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.orchestrator_position, OrchestratorPosition::Left);
    }

    #[test]
    fn parse_orchestrator_position_defaults_to_bottom() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom);
    }

    // --- Edge case tests: event log and retro ---

    #[test]
    fn parse_event_log_max_bytes_zero() {
        let yaml = r#"
name: test
event_log_max_bytes: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.event_log_max_bytes, 0);
    }

    #[test]
    fn parse_retro_min_duration_zero() {
        let yaml = r#"
name: test
retro_min_duration_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.retro_min_duration_secs, 0);
    }

    // --- Edge case tests: planning directives ---

    #[test]
    fn load_planning_directive_returns_none_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("review_policy.md"), "").unwrap();

        let loaded =
            load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();
        assert_eq!(loaded, None);
    }

    #[test]
    fn load_planning_directive_returns_none_for_whitespace_only() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("review_policy.md"), "   \n  \n  ").unwrap();

        let loaded =
            load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();
        assert_eq!(loaded, None);
    }

    #[test]
    fn load_planning_directive_truncation_boundary_exact_length() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("review_policy.md"), "abcde").unwrap();

        // Exact length — no truncation
        let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 5)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, "abcde");
        assert!(!loaded.contains("truncated"));
    }

    #[test]
    fn load_planning_directive_truncation_boundary_one_over() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("review_policy.md"), "abcdef").unwrap();

        // One char over — truncated
        let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 5)
            .unwrap()
            .unwrap();
        assert!(loaded.starts_with("abcde"));
        assert!(loaded.contains("truncated"));
    }

    // --- Edge case tests: capability overrides ---

    #[test]
    fn parse_capability_overrides() {
        let yaml = r#"
name: test
workflow_policy:
  capability_overrides:
    engineer:
      - review
      - merge
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let overrides = &config.workflow_policy.capability_overrides;
        assert_eq!(
            overrides.get("engineer").unwrap(),
            &vec!["review".to_string(), "merge".to_string()]
        );
    }

    #[test]
    fn parse_capability_overrides_empty() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert!(config.workflow_policy.capability_overrides.is_empty());
    }

    // --- Edge case tests: board config boundaries ---

    #[test]
    fn parse_board_config_zero_thresholds() {
        let yaml = r#"
name: test
board:
  rotation_threshold: 0
  dispatch_stabilization_delay_secs: 0
  dispatch_dedup_window_secs: 0
  dispatch_manual_cooldown_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.board.rotation_threshold, 0);
        assert_eq!(config.board.dispatch_stabilization_delay_secs, 0);
        assert_eq!(config.board.dispatch_dedup_window_secs, 0);
        assert_eq!(config.board.dispatch_manual_cooldown_secs, 0);
    }

    // --- Edge case tests: load from file errors ---

    #[test]
    fn load_from_nonexistent_file_returns_error() {
        let result = TeamConfig::load(Path::new("/nonexistent/path/team.yaml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to read"));
    }

    #[test]
    fn load_from_invalid_yaml_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(&path, "{{{{not valid yaml}}}}").unwrap();
        let result = TeamConfig::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to parse"));
    }

    #[test]
    fn load_from_empty_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(&path, "").unwrap();
        let result = TeamConfig::load(&path);
        assert!(result.is_err());
    }

    // --- Task #291: Config validation improvements ---

    #[test]
    fn invalid_talks_to_error_shows_defined_roles() {
        let yaml = r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [nonexistent]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("references unknown role 'nonexistent'"),
            "expected unknown role message, got: {err}"
        );
        assert!(
            err.contains("defined roles:"),
            "expected defined roles list, got: {err}"
        );
        assert!(
            err.contains("architect"),
            "expected architect in defined roles, got: {err}"
        );
        assert!(
            err.contains("engineer"),
            "expected engineer in defined roles, got: {err}"
        );
    }

    #[test]
    fn missing_field_error_lists_valid_agents() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("no agent configured"),
            "expected missing agent message, got: {err}"
        );
        assert!(
            err.contains("valid agents:"),
            "expected valid agents list, got: {err}"
        );
        assert!(
            err.contains("claude"),
            "expected claude in valid agents, got: {err}"
        );
    }

    #[test]
    fn unknown_backend_error_lists_valid_agents() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: gpt4
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("unknown agent 'gpt4'"),
            "expected unknown agent message, got: {err}"
        );
        assert!(
            err.contains("valid agents:"),
            "expected valid agents list, got: {err}"
        );
        assert!(err.contains("claude"), "expected claude listed, got: {err}");
        assert!(err.contains("codex"), "expected codex listed, got: {err}");
        assert!(err.contains("kiro"), "expected kiro listed, got: {err}");
    }

    #[test]
    fn verbose_shows_checks_all_pass() {
        let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [architect]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let checks = config.validate_verbose();

        assert!(!checks.is_empty(), "expected at least one check");
        assert!(
            checks.iter().all(|c| c.passed),
            "expected all checks to pass, failures: {:?}",
            checks.iter().filter(|c| !c.passed).collect::<Vec<_>>()
        );

        // Verify specific checks are present
        assert!(
            checks.iter().any(|c| c.name == "team_name"),
            "expected team_name check"
        );
        assert!(
            checks.iter().any(|c| c.name == "roles_present"),
            "expected roles_present check"
        );
        assert!(
            checks.iter().any(|c| c.name == "team_agent"),
            "expected team_agent check"
        );
        assert!(
            checks.iter().any(|c| c.name.starts_with("role_unique:")),
            "expected role_unique check"
        );
        assert!(
            checks.iter().any(|c| c.name.starts_with("role_agent:")),
            "expected role_agent check"
        );
        assert!(
            checks.iter().any(|c| c.name.starts_with("talks_to:")),
            "expected talks_to check"
        );
    }

    #[test]
    fn verbose_shows_checks_with_failures() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: mystery
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let checks = config.validate_verbose();

        let failed: Vec<_> = checks.iter().filter(|c| !c.passed).collect();
        assert!(!failed.is_empty(), "expected at least one failing check");
        assert!(
            failed.iter().any(|c| c.name.contains("role_agent_valid")),
            "expected role_agent_valid failure, failures: {:?}",
            failed
        );
        assert!(
            failed.iter().any(|c| c.detail.contains("unknown agent")),
            "expected unknown agent detail in failure"
        );
    }

    // --- Property-based tests (proptest) ---

    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        /// Valid agent backend names recognized by adapter_from_name.
        const VALID_AGENTS: &[&str] = &[
            "claude",
            "claude-code",
            "codex",
            "codex-cli",
            "kiro",
            "kiro-cli",
        ];

        /// Valid role type strings for YAML.
        const VALID_ROLE_TYPES: &[&str] = &["user", "architect", "manager", "engineer"];

        /// Valid workflow mode strings for YAML.
        const VALID_WORKFLOW_MODES: &[&str] = &["legacy", "hybrid", "workflow_first"];

        /// Valid orchestrator position strings for YAML.
        const VALID_ORCH_POSITIONS: &[&str] = &["bottom", "left"];

        /// Strategy for a valid agent name.
        fn valid_agent() -> impl Strategy<Value = String> {
            proptest::sample::select(VALID_AGENTS).prop_map(|s| s.to_string())
        }

        /// Strategy for a valid role type.
        fn valid_role_type() -> impl Strategy<Value = String> {
            proptest::sample::select(VALID_ROLE_TYPES).prop_map(|s| s.to_string())
        }

        /// Strategy for a safe YAML name (alphanumeric + hyphens, non-empty).
        fn safe_name() -> impl Strategy<Value = String> {
            "[a-z][a-z0-9\\-]{0,15}".prop_map(|s| s.to_string())
        }

        // 1. Random role count: valid configs with 1-10 roles never panic on parse
        proptest! {
            #[test]
            fn valid_random_role_count_parses_without_panic(
                team_name in safe_name(),
                role_count in 1usize..=10,
            ) {
                let mut roles_yaml = String::new();
                for i in 0..role_count {
                    let role_type = if i == 0 { "architect" } else { "engineer" };
                    roles_yaml.push_str(&format!(
                        "  - name: role-{i}\n    role_type: {role_type}\n    agent: claude\n"
                    ));
                }
                let yaml = format!("name: {team_name}\nroles:\n{roles_yaml}");
                let result = serde_yaml::from_str::<TeamConfig>(&yaml);
                prop_assert!(result.is_ok(), "Failed to parse: {:?}", result.err());
                let config = result.unwrap();
                prop_assert_eq!(config.roles.len(), role_count);
            }
        }

        // 2. Random agent backends: all valid agent names parse successfully
        proptest! {
            #[test]
            fn valid_agent_backend_parses(agent in valid_agent()) {
                let yaml = format!(
                    "name: test\nroles:\n  - name: worker\n    role_type: engineer\n    agent: {agent}\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(config.roles[0].agent.as_deref(), Some(agent.as_str()));
            }
        }

        // 3. Random invalid agent names: parse succeeds but validate rejects
        proptest! {
            #[test]
            fn invalid_agent_backend_rejected_by_validate(
                agent in "[a-z]{3,10}".prop_filter(
                    "must not be a valid agent",
                    |s| !VALID_AGENTS.contains(&s.as_str()),
                ),
            ) {
                let yaml = format!(
                    "name: test\nroles:\n  - name: worker\n    role_type: engineer\n    agent: {agent}\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                let err = config.validate().unwrap_err().to_string();
                prop_assert!(err.contains("unknown agent"), "Error was: {err}");
            }
        }

        // 4. Team-level agent applied to roles without explicit agent
        proptest! {
            #[test]
            fn team_level_agent_applied_to_agentless_roles(
                team_agent in valid_agent(),
                role_count in 1usize..=5,
            ) {
                let mut roles_yaml = String::new();
                for i in 0..role_count {
                    let role_type = if i == 0 { "architect" } else { "engineer" };
                    roles_yaml.push_str(&format!(
                        "  - name: role-{i}\n    role_type: {role_type}\n"
                    ));
                }
                let yaml = format!("name: test\nagent: {team_agent}\nroles:\n{roles_yaml}");
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                prop_assert!(config.validate().is_ok());
                for role in &config.roles {
                    let resolved = config.resolve_agent(role);
                    prop_assert_eq!(resolved.as_deref(), Some(team_agent.as_str()));
                }
            }
        }

        // 5. Random workflow policy values: never panic on parse
        proptest! {
            #[test]
            fn random_workflow_policy_values_parse(
                wip_eng in proptest::option::of(0u32..100),
                wip_rev in proptest::option::of(0u32..100),
                escalation in 0u64..100_000,
                stall in 0u64..100_000,
                max_restarts in 0u32..20,
                health_interval in 0u64..10_000,
                uncommitted_warn in 0usize..1000,
            ) {
                let mut policy_yaml = String::from("workflow_policy:\n");
                if let Some(v) = wip_eng {
                    policy_yaml.push_str(&format!("  wip_limit_per_engineer: {v}\n"));
                }
                if let Some(v) = wip_rev {
                    policy_yaml.push_str(&format!("  wip_limit_per_reviewer: {v}\n"));
                }
                policy_yaml.push_str(&format!("  escalation_threshold_secs: {escalation}\n"));
                policy_yaml.push_str(&format!("  stall_threshold_secs: {stall}\n"));
                policy_yaml.push_str(&format!("  max_stall_restarts: {max_restarts}\n"));
                policy_yaml.push_str(&format!("  health_check_interval_secs: {health_interval}\n"));
                policy_yaml.push_str(&format!("  uncommitted_warn_threshold: {uncommitted_warn}\n"));

                let yaml = format!(
                    "name: test\n{policy_yaml}roles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let result = serde_yaml::from_str::<TeamConfig>(&yaml);
                prop_assert!(result.is_ok(), "Parse failed: {:?}", result.err());
                let config = result.unwrap();
                prop_assert_eq!(config.workflow_policy.escalation_threshold_secs, escalation);
                prop_assert_eq!(config.workflow_policy.stall_threshold_secs, stall);
                prop_assert_eq!(config.workflow_policy.max_stall_restarts, max_restarts);
            }
        }

        // 6. Missing optional fields: config parses with defaults
        proptest! {
            #[test]
            fn missing_optional_fields_use_defaults(
                team_name in safe_name(),
                instances in 1u32..=20,
            ) {
                // Minimal config: only required fields (name, roles with name+role_type+agent)
                let yaml = format!(
                    "name: {team_name}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n    instances: {instances}\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                // All optional fields should have defaults
                prop_assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
                prop_assert!(config.orchestrator_pane);
                prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom);
                prop_assert!(config.layout.is_none());
                prop_assert!(config.agent.is_none());
                prop_assert!(config.automation_sender.is_none());
                prop_assert!(config.external_senders.is_empty());
                prop_assert_eq!(config.board.rotation_threshold, 20);
                prop_assert!(config.board.auto_dispatch);
                prop_assert_eq!(config.roles[0].instances, instances);
            }
        }

        // 7. Extra unknown fields: serde ignores them (forward compatibility)
        proptest! {
            #[test]
            fn extra_unknown_fields_ignored(
                extra_key in "[a-z_]{3,12}",
                extra_val in "[a-z0-9]{1,10}",
            ) {
                let yaml = format!(
                    "name: test\n{extra_key}: {extra_val}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n    {extra_key}: {extra_val}\n"
                );
                let result = serde_yaml::from_str::<TeamConfig>(&yaml);
                prop_assert!(result.is_ok(), "Unknown fields should be ignored: {:?}", result.err());
            }
        }

        // 8. Random workflow mode: all valid modes parse correctly
        proptest! {
            #[test]
            fn valid_workflow_modes_parse(
                mode_idx in 0usize..VALID_WORKFLOW_MODES.len(),
            ) {
                let mode = VALID_WORKFLOW_MODES[mode_idx];
                let yaml = format!(
                    "name: test\nworkflow_mode: {mode}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                match mode {
                    "legacy" => prop_assert_eq!(config.workflow_mode, WorkflowMode::Legacy),
                    "hybrid" => prop_assert_eq!(config.workflow_mode, WorkflowMode::Hybrid),
                    "workflow_first" => prop_assert_eq!(config.workflow_mode, WorkflowMode::WorkflowFirst),
                    _ => unreachable!(),
                }
            }
        }

        // 9. Invalid workflow modes: produce parse errors, not panics
        proptest! {
            #[test]
            fn invalid_workflow_mode_produces_error(
                mode in "[a-z]{3,10}".prop_filter(
                    "must not be valid",
                    |s| !VALID_WORKFLOW_MODES.contains(&s.as_str()),
                ),
            ) {
                let yaml = format!(
                    "name: test\nworkflow_mode: {mode}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let result = serde_yaml::from_str::<TeamConfig>(&yaml);
                prop_assert!(result.is_err(), "Should reject invalid workflow_mode '{mode}'");
            }
        }

        // 10. Random orchestrator position: valid ones parse, invalid error
        proptest! {
            #[test]
            fn valid_orchestrator_positions_parse(
                pos_idx in 0usize..VALID_ORCH_POSITIONS.len(),
            ) {
                let pos = VALID_ORCH_POSITIONS[pos_idx];
                let yaml = format!(
                    "name: test\norchestrator_position: {pos}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                match pos {
                    "bottom" => prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom),
                    "left" => prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Left),
                    _ => unreachable!(),
                }
            }
        }

        // 11. Layout zone widths: parsing never panics, validation catches >100%
        proptest! {
            #[test]
            fn layout_zone_widths_parse_and_validate(
                zone_count in 1usize..=6,
                width in 0u32..=100,
            ) {
                let mut zones_yaml = String::new();
                for i in 0..zone_count {
                    zones_yaml.push_str(&format!(
                        "    - name: zone-{i}\n      width_pct: {width}\n"
                    ));
                }
                let yaml = format!(
                    "name: test\nlayout:\n  zones:\n{zones_yaml}roles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                let total: u32 = config.layout.as_ref().unwrap().zones.iter().map(|z| z.width_pct).sum();
                if total > 100 {
                    prop_assert!(config.validate().is_err());
                } else {
                    prop_assert!(config.validate().is_ok());
                }
            }
        }

        // 12. Random automation config booleans: never panic
        proptest! {
            #[test]
            fn random_automation_booleans_parse(
                nudges in proptest::bool::ANY,
                standups in proptest::bool::ANY,
                failure_det in proptest::bool::ANY,
                triage in proptest::bool::ANY,
                review in proptest::bool::ANY,
                owned in proptest::bool::ANY,
                dispatch in proptest::bool::ANY,
                arch_util in proptest::bool::ANY,
            ) {
                let yaml = format!(
                    "name: test\nautomation:\n  timeout_nudges: {nudges}\n  standups: {standups}\n  failure_pattern_detection: {failure_det}\n  triage_interventions: {triage}\n  review_interventions: {review}\n  owned_task_interventions: {owned}\n  manager_dispatch_interventions: {dispatch}\n  architect_utilization_interventions: {arch_util}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(config.automation.timeout_nudges, nudges);
                prop_assert_eq!(config.automation.standups, standups);
                prop_assert_eq!(config.automation.failure_pattern_detection, failure_det);
                prop_assert_eq!(config.automation.triage_interventions, triage);
                prop_assert_eq!(config.automation.review_interventions, review);
                prop_assert_eq!(config.automation.owned_task_interventions, owned);
                prop_assert_eq!(config.automation.manager_dispatch_interventions, dispatch);
                prop_assert_eq!(config.automation.architect_utilization_interventions, arch_util);
            }
        }

        // 13. Random standup/board config values: parse without panic
        proptest! {
            #[test]
            fn random_standup_and_board_values_parse(
                interval in 0u64..=1_000_000,
                output_lines in 0u32..=500,
                rotation in 0u32..=1000,
                auto_dispatch in proptest::bool::ANY,
            ) {
                let yaml = format!(
                    "name: test\nstandup:\n  interval_secs: {interval}\n  output_lines: {output_lines}\nboard:\n  rotation_threshold: {rotation}\n  auto_dispatch: {auto_dispatch}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(config.standup.interval_secs, interval);
                prop_assert_eq!(config.standup.output_lines, output_lines);
                prop_assert_eq!(config.board.rotation_threshold, rotation);
                prop_assert_eq!(config.board.auto_dispatch, auto_dispatch);
            }
        }

        // 14. Random role type: all valid types parse
        proptest! {
            #[test]
            fn all_role_types_parse(role_type in valid_role_type()) {
                let agent_line = if role_type == "user" { "" } else { "    agent: claude\n" };
                let yaml = format!(
                    "name: test\nroles:\n  - name: r\n    role_type: {role_type}\n{agent_line}"
                );
                let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
                prop_assert_eq!(config.roles.len(), 1);
            }
        }

        // 15. Auto-merge policy random values: never panic
        proptest! {
            #[test]
            fn random_auto_merge_policy_parses(
                enabled in proptest::bool::ANY,
                max_diff in 0usize..10_000,
                max_files in 0usize..100,
                max_modules in 0usize..50,
                confidence in 0.0f64..=1.0,
                require_tests in proptest::bool::ANY,
            ) {
                let yaml = format!(
                    "name: test\nworkflow_policy:\n  auto_merge:\n    enabled: {enabled}\n    max_diff_lines: {max_diff}\n    max_files_changed: {max_files}\n    max_modules_touched: {max_modules}\n    confidence_threshold: {confidence}\n    require_tests_pass: {require_tests}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
                );
                let result = serde_yaml::from_str::<TeamConfig>(&yaml);
                prop_assert!(result.is_ok(), "Parse failed: {:?}", result.err());
                let config = result.unwrap();
                prop_assert_eq!(config.workflow_policy.auto_merge.enabled, enabled);
                prop_assert_eq!(config.workflow_policy.auto_merge.max_diff_lines, max_diff);
                prop_assert_eq!(config.workflow_policy.auto_merge.max_files_changed, max_files);
                prop_assert_eq!(config.workflow_policy.auto_merge.require_tests_pass, require_tests);
            }
        }

        // 16. Completely random YAML strings: never panic (errors OK)
        proptest! {
            #[test]
            fn arbitrary_yaml_never_panics(yaml in "\\PC{0,200}") {
                // Parsing arbitrary bytes should either succeed or return Err, never panic
                let _ = serde_yaml::from_str::<TeamConfig>(&yaml);
            }
        }
    }
}
