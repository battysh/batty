//! Type definitions for team configuration.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

use super::super::DEFAULT_EVENT_LOG_MAX_BYTES;

#[derive(Debug, Clone)]
pub struct TeamConfig {
    pub name: String,
    /// Team-level default agent backend. Individual roles can override this
    /// with their own `agent` field. Resolution order:
    /// role-level agent > team-level agent > "claude" (hardcoded default).
    pub agent: Option<String>,
    pub workflow_mode: WorkflowMode,
    pub board: BoardConfig,
    pub standup: StandupConfig,
    pub automation: AutomationConfig,
    pub automation_sender: Option<String>,
    /// External senders (e.g. email-router, slack-bridge) that are allowed to
    /// message any role even though they are not team members.
    pub external_senders: Vec<String>,
    pub orchestrator_pane: bool,
    pub orchestrator_position: OrchestratorPosition,
    pub layout: Option<LayoutConfig>,
    pub workflow_policy: WorkflowPolicy,
    pub cost: CostConfig,
    pub grafana: GrafanaConfig,
    /// When true, agents are spawned as shim subprocesses instead of
    /// directly in tmux panes. The shim manages PTY, state classification,
    /// and message delivery over a structured channel.
    pub use_shim: bool,
    /// When true and `use_shim` is enabled, agents that support structured I/O
    /// (Claude Code, Codex) communicate via JSON protocols instead of PTY
    /// screen-scraping. Requires `use_shim: true`. Defaults to true.
    pub use_sdk_mode: bool,
    /// When true and `use_shim` is enabled, crashed agents are automatically
    /// respawned instead of escalating to the manager. This is the default
    /// posture for unattended teams; disable it only for debugging or
    /// deliberate manual supervision.
    pub auto_respawn_on_crash: bool,
    /// Interval in seconds between Ping health checks sent to shim handles.
    pub shim_health_check_interval_secs: u64,
    /// Seconds without a Pong response before a shim handle is considered stale.
    pub shim_health_timeout_secs: u64,
    /// Seconds to wait for graceful shutdown before sending Kill.
    pub shim_shutdown_timeout_secs: u32,
    /// Maximum seconds an agent can remain in "Working" state before being
    /// force-transitioned to Idle. Prevents permanent stalls where the shim
    /// state classifier gets stuck on "working" while the agent is actually
    /// idle. 0 or None disables the check. Default: 1800 (30 minutes).
    pub shim_working_state_timeout_secs: u64,
    /// Maximum seconds a message can sit in the pending delivery queue before
    /// being force-delivered via inbox fallback. Prevents message loss when
    /// the target agent appears permanently busy. Default: 600 (10 minutes).
    pub pending_queue_max_age_secs: u64,
    pub event_log_max_bytes: u64,
    pub retro_min_duration_secs: u64,
    pub roles: Vec<RoleDef>,
}

#[derive(Debug, Deserialize)]
struct TeamConfigWire {
    pub name: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workflow_mode: Option<WorkflowMode>,
    #[serde(default)]
    pub board: BoardConfig,
    #[serde(default)]
    pub standup: StandupConfig,
    #[serde(default)]
    pub automation: AutomationConfig,
    #[serde(default)]
    pub automation_sender: Option<String>,
    #[serde(default)]
    pub external_senders: Vec<String>,
    #[serde(default)]
    pub orchestrator_pane: Option<bool>,
    #[serde(default)]
    pub orchestrator_position: OrchestratorPosition,
    #[serde(default)]
    pub layout: Option<LayoutConfig>,
    #[serde(default)]
    pub workflow_policy: WorkflowPolicy,
    #[serde(default)]
    pub cost: CostConfig,
    #[serde(default)]
    pub grafana: GrafanaConfig,
    #[serde(default)]
    pub use_shim: bool,
    #[serde(default = "default_use_sdk_mode")]
    pub use_sdk_mode: bool,
    #[serde(default = "default_auto_respawn_on_crash")]
    pub auto_respawn_on_crash: bool,
    #[serde(default = "default_shim_health_check_interval_secs")]
    pub shim_health_check_interval_secs: u64,
    #[serde(default = "default_shim_health_timeout_secs")]
    pub shim_health_timeout_secs: u64,
    #[serde(default = "default_shim_shutdown_timeout_secs")]
    pub shim_shutdown_timeout_secs: u32,
    #[serde(default = "default_shim_working_state_timeout_secs")]
    pub shim_working_state_timeout_secs: u64,
    #[serde(default = "default_pending_queue_max_age_secs")]
    pub pending_queue_max_age_secs: u64,
    #[serde(default = "default_event_log_max_bytes")]
    pub event_log_max_bytes: u64,
    #[serde(default = "default_retro_min_duration_secs")]
    pub retro_min_duration_secs: u64,
    pub roles: Vec<RoleDef>,
}

