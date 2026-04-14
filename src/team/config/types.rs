//! Type definitions for team configuration.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, de};

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
    pub clean_room_mode: bool,
    #[serde(default)]
    pub barrier_groups: HashMap<String, Vec<String>>,
    #[serde(default = "default_handoff_directory")]
    pub handoff_directory: String,
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
    #[serde(default = "default_stale_in_progress_hours")]
    pub stale_in_progress_hours: u64,
    #[serde(default = "default_aged_todo_hours")]
    pub aged_todo_hours: u64,
    #[serde(default = "default_stale_review_hours")]
    pub stale_review_hours: u64,
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
    #[serde(default = "default_narration_threshold")]
    pub narration_threshold: f64,
    #[serde(default = "default_narration_nudge_max")]
    pub narration_nudge_max: u32,
    #[serde(default = "default_narration_detection_enabled")]
    pub narration_detection_enabled: bool,
    #[serde(default = "default_narration_threshold_polls")]
    pub narration_threshold_polls: u32,
    #[serde(default = "default_context_pressure_threshold")]
    pub context_pressure_threshold: u64,
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
    /// Legacy global test command. Verification now prefers
    /// `workflow_policy.verification.test_command` and falls back here.
    #[serde(default)]
    pub test_command: Option<String>,
    #[serde(default)]
    pub verification: VerificationPolicy,
    #[serde(default)]
    pub claim_ttl: ClaimTtlPolicy,
    #[serde(default)]
    pub allocation: AllocationPolicy,
    #[serde(default)]
    pub main_smoke: MainSmokePolicy,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ClaimTtlPolicy {
    #[serde(default = "default_claim_ttl_default_secs")]
    pub default_secs: u64,
    #[serde(default = "default_claim_ttl_critical_secs")]
    pub critical_secs: u64,
    #[serde(default = "default_claim_ttl_max_extensions")]
    pub max_extensions: u32,
    #[serde(default = "default_claim_ttl_progress_check_interval_secs")]
    pub progress_check_interval_secs: u64,
    #[serde(default = "default_claim_ttl_warning_secs")]
    pub warning_secs: u64,
}

impl Default for ClaimTtlPolicy {
    fn default() -> Self {
        Self {
            default_secs: default_claim_ttl_default_secs(),
            critical_secs: default_claim_ttl_critical_secs(),
            max_extensions: default_claim_ttl_max_extensions(),
            progress_check_interval_secs: default_claim_ttl_progress_check_interval_secs(),
            warning_secs: default_claim_ttl_warning_secs(),
        }
    }
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
            clean_room_mode: false,
            barrier_groups: HashMap::new(),
            handoff_directory: default_handoff_directory(),
            wip_limit_per_engineer: None,
            wip_limit_per_reviewer: None,
            pipeline_starvation_threshold: default_pipeline_starvation_threshold(),
            escalation_threshold_secs: default_escalation_threshold_secs(),
            review_nudge_threshold_secs: default_review_nudge_threshold_secs(),
            review_timeout_secs: default_review_timeout_secs(),
            stale_in_progress_hours: default_stale_in_progress_hours(),
            aged_todo_hours: default_aged_todo_hours(),
            stale_review_hours: default_stale_review_hours(),
            review_timeout_overrides: HashMap::new(),
            auto_archive_done_after_secs: None,
            capability_overrides: HashMap::new(),
            stall_threshold_secs: default_stall_threshold_secs(),
            max_stall_restarts: default_max_stall_restarts(),
            health_check_interval_secs: default_health_check_interval_secs(),
            planning_cycle_cooldown_secs: default_planning_cycle_cooldown_secs(),
            narration_threshold: default_narration_threshold(),
            narration_nudge_max: default_narration_nudge_max(),
            narration_detection_enabled: default_narration_detection_enabled(),
            narration_threshold_polls: default_narration_threshold_polls(),
            context_pressure_threshold: default_context_pressure_threshold(),
            context_pressure_threshold_bytes: default_context_pressure_threshold_bytes(),
            context_pressure_restart_delay_secs: default_context_pressure_restart_delay_secs(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout_secs(),
            auto_commit_on_restart: default_auto_commit_on_restart(),
            uncommitted_warn_threshold: default_uncommitted_warn_threshold(),
            test_command: None,
            verification: VerificationPolicy::default(),
            claim_ttl: ClaimTtlPolicy::default(),
            allocation: AllocationPolicy::default(),
            main_smoke: MainSmokePolicy::default(),
            auto_merge: AutoMergePolicy::default(),
            context_handoff_enabled: default_context_handoff_enabled(),
            handoff_screen_history: default_handoff_screen_history(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationStrategy {
    RoundRobin,
    Scored,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AllocationPolicy {
    #[serde(default = "default_allocation_strategy")]
    pub strategy: AllocationStrategy,
    #[serde(default = "default_allocation_tag_weight")]
    pub tag_weight: i32,
    #[serde(default = "default_allocation_file_overlap_weight")]
    pub file_overlap_weight: i32,
    #[serde(default = "default_allocation_load_penalty")]
    pub load_penalty: i32,
    #[serde(default = "default_allocation_conflict_penalty")]
    pub conflict_penalty: i32,
    #[serde(default = "default_allocation_experience_bonus")]
    pub experience_bonus: i32,
}

impl Default for AllocationPolicy {
    fn default() -> Self {
        Self {
            strategy: default_allocation_strategy(),
            tag_weight: default_allocation_tag_weight(),
            file_overlap_weight: default_allocation_file_overlap_weight(),
            load_penalty: default_allocation_load_penalty(),
            conflict_penalty: default_allocation_conflict_penalty(),
            experience_bonus: default_allocation_experience_bonus(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MainSmokePolicy {
    #[serde(default = "default_main_smoke_enabled")]
    pub enabled: bool,
    #[serde(default = "default_main_smoke_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_main_smoke_command")]
    pub command: String,
    #[serde(default = "default_main_smoke_pause_dispatch_on_failure")]
    pub pause_dispatch_on_failure: bool,
    #[serde(default = "default_main_smoke_auto_revert")]
    pub auto_revert: bool,
}

impl Default for MainSmokePolicy {
    fn default() -> Self {
        Self {
            enabled: default_main_smoke_enabled(),
            interval_secs: default_main_smoke_interval_secs(),
            command: default_main_smoke_command(),
            pause_dispatch_on_failure: default_main_smoke_pause_dispatch_on_failure(),
            auto_revert: default_main_smoke_auto_revert(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerificationPolicy {
    #[serde(default = "default_verification_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_verification_auto_run_tests")]
    pub auto_run_tests: bool,
    #[serde(default = "default_verification_require_evidence")]
    pub require_evidence: bool,
    #[serde(default)]
    pub test_command: Option<String>,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self {
            max_iterations: default_verification_max_iterations(),
            auto_run_tests: default_verification_auto_run_tests(),
            require_evidence: default_verification_require_evidence(),
            test_command: None,
        }
    }
}

fn default_graceful_shutdown_timeout_secs() -> u64 {
    30
}

fn default_verification_max_iterations() -> u32 {
    5
}

fn default_verification_auto_run_tests() -> bool {
    true
}

fn default_verification_require_evidence() -> bool {
    true
}

fn default_claim_ttl_default_secs() -> u64 {
    1800
}

fn default_claim_ttl_critical_secs() -> u64 {
    900
}

fn default_claim_ttl_max_extensions() -> u32 {
    2
}

fn default_claim_ttl_progress_check_interval_secs() -> u64 {
    120
}

fn default_claim_ttl_warning_secs() -> u64 {
    300
}

fn default_allocation_strategy() -> AllocationStrategy {
    AllocationStrategy::Scored
}

fn default_allocation_tag_weight() -> i32 {
    15
}

fn default_allocation_file_overlap_weight() -> i32 {
    10
}

fn default_allocation_load_penalty() -> i32 {
    8
}

fn default_allocation_conflict_penalty() -> i32 {
    12
}

fn default_allocation_experience_bonus() -> i32 {
    3
}

fn default_main_smoke_enabled() -> bool {
    true
}

fn default_main_smoke_interval_secs() -> u64 {
    600
}

fn default_main_smoke_command() -> String {
    "cargo check".to_string()
}

fn default_main_smoke_pause_dispatch_on_failure() -> bool {
    true
}

fn default_main_smoke_auto_revert() -> bool {
    false
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

fn default_handoff_directory() -> String {
    ".batty/handoff".to_string()
}

fn default_planning_cycle_cooldown_secs() -> u64 {
    300
}

fn default_context_pressure_threshold() -> u64 {
    100
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
    #[serde(default = "default_auto_merge_enabled")]
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
    #[serde(default = "default_post_merge_verify")]
    pub post_merge_verify: bool,
}

fn default_max_diff_lines() -> usize {
    2000 // Real tasks produce 300-600 lines; 200 blocked everything
}
fn default_auto_merge_enabled() -> bool {
    true
}
fn default_max_files_changed() -> usize {
    30 // Real tasks touch 8-14 files; 5 blocked everything
}
fn default_max_modules_touched() -> usize {
    10 // Real tasks touch 3-5 modules; 2 blocked everything
}
fn default_narration_threshold() -> f64 {
    0.8
}
fn default_narration_nudge_max() -> u32 {
    2
}
fn default_narration_detection_enabled() -> bool {
    true
}
fn default_narration_threshold_polls() -> u32 {
    5
}
fn default_confidence_threshold() -> f64 {
    0.0 // Trust tests as the merge gate, not heuristic confidence scoring
}
fn default_require_tests_pass() -> bool {
    true
}
fn default_post_merge_verify() -> bool {
    true
}

impl Default for AutoMergePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_diff_lines: default_max_diff_lines(),
            max_files_changed: default_max_files_changed(),
            max_modules_touched: default_max_modules_touched(),
            sensitive_paths: default_sensitive_paths(),
            confidence_threshold: default_confidence_threshold(),
            require_tests_pass: default_require_tests_pass(),
            post_merge_verify: default_post_merge_verify(),
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
    BoardFirst,
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
        matches!(self, Self::WorkflowFirst | Self::BoardFirst)
    }

    pub fn enables_runtime_surface(self) -> bool {
        matches!(self, Self::Hybrid | Self::WorkflowFirst | Self::BoardFirst)
    }

    pub fn suppresses_manager_relay(self) -> bool {
        matches!(self, Self::BoardFirst)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Hybrid => "hybrid",
            Self::WorkflowFirst => "workflow_first",
            Self::BoardFirst => "board_first",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoardConfig {
    #[serde(default = "default_rotation_threshold")]
    pub rotation_threshold: u32,
    #[serde(default = "default_board_auto_dispatch")]
    pub auto_dispatch: bool,
    #[serde(default = "default_worktree_stale_rebase_threshold")]
    pub worktree_stale_rebase_threshold: u32,
    #[serde(default = "default_board_auto_replenish")]
    pub auto_replenish: bool,
    #[serde(default = "default_state_reconciliation_interval_secs")]
    pub state_reconciliation_interval_secs: u64,
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
            worktree_stale_rebase_threshold: default_worktree_stale_rebase_threshold(),
            auto_replenish: default_board_auto_replenish(),
            state_reconciliation_interval_secs: default_state_reconciliation_interval_secs(),
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
    #[serde(default)]
    pub disk_hygiene: DiskHygieneConfig,
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
            disk_hygiene: DiskHygieneConfig::default(),
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
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<ClaudeAuthMode>,
    #[serde(default)]
    pub auth_env: Vec<String>,
    #[serde(default = "default_instances")]
    pub instances: u32,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub posture: Option<String>,
    #[serde(default)]
    pub model_class: Option<String>,
    #[serde(default)]
    pub provider_overlay: Option<String>,
    #[serde(default)]
    pub instance_overrides: HashMap<String, RoleInstanceOverride>,
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
    pub barrier_group: Option<String>,
    #[serde(default)]
    pub use_worktrees: bool,
}

impl Default for RoleDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            role_type: RoleType::Engineer,
            agent: None,
            model: None,
            auth_mode: None,
            auth_env: Vec::new(),
            instances: default_instances(),
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            instance_overrides: HashMap::new(),
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoleInstanceOverride {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub posture: Option<String>,
    #[serde(default)]
    pub model_class: Option<String>,
    #[serde(default)]
    pub provider_overlay: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAuthMode {
    #[default]
    Oauth,
    ApiKey,
    Custom,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub provider: String,
    /// Telegram bot token for native API (optional; falls back to provider CLI).
    /// Can also be set via `BATTY_TELEGRAM_BOT_TOKEN` env var.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// User IDs allowed to send messages (access control).
    ///
    /// Accepts either numeric YAML values or quoted strings so the same field
    /// can represent Telegram integers and Discord snowflake IDs.
    #[serde(default, deserialize_with = "deserialize_user_id_list")]
    pub allowed_user_ids: Vec<i64>,
    #[serde(default)]
    pub events_channel_id: Option<String>,
    #[serde(default)]
    pub agents_channel_id: Option<String>,
    #[serde(default)]
    pub commands_channel_id: Option<String>,
    #[serde(default)]
    pub board_channel_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UserIdValue {
    Integer(i64),
    String(String),
}

fn deserialize_user_id_list<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<UserIdValue>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            UserIdValue::Integer(id) => Ok(id),
            UserIdValue::String(raw) => raw
                .parse::<i64>()
                .map_err(|error| de::Error::custom(format!("invalid user id '{raw}': {error}"))),
        })
        .collect()
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

fn default_worktree_stale_rebase_threshold() -> u32 {
    5
}

fn default_board_auto_replenish() -> bool {
    true
}

fn default_state_reconciliation_interval_secs() -> u64 {
    30
}

fn default_dispatch_stabilization_delay_secs() -> u64 {
    30
}

fn default_dispatch_dedup_window_secs() -> u64 {
    900 // 15 minutes — prevents dispatch→fail→re-enqueue loops
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

fn default_stale_in_progress_hours() -> u64 {
    4
}

fn default_aged_todo_hours() -> u64 {
    48
}

fn default_stale_review_hours() -> u64 {
    1
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

fn default_disk_hygiene_check_interval_secs() -> u64 {
    600
}

fn default_disk_hygiene_min_free_gb() -> u64 {
    10
}

fn default_disk_hygiene_max_shared_target_gb() -> u64 {
    4
}

fn default_disk_hygiene_log_rotation_hours() -> u64 {
    24
}

/// Configuration for automated disk hygiene during long runs.
#[derive(Debug, Clone, Deserialize)]
pub struct DiskHygieneConfig {
    /// Enable automated disk hygiene checks.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Interval in seconds between periodic disk pressure checks.
    #[serde(default = "default_disk_hygiene_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Minimum free disk space in GB before triggering cleanup.
    #[serde(default = "default_disk_hygiene_min_free_gb")]
    pub min_free_gb: u64,
    /// Maximum size in GB for the shared-target directory.
    #[serde(default = "default_disk_hygiene_max_shared_target_gb")]
    pub max_shared_target_gb: u64,
    /// Hours after which shim-logs and inbox messages are rotated.
    #[serde(default = "default_disk_hygiene_log_rotation_hours")]
    pub log_rotation_hours: u64,
    /// Run `cargo clean --profile dev` in engineer worktree shared-target after merge.
    #[serde(default = "default_enabled")]
    pub post_merge_cleanup: bool,
    /// Prune completed task branches after merge.
    #[serde(default = "default_enabled")]
    pub prune_merged_branches: bool,
}

impl Default for DiskHygieneConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            check_interval_secs: default_disk_hygiene_check_interval_secs(),
            min_free_gb: default_disk_hygiene_min_free_gb(),
            max_shared_target_gb: default_disk_hygiene_max_shared_target_gb(),
            log_rotation_hours: default_disk_hygiene_log_rotation_hours(),
            post_merge_cleanup: default_enabled(),
            prune_merged_branches: default_enabled(),
        }
    }
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
    7200 // 2 hours — modern agents work for extended periods
}

fn default_pending_queue_max_age_secs() -> u64 {
    600 // 10 minutes
}