impl From<TeamConfigWire> for TeamConfig {
    fn from(wire: TeamConfigWire) -> Self {
        let orchestrator_pane = wire
            .orchestrator_pane
            .unwrap_or_else(default_orchestrator_pane);
        let workflow_mode = wire.workflow_mode.unwrap_or_else(|| {
            if matches!(wire.orchestrator_pane, Some(true)) {
                WorkflowMode::Hybrid
            } else {
                default_workflow_mode()
            }
        });

        Self {
            name: wire.name,
            agent: wire.agent,
            workflow_mode,
            board: wire.board,
            standup: wire.standup,
            automation: wire.automation,
            automation_sender: wire.automation_sender,
            external_senders: wire.external_senders,
            orchestrator_pane,
            orchestrator_position: wire.orchestrator_position,
            layout: wire.layout,
            workflow_policy: wire.workflow_policy,
            cost: wire.cost,
            grafana: wire.grafana,
            use_shim: wire.use_shim,
            use_sdk_mode: wire.use_sdk_mode,
            auto_respawn_on_crash: wire.auto_respawn_on_crash,
            shim_health_check_interval_secs: wire.shim_health_check_interval_secs,
            shim_health_timeout_secs: wire.shim_health_timeout_secs,
            shim_shutdown_timeout_secs: wire.shim_shutdown_timeout_secs,
            shim_working_state_timeout_secs: wire.shim_working_state_timeout_secs,
            pending_queue_max_age_secs: wire.pending_queue_max_age_secs,
            event_log_max_bytes: wire.event_log_max_bytes,
            retro_min_duration_secs: wire.retro_min_duration_secs,
            roles: wire.roles,
        }
    }
}

impl<'de> Deserialize<'de> for TeamConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        TeamConfigWire::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrafanaConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_grafana_port")]
    pub port: u16,
}

impl Default for GrafanaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_grafana_port(),
        }
    }
}

fn default_grafana_port() -> u16 {
    3000
}

fn default_use_sdk_mode() -> bool {
    true
}

fn default_auto_respawn_on_crash() -> bool {
    true
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
    #[serde(default = "default_planning_cycle_cooldown_secs")]
    pub planning_cycle_cooldown_secs: u64,
    #[serde(default = "default_narration_detection_threshold")]
    pub narration_detection_threshold: usize,
    #[serde(default = "default_context_pressure_threshold_bytes")]
    pub context_pressure_threshold_bytes: u64,
    #[serde(default = "default_context_pressure_restart_delay_secs")]
    pub context_pressure_restart_delay_secs: u64,
    #[serde(default = "default_graceful_shutdown_timeout_secs")]
    pub graceful_shutdown_timeout_secs: u64,
    #[serde(default = "default_auto_commit_on_restart")]
    pub auto_commit_on_restart: bool,
    #[serde(default = "default_uncommitted_warn_threshold")]
    pub uncommitted_warn_threshold: usize,
    #[serde(default)]
    pub test_command: Option<String>,
    #[serde(default)]
    pub auto_merge: AutoMergePolicy,
    /// When true, context exhaustion restarts capture a work summary and
    /// inject it into the new agent session so it can continue where the
    /// old session left off.
    #[serde(default = "default_context_handoff_enabled")]
    pub context_handoff_enabled: bool,
    /// Number of PTY screen pages to include in the handoff summary.
    #[serde(default = "default_handoff_screen_history")]
    pub handoff_screen_history: usize,
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
            planning_cycle_cooldown_secs: default_planning_cycle_cooldown_secs(),
            narration_detection_threshold: default_narration_detection_threshold(),
            context_pressure_threshold_bytes: default_context_pressure_threshold_bytes(),
            context_pressure_restart_delay_secs: default_context_pressure_restart_delay_secs(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout_secs(),
            auto_commit_on_restart: default_auto_commit_on_restart(),
            uncommitted_warn_threshold: default_uncommitted_warn_threshold(),
            test_command: None,
            auto_merge: AutoMergePolicy::default(),
            context_handoff_enabled: default_context_handoff_enabled(),
            handoff_screen_history: default_handoff_screen_history(),
        }
    }
}

fn default_graceful_shutdown_timeout_secs() -> u64 {
    5
}

fn default_auto_commit_on_restart() -> bool {
    true
}

fn default_context_handoff_enabled() -> bool {
    true
}

fn default_handoff_screen_history() -> usize {
    20
}

fn default_planning_cycle_cooldown_secs() -> u64 {
    300
}

fn default_context_pressure_threshold_bytes() -> u64 {
    512_000
}

fn default_context_pressure_restart_delay_secs() -> u64 {
    120
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
fn default_narration_detection_threshold() -> usize {
    6
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
    Left,
    Bottom,
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
    #[serde(default)]
    pub clean_room_mode: bool,
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
            clean_room_mode: false,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleType {
    User,
    Architect,
    Manager,
    Engineer,
}

// --- Default value functions ---

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

fn default_shim_health_check_interval_secs() -> u64 {
    60
}

fn default_shim_health_timeout_secs() -> u64 {
    120
}

fn default_shim_shutdown_timeout_secs() -> u32 {
    30
}

fn default_shim_working_state_timeout_secs() -> u64 {
    600 // 10 minutes
}

fn default_pending_queue_max_age_secs() -> u64 {
    600 // 10 minutes
}
