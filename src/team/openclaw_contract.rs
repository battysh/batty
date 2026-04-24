//! Stable Batty <-> OpenClaw DTOs and capability negotiation.
//!
//! This module is the anti-corruption boundary between Batty's internal
//! supervision model and the external OpenClaw-facing contract. Batty remains
//! authoritative for prompts, workflow policy, and operator actions; this
//! module exports only versioned DTOs, enums, and counters.

use serde::{Deserialize, Serialize};

use super::{events, status};

#[cfg(test)]
use crate::project_registry::RegisteredProject;

pub const CONTRACT_SCHEMA_VERSION: u32 = 1;
pub const MIN_SUPPORTED_SCHEMA_VERSION: u32 = 1;
pub const CONTRACT_DESCRIPTOR_KIND: &str = "batty.openclaw.contractDescriptor";
pub const TEAM_STATUS_KIND: &str = "batty.openclaw.teamStatus";
pub const TEAM_EVENT_KIND: &str = "batty.openclaw.teamEvent";
pub const TEAM_COMMAND_KIND: &str = "batty.openclaw.teamCommand";
pub const CAPABILITY_NEGOTIATION_KIND: &str = "batty.openclaw.capabilityNegotiation";

const SUPPORTED_CAPABILITIES: &[OpenClawCapability] = &[
    OpenClawCapability::TeamStatus,
    OpenClawCapability::TeamEvents,
    OpenClawCapability::TeamCommands,
    OpenClawCapability::EscalationSurface,
    OpenClawCapability::ApprovalSurface,
    OpenClawCapability::CapabilityNegotiation,
];

const SUPPORTED_EVENT_KINDS: &[TeamEventKind] = &[
    TeamEventKind::TaskCompleted,
    TeamEventKind::ReviewNudged,
    TeamEventKind::ReviewEscalated,
    TeamEventKind::ReviewStalled,
    TeamEventKind::AgentStalled,
    TeamEventKind::TaskStalled,
    TeamEventKind::TaskMergedAutomatic,
    TeamEventKind::TaskMergedManual,
    TeamEventKind::TaskEscalated,
    TeamEventKind::VerificationEscalated,
    TeamEventKind::DeliveryFailed,
    TeamEventKind::SessionStarted,
    TeamEventKind::SessionReloading,
    TeamEventKind::SessionReloaded,
    TeamEventKind::SessionStopped,
    TeamEventKind::AgentStarted,
    TeamEventKind::AgentRestarted,
    TeamEventKind::AgentCrashed,
    TeamEventKind::AgentStopped,
    TeamEventKind::AgentRespawned,
    TeamEventKind::AgentContextExhausted,
    TeamEventKind::AgentHealthChanged,
    TeamEventKind::SessionTopologyChanged,
    TeamEventKind::AgentRemoved,
];

const HUMAN_ONLY_DECISIONS: &[HumanDecisionKind] = &[
    HumanDecisionKind::StopSession,
    HumanDecisionKind::RestartSession,
    HumanDecisionKind::ReviewDisposition,
    HumanDecisionKind::MergeDisposition,
    HumanDecisionKind::PolicyOverride,
];

const ESCALATION_KINDS: &[EscalationKind] = &[
    EscalationKind::SessionUnavailable,
    EscalationKind::MemberUnhealthy,
    EscalationKind::ReviewQueueBlocked,
    EscalationKind::TaskBlocked,
    EscalationKind::HumanApprovalRequired,
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OpenClawCapability {
    TeamStatus,
    TeamEvents,
    TeamCommands,
    EscalationSurface,
    ApprovalSurface,
    CapabilityNegotiation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamLifecycle {
    Running,
    Stopped,
    Degraded,
    Recovering,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Starting,
    Idle,
    Working,
    Done,
    Crashed,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemberHealth {
    Healthy,
    Warning,
    Unhealthy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BackendHealth {
    Healthy,
    Degraded,
    Unreachable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamEventTopic {
    Completion,
    Review,
    Stall,
    Merge,
    Escalation,
    DeliveryFailure,
    Lifecycle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamEventKind {
    TaskCompleted,
    ReviewNudged,
    ReviewEscalated,
    ReviewStalled,
    AgentStalled,
    TaskStalled,
    TaskMergedAutomatic,
    TaskMergedManual,
    TaskEscalated,
    VerificationEscalated,
    DeliveryFailed,
    SessionStarted,
    SessionReloading,
    SessionReloaded,
    SessionStopped,
    AgentStarted,
    AgentRestarted,
    AgentCrashed,
    AgentStopped,
    AgentRespawned,
    AgentContextExhausted,
    AgentHealthChanged,
    SessionTopologyChanged,
    AgentRemoved,
}

impl TeamEventKind {
    pub(crate) const fn legacy_event_type(self) -> &'static str {
        match self {
            Self::TaskCompleted => "task.completed",
            Self::ReviewNudged => "review.nudged",
            Self::ReviewEscalated => "review.escalated",
            Self::ReviewStalled => "review.stalled",
            Self::AgentStalled => "agent.stalled",
            Self::TaskStalled => "task.stalled",
            Self::TaskMergedAutomatic => "task.merged.automatic",
            Self::TaskMergedManual => "task.merged.manual",
            Self::TaskEscalated => "task.escalated",
            Self::VerificationEscalated => "verification.escalated",
            Self::DeliveryFailed => "delivery.failed",
            Self::SessionStarted => "session.started",
            Self::SessionReloading => "session.reloading",
            Self::SessionReloaded => "session.reloaded",
            Self::SessionStopped => "session.stopped",
            Self::AgentStarted => "agent.started",
            Self::AgentRestarted => "agent.restarted",
            Self::AgentCrashed => "agent.crashed",
            Self::AgentStopped => "agent.stopped",
            Self::AgentRespawned => "agent.respawned",
            Self::AgentContextExhausted => "agent.context_exhausted",
            Self::AgentHealthChanged => "agent.health_changed",
            Self::SessionTopologyChanged => "session.topology_changed",
            Self::AgentRemoved => "agent.removed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamCommandKind {
    Start,
    Stop,
    Restart,
    Send,
    Nudge,
    Review,
    Merge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CommandScope {
    Team,
    Member,
    Task,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLevel {
    NotRequired,
    Suggested,
    Required,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HumanDecisionKind {
    StopSession,
    RestartSession,
    ReviewDisposition,
    MergeDisposition,
    PolicyOverride,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EscalationAuthority {
    RecommendOnly,
    HumanApproved,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EscalationKind {
    SessionUnavailable,
    MemberUnhealthy,
    ReviewQueueBlocked,
    TaskBlocked,
    HumanApprovalRequired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition {
    Request,
    Approve,
    Rework,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    FastForward,
    Squash,
    Rebase,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DtoKinds {
    pub team_status: String,
    pub team_event: String,
    pub team_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VersioningPolicy {
    pub current_schema_version: u32,
    pub min_supported_schema_version: u32,
    pub add_only_fields: bool,
    pub new_enum_variants_require_capability_review: bool,
    pub incompatible_changes_require_new_schema_version: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AntiCorruptionBoundary {
    pub batty_is_system_of_record: bool,
    pub prompt_wording_leaks_forbidden: bool,
    pub command_intents_are_explicit: bool,
    pub status_inputs: Vec<String>,
    pub event_inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandPolicy {
    pub command: TeamCommandKind,
    pub scope: CommandScope,
    pub approval_level: ApprovalLevel,
    #[serde(default)]
    pub human_only_decisions: Vec<HumanDecisionKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EscalationSurface {
    pub authority: EscalationAuthority,
    #[serde(default)]
    pub supported_kinds: Vec<EscalationKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalSurface {
    #[serde(default)]
    pub command_policies: Vec<CommandPolicy>,
    #[serde(default)]
    pub human_only_decisions: Vec<HumanDecisionKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContractDescriptor {
    pub kind: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub dto_kinds: DtoKinds,
    pub versioning: VersioningPolicy,
    pub anti_corruption_boundary: AntiCorruptionBoundary,
    #[serde(default)]
    pub capabilities: Vec<OpenClawCapability>,
    #[serde(default)]
    pub supported_event_kinds: Vec<TeamEventKind>,
    pub escalation_surface: EscalationSurface,
    pub approval_surface: ApprovalSurface,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityNegotiationRequest {
    pub kind: String,
    pub requested_schema_version: u32,
    pub min_compatible_schema_version: u32,
    #[serde(default)]
    pub requested_capabilities: Vec<OpenClawCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityNegotiationResult {
    pub kind: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiated_schema_version: Option<u32>,
    #[serde(default)]
    pub granted_capabilities: Vec<OpenClawCapability>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemberStatus {
    pub name: String,
    pub role: String,
    pub role_type: String,
    pub state: MemberState,
    pub health: MemberHealth,
    #[serde(default)]
    pub active_task_ids: Vec<String>,
    #[serde(default)]
    pub review_task_ids: Vec<String>,
    pub pending_inbox_count: usize,
    pub triage_backlog_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub restart_count: u32,
    pub context_exhaustion_count: u32,
    pub delivery_failure_count: u32,
    pub supervisory_digest_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_elapsed_secs: Option<u64>,
    pub backend_health: BackendHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PipelineMetrics {
    pub active_task_count: usize,
    pub review_queue_count: usize,
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub in_progress_count: u32,
    pub stale_in_progress_count: u32,
    pub stale_review_count: u32,
    pub triage_backlog_count: usize,
    pub unhealthy_member_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rework_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_review_latency_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamStatus {
    pub kind: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub team_name: String,
    pub session_name: String,
    pub lifecycle: TeamLifecycle,
    pub running: bool,
    pub paused: bool,
    #[serde(default)]
    pub members: Vec<MemberStatus>,
    pub pipeline: PipelineMetrics,
    pub escalation_surface: EscalationSurface,
    pub approval_surface: ApprovalSurface,
    #[serde(default)]
    pub capabilities: Vec<OpenClawCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamEvent {
    pub topic: TeamEventTopic,
    pub event_kind: TeamEventKind,
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_running: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEventEnvelope {
    pub kind: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub project_id: String,
    pub project_name: String,
    pub project_root: String,
    pub team_name: String,
    pub session_name: String,
    pub event: TeamEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandActor {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequirement {
    pub level: ApprovalLevel,
    #[serde(default)]
    pub human_only_decisions: Vec<HumanDecisionKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleCommand {
    pub scope: CommandScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestartCommand {
    pub scope: CommandScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SendCommand {
    pub from_role: String,
    pub to_role: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NudgeCommand {
    pub member_name: String,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommand {
    pub task_id: String,
    pub disposition: ReviewDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MergeCommand {
    pub task_id: String,
    pub strategy: MergeStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TeamCommandAction {
    Start(LifecycleCommand),
    Stop(LifecycleCommand),
    Restart(RestartCommand),
    Send(SendCommand),
    Nudge(NudgeCommand),
    Review(ReviewCommand),
    Merge(MergeCommand),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TeamCommand {
    pub kind: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub project_id: String,
    pub actor: CommandActor,
    pub approval: ApprovalRequirement,
    #[serde(flatten)]
    pub action: TeamCommandAction,
}

impl TeamCommand {
    pub fn start(project_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self::lifecycle_command(
            project_id,
            source,
            TeamCommandAction::Start(LifecycleCommand {
                scope: CommandScope::Team,
                member_name: None,
            }),
        )
    }

    pub fn stop(project_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self::lifecycle_command(
            project_id,
            source,
            TeamCommandAction::Stop(LifecycleCommand {
                scope: CommandScope::Team,
                member_name: None,
            }),
        )
    }

    pub fn restart(
        project_id: impl Into<String>,
        source: impl Into<String>,
        member_name: Option<String>,
        reason_code: Option<String>,
    ) -> Self {
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(TeamCommandKind::Restart),
            action: TeamCommandAction::Restart(RestartCommand {
                scope: if member_name.is_some() {
                    CommandScope::Member
                } else {
                    CommandScope::Team
                },
                member_name,
                reason_code,
            }),
        }
    }

    pub fn send(
        project_id: impl Into<String>,
        source: impl Into<String>,
        from_role: impl Into<String>,
        to_role: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(TeamCommandKind::Send),
            action: TeamCommandAction::Send(SendCommand {
                from_role: from_role.into(),
                to_role: to_role.into(),
                message: message.into(),
            }),
        }
    }

    pub fn nudge(
        project_id: impl Into<String>,
        source: impl Into<String>,
        member_name: impl Into<String>,
        reason_code: impl Into<String>,
        summary: Option<String>,
    ) -> Self {
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(TeamCommandKind::Nudge),
            action: TeamCommandAction::Nudge(NudgeCommand {
                member_name: member_name.into(),
                reason_code: reason_code.into(),
                summary,
            }),
        }
    }

    pub fn review(
        project_id: impl Into<String>,
        source: impl Into<String>,
        task_id: impl Into<String>,
        disposition: ReviewDisposition,
        reviewer_role: Option<String>,
    ) -> Self {
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(TeamCommandKind::Review),
            action: TeamCommandAction::Review(ReviewCommand {
                task_id: task_id.into(),
                disposition,
                reviewer_role,
            }),
        }
    }

    pub fn merge(
        project_id: impl Into<String>,
        source: impl Into<String>,
        task_id: impl Into<String>,
        strategy: MergeStrategy,
    ) -> Self {
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(TeamCommandKind::Merge),
            action: TeamCommandAction::Merge(MergeCommand {
                task_id: task_id.into(),
                strategy,
            }),
        }
    }

    fn lifecycle_command(
        project_id: impl Into<String>,
        source: impl Into<String>,
        action: TeamCommandAction,
    ) -> Self {
        let kind = match &action {
            TeamCommandAction::Start(_) => TeamCommandKind::Start,
            TeamCommandAction::Stop(_) => TeamCommandKind::Stop,
            TeamCommandAction::Restart(_) => TeamCommandKind::Restart,
            TeamCommandAction::Send(_) => TeamCommandKind::Send,
            TeamCommandAction::Nudge(_) => TeamCommandKind::Nudge,
            TeamCommandAction::Review(_) => TeamCommandKind::Review,
            TeamCommandAction::Merge(_) => TeamCommandKind::Merge,
        };
        Self {
            kind: TEAM_COMMAND_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            project_id: project_id.into(),
            actor: CommandActor {
                source: source.into(),
                source_role: None,
            },
            approval: approval_for_command(kind),
            action,
        }
    }
}

pub fn descriptor() -> ContractDescriptor {
    ContractDescriptor {
        kind: CONTRACT_DESCRIPTOR_KIND.to_string(),
        schema_version: CONTRACT_SCHEMA_VERSION,
        min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
        dto_kinds: DtoKinds {
            team_status: TEAM_STATUS_KIND.to_string(),
            team_event: TEAM_EVENT_KIND.to_string(),
            team_command: TEAM_COMMAND_KIND.to_string(),
        },
        versioning: VersioningPolicy {
            current_schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            add_only_fields: true,
            new_enum_variants_require_capability_review: true,
            incompatible_changes_require_new_schema_version: true,
        },
        anti_corruption_boundary: AntiCorruptionBoundary {
            batty_is_system_of_record: true,
            prompt_wording_leaks_forbidden: true,
            command_intents_are_explicit: true,
            status_inputs: vec![
                "team status report".to_string(),
                "workflow metrics".to_string(),
                "member health counters".to_string(),
            ],
            event_inputs: vec![
                "team events jsonl".to_string(),
                "project registry".to_string(),
            ],
        },
        capabilities: supported_capabilities(),
        supported_event_kinds: supported_event_kinds(),
        escalation_surface: default_escalation_surface(),
        approval_surface: default_approval_surface(),
    }
}

pub fn negotiate_capabilities(
    request: &CapabilityNegotiationRequest,
) -> CapabilityNegotiationResult {
    if request.requested_schema_version < MIN_SUPPORTED_SCHEMA_VERSION {
        return CapabilityNegotiationResult {
            kind: CAPABILITY_NEGOTIATION_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            compatible: false,
            negotiated_schema_version: None,
            granted_capabilities: Vec::new(),
            reason: format!(
                "requested schema version {} is older than the minimum supported {}",
                request.requested_schema_version, MIN_SUPPORTED_SCHEMA_VERSION
            ),
        };
    }

    if request.min_compatible_schema_version > CONTRACT_SCHEMA_VERSION {
        return CapabilityNegotiationResult {
            kind: CAPABILITY_NEGOTIATION_KIND.to_string(),
            schema_version: CONTRACT_SCHEMA_VERSION,
            min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
            compatible: false,
            negotiated_schema_version: None,
            granted_capabilities: Vec::new(),
            reason: format!(
                "minimum compatible schema version {} is newer than Batty's current {}",
                request.min_compatible_schema_version, CONTRACT_SCHEMA_VERSION
            ),
        };
    }

    let granted_capabilities = if request.requested_capabilities.is_empty() {
        supported_capabilities()
    } else {
        SUPPORTED_CAPABILITIES
            .iter()
            .copied()
            .filter(|capability| request.requested_capabilities.contains(capability))
            .collect()
    };

    CapabilityNegotiationResult {
        kind: CAPABILITY_NEGOTIATION_KIND.to_string(),
        schema_version: CONTRACT_SCHEMA_VERSION,
        min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
        compatible: true,
        negotiated_schema_version: Some(
            request
                .requested_schema_version
                .min(CONTRACT_SCHEMA_VERSION),
        ),
        granted_capabilities,
        reason: "compatible via add-only schema evolution".to_string(),
    }
}

pub(crate) fn team_status_from_report(report: &status::TeamStatusJsonReport) -> TeamStatus {
    let workflow_metrics = report.workflow_metrics.clone().unwrap_or_default();
    TeamStatus {
        kind: TEAM_STATUS_KIND.to_string(),
        schema_version: CONTRACT_SCHEMA_VERSION,
        min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
        team_name: report.team.clone(),
        session_name: report.session.clone(),
        lifecycle: team_lifecycle(report),
        running: report.running,
        paused: report.paused,
        members: report.members.iter().map(member_status_from_row).collect(),
        pipeline: PipelineMetrics {
            active_task_count: report.active_tasks.len(),
            review_queue_count: report.review_queue.len(),
            runnable_count: workflow_metrics.runnable_count,
            blocked_count: workflow_metrics.blocked_count,
            in_review_count: workflow_metrics.in_review_count,
            in_progress_count: workflow_metrics.in_progress_count,
            stale_in_progress_count: workflow_metrics.stale_in_progress_count,
            stale_review_count: workflow_metrics.stale_review_count,
            triage_backlog_count: report.health.triage_backlog_count,
            unhealthy_member_count: report.health.unhealthy_members.len(),
            auto_merge_rate: workflow_metrics.auto_merge_rate,
            rework_rate: workflow_metrics.rework_rate,
            avg_review_latency_secs: workflow_metrics.avg_review_latency_secs,
        },
        escalation_surface: default_escalation_surface(),
        approval_surface: default_approval_surface(),
        capabilities: supported_capabilities(),
    }
}

#[cfg(test)]
pub(crate) fn project_event_from_internal(
    project: &RegisteredProject,
    event: &events::TeamEvent,
) -> Option<ProjectEventEnvelope> {
    let (topic, event_kind) = contract_for_internal_event(event)?;
    Some(ProjectEventEnvelope {
        kind: TEAM_EVENT_KIND.to_string(),
        schema_version: CONTRACT_SCHEMA_VERSION,
        min_supported_schema_version: MIN_SUPPORTED_SCHEMA_VERSION,
        project_id: project.project_id.clone(),
        project_name: project.name.clone(),
        project_root: project.project_root.display().to_string(),
        team_name: project.team_name.clone(),
        session_name: project.session_name.clone(),
        event: TeamEvent {
            topic,
            event_kind,
            ts: event.ts,
            member_name: event.role.clone(),
            task_id: event.task.clone(),
            sender: event.from.clone(),
            recipient: event.recipient.clone().or_else(|| event.to.clone()),
            reason: event.reason.clone(),
            detail: event.details.clone(),
            action_type: event.action_type.clone(),
            success: event.success,
            restart_count: event.restart_count,
            load: event.load,
            uptime_secs: event.uptime_secs,
            session_running: event.session_running,
        },
    })
}

pub(crate) fn contract_for_internal_event(
    event: &events::TeamEvent,
) -> Option<(TeamEventTopic, TeamEventKind)> {
    match event.event.as_str() {
        "task_completed" => Some((TeamEventTopic::Completion, TeamEventKind::TaskCompleted)),
        "review_nudge_sent" => Some((TeamEventTopic::Review, TeamEventKind::ReviewNudged)),
        "review_escalated" => Some((TeamEventTopic::Review, TeamEventKind::ReviewEscalated)),
        "review_stale" => Some((TeamEventTopic::Review, TeamEventKind::ReviewStalled)),
        "stall_detected" => Some((TeamEventTopic::Stall, TeamEventKind::AgentStalled)),
        "task_stale" => Some((TeamEventTopic::Stall, TeamEventKind::TaskStalled)),
        "task_auto_merged" => Some((TeamEventTopic::Merge, TeamEventKind::TaskMergedAutomatic)),
        "task_manual_merged" => Some((TeamEventTopic::Merge, TeamEventKind::TaskMergedManual)),
        "task_escalated" => Some((TeamEventTopic::Escalation, TeamEventKind::TaskEscalated)),
        "verification_max_iterations_reached" => Some((
            TeamEventTopic::Escalation,
            TeamEventKind::VerificationEscalated,
        )),
        "delivery_failed" => Some((
            TeamEventTopic::DeliveryFailure,
            TeamEventKind::DeliveryFailed,
        )),
        "daemon_started" => Some((TeamEventTopic::Lifecycle, TeamEventKind::SessionStarted)),
        "daemon_reloading" => Some((TeamEventTopic::Lifecycle, TeamEventKind::SessionReloading)),
        "daemon_reloaded" => Some((TeamEventTopic::Lifecycle, TeamEventKind::SessionReloaded)),
        "daemon_stopped" => Some((TeamEventTopic::Lifecycle, TeamEventKind::SessionStopped)),
        "agent_spawned" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentStarted)),
        "agent_restarted" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentRestarted)),
        "member_crashed" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentCrashed)),
        "pane_death" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentStopped)),
        "pane_respawned" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentRespawned)),
        "context_exhausted" => Some((
            TeamEventTopic::Lifecycle,
            TeamEventKind::AgentContextExhausted,
        )),
        "health_changed" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentHealthChanged)),
        "topology_changed" => Some((
            TeamEventTopic::Lifecycle,
            TeamEventKind::SessionTopologyChanged,
        )),
        "agent_removed" => Some((TeamEventTopic::Lifecycle, TeamEventKind::AgentRemoved)),
        _ => None,
    }
}

pub(crate) fn event_kind_from_legacy_event_type(event_type: &str) -> Option<TeamEventKind> {
    SUPPORTED_EVENT_KINDS
        .iter()
        .copied()
        .find(|kind| kind.legacy_event_type() == event_type)
}

fn supported_capabilities() -> Vec<OpenClawCapability> {
    SUPPORTED_CAPABILITIES.to_vec()
}

fn supported_event_kinds() -> Vec<TeamEventKind> {
    SUPPORTED_EVENT_KINDS.to_vec()
}

fn default_escalation_surface() -> EscalationSurface {
    EscalationSurface {
        authority: EscalationAuthority::RecommendOnly,
        supported_kinds: ESCALATION_KINDS.to_vec(),
    }
}

fn default_approval_surface() -> ApprovalSurface {
    ApprovalSurface {
        command_policies: vec![
            CommandPolicy {
                command: TeamCommandKind::Start,
                scope: CommandScope::Team,
                approval_level: ApprovalLevel::NotRequired,
                human_only_decisions: Vec::new(),
            },
            CommandPolicy {
                command: TeamCommandKind::Stop,
                scope: CommandScope::Team,
                approval_level: ApprovalLevel::Required,
                human_only_decisions: vec![HumanDecisionKind::StopSession],
            },
            CommandPolicy {
                command: TeamCommandKind::Restart,
                scope: CommandScope::Member,
                approval_level: ApprovalLevel::Suggested,
                human_only_decisions: vec![HumanDecisionKind::RestartSession],
            },
            CommandPolicy {
                command: TeamCommandKind::Send,
                scope: CommandScope::Member,
                approval_level: ApprovalLevel::NotRequired,
                human_only_decisions: Vec::new(),
            },
            CommandPolicy {
                command: TeamCommandKind::Nudge,
                scope: CommandScope::Member,
                approval_level: ApprovalLevel::Suggested,
                human_only_decisions: Vec::new(),
            },
            CommandPolicy {
                command: TeamCommandKind::Review,
                scope: CommandScope::Task,
                approval_level: ApprovalLevel::Required,
                human_only_decisions: vec![HumanDecisionKind::ReviewDisposition],
            },
            CommandPolicy {
                command: TeamCommandKind::Merge,
                scope: CommandScope::Task,
                approval_level: ApprovalLevel::Required,
                human_only_decisions: vec![HumanDecisionKind::MergeDisposition],
            },
        ],
        human_only_decisions: HUMAN_ONLY_DECISIONS.to_vec(),
    }
}

fn approval_for_command(command: TeamCommandKind) -> ApprovalRequirement {
    let policy = default_approval_surface()
        .command_policies
        .into_iter()
        .find(|policy| policy.command == command)
        .unwrap_or(CommandPolicy {
            command,
            scope: CommandScope::Team,
            approval_level: ApprovalLevel::Required,
            human_only_decisions: vec![HumanDecisionKind::PolicyOverride],
        });
    ApprovalRequirement {
        level: policy.approval_level,
        human_only_decisions: policy.human_only_decisions,
    }
}

fn team_lifecycle(report: &status::TeamStatusJsonReport) -> TeamLifecycle {
    if !report.running {
        TeamLifecycle::Stopped
    } else if report.watchdog.current_backoff_secs.is_some()
        || report.watchdog.state.eq_ignore_ascii_case("recovering")
        || report.watchdog.state.eq_ignore_ascii_case("backoff")
    {
        TeamLifecycle::Recovering
    } else if !report.health.unhealthy_members.is_empty() {
        TeamLifecycle::Degraded
    } else {
        TeamLifecycle::Running
    }
}

fn member_status_from_row(row: &status::TeamStatusRow) -> MemberStatus {
    MemberStatus {
        name: row.name.clone(),
        role: row.role.clone(),
        role_type: row.role_type.clone(),
        state: member_state(&row.state),
        health: member_health(row),
        active_task_ids: row.active_owned_tasks.iter().map(u32::to_string).collect(),
        review_task_ids: row.review_owned_tasks.iter().map(u32::to_string).collect(),
        pending_inbox_count: row.pending_inbox,
        triage_backlog_count: row.triage_backlog,
        signal: row.signal.clone(),
        restart_count: row.health.restart_count,
        context_exhaustion_count: row.health.context_exhaustion_count,
        delivery_failure_count: row.health.delivery_failure_count,
        supervisory_digest_count: row.health.supervisory_digest_count,
        stall_reason: row.health.stall_reason.clone(),
        stall_summary: row.health.stall_summary.clone(),
        task_elapsed_secs: row.health.task_elapsed_secs,
        backend_health: backend_health(row.health.backend_health),
    }
}

fn member_state(raw: &str) -> MemberState {
    match raw {
        "starting" => MemberState::Starting,
        "idle" => MemberState::Idle,
        "working" => MemberState::Working,
        "done" => MemberState::Done,
        "crashed" => MemberState::Crashed,
        _ => MemberState::Unknown,
    }
}

fn member_health(row: &status::TeamStatusRow) -> MemberHealth {
    if matches!(
        row.health.backend_health,
        crate::agent::BackendHealth::Unreachable
    ) || row.state == "crashed"
    {
        MemberHealth::Unhealthy
    } else if row.health.has_operator_warning() {
        MemberHealth::Warning
    } else {
        MemberHealth::Healthy
    }
}

fn backend_health(health: crate::agent::BackendHealth) -> BackendHealth {
    match health {
        crate::agent::BackendHealth::Healthy => BackendHealth::Healthy,
        crate::agent::BackendHealth::Degraded => BackendHealth::Degraded,
        crate::agent::BackendHealth::Unreachable => BackendHealth::Unreachable,
        crate::agent::BackendHealth::QuotaExhausted => BackendHealth::Unreachable,
        crate::agent::BackendHealth::AuthRequired => BackendHealth::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> status::TeamStatusJsonReport {
        status::TeamStatusJsonReport {
            team: "fixture-team".to_string(),
            session: "batty-fixture-team".to_string(),
            running: true,
            paused: false,
            main_smoke: None,
            watchdog: status::WatchdogStatus {
                state: "running".to_string(),
                restart_count: 0,
                current_backoff_secs: None,
                last_exit_category: None,
                last_exit_reason: None,
            },
            health: status::TeamStatusHealth {
                session_running: true,
                paused: false,
                member_count: 2,
                active_member_count: 1,
                pending_inbox_count: 1,
                triage_backlog_count: 2,
                unhealthy_members: vec!["eng-1".to_string()],
            },
            workflow_metrics: Some(status::WorkflowMetrics {
                runnable_count: 3,
                implementation_runnable_count: 3,
                blocked_count: 1,
                in_review_count: 1,
                actionable_review_count: 1,
                in_progress_count: 2,
                stale_in_progress_count: 1,
                aged_todo_count: 0,
                stale_review_count: 1,
                idle_with_runnable: vec!["manager".to_string()],
                top_runnable_tasks: vec!["#42 (high) Inbox fix".to_string()],
                blocked_dispatch_reasons: vec![
                    "#99 Waiting: unmet dependency #42 (in-progress)".to_string(),
                ],
                oldest_review_age_secs: Some(120),
                oldest_assignment_age_secs: Some(60),
                auto_merge_count: 1,
                manual_merge_count: 0,
                direct_root_merge_count: 1,
                isolated_integration_merge_count: 0,
                direct_root_failure_count: 0,
                isolated_integration_failure_count: 0,
                auto_merge_rate: Some(0.5),
                rework_count: 1,
                rework_rate: Some(0.25),
                review_nudge_count: 1,
                review_escalation_count: 1,
                avg_review_latency_secs: Some(42.0),
            }),
            active_tasks: vec![status::StatusTaskEntry {
                id: 42,
                title: "Fix contract".to_string(),
                status: "in-progress".to_string(),
                priority: "high".to_string(),
                claimed_by: Some("eng-1".to_string()),
                review_owner: None,
                blocked_on: None,
                branch: None,
                worktree_path: None,
                commit: None,
                branch_mismatch: None,
                next_action: None,
                test_summary: None,
            }],
            review_queue: vec![status::StatusTaskEntry {
                id: 43,
                title: "Review contract".to_string(),
                status: "review".to_string(),
                priority: "medium".to_string(),
                claimed_by: None,
                review_owner: Some("manager".to_string()),
                blocked_on: None,
                branch: None,
                worktree_path: None,
                commit: None,
                branch_mismatch: None,
                next_action: None,
                test_summary: None,
            }],
            optional_subsystems: None,
            engineer_profiles: None,
            members: vec![
                status::TeamStatusRow {
                    name: "eng-1".to_string(),
                    role: "engineer".to_string(),
                    role_type: "engineer".to_string(),
                    agent: Some("codex".to_string()),
                    reports_to: Some("manager".to_string()),
                    state: "working".to_string(),
                    pending_inbox: 1,
                    triage_backlog: 2,
                    active_owned_tasks: vec![42],
                    review_owned_tasks: Vec::new(),
                    signal: Some("nudge sent".to_string()),
                    runtime_label: Some("working".to_string()),
                    worktree_staleness: None,
                    health: status::AgentHealthSummary {
                        restart_count: 1,
                        context_exhaustion_count: 1,
                        proactive_handoff_count: 0,
                        delivery_failure_count: 0,
                        supervisory_digest_count: 2,
                        dispatch_fallback_count: 0,
                        dispatch_fallback_reason: None,
                        stale_active_cleared_count: 0,
                        stale_active_summary: None,
                        task_elapsed_secs: Some(300),
                        stall_reason: Some("supervisory_stalled".to_string()),
                        stall_summary: Some(
                            "eng-1 stayed in Working for 5m (timeout=60s)".to_string(),
                        ),
                        backend_health: crate::agent::BackendHealth::Degraded,
                    },
                    health_summary: "warning".to_string(),
                    eta: "soon".to_string(),
                },
                status::TeamStatusRow {
                    name: "manager".to_string(),
                    role: "manager".to_string(),
                    role_type: "manager".to_string(),
                    agent: Some("codex".to_string()),
                    reports_to: None,
                    state: "idle".to_string(),
                    pending_inbox: 0,
                    triage_backlog: 0,
                    active_owned_tasks: Vec::new(),
                    review_owned_tasks: vec![43],
                    signal: None,
                    runtime_label: Some("idle".to_string()),
                    worktree_staleness: None,
                    health: status::AgentHealthSummary {
                        restart_count: 0,
                        context_exhaustion_count: 0,
                        proactive_handoff_count: 0,
                        delivery_failure_count: 0,
                        supervisory_digest_count: 0,
                        dispatch_fallback_count: 0,
                        dispatch_fallback_reason: None,
                        stale_active_cleared_count: 0,
                        stale_active_summary: None,
                        task_elapsed_secs: None,
                        stall_reason: None,
                        stall_summary: None,
                        backend_health: crate::agent::BackendHealth::Healthy,
                    },
                    health_summary: "healthy".to_string(),
                    eta: "idle".to_string(),
                },
            ],
        }
    }

    #[test]
    fn descriptor_exposes_versioning_and_surfaces() {
        let descriptor = descriptor();

        assert_eq!(descriptor.kind, CONTRACT_DESCRIPTOR_KIND);
        assert_eq!(descriptor.schema_version, CONTRACT_SCHEMA_VERSION);
        assert!(
            descriptor
                .anti_corruption_boundary
                .batty_is_system_of_record
        );
        assert!(
            descriptor
                .approval_surface
                .human_only_decisions
                .contains(&HumanDecisionKind::MergeDisposition)
        );
        assert!(
            descriptor
                .supported_event_kinds
                .contains(&TeamEventKind::TaskEscalated)
        );
    }

    #[test]
    fn team_status_from_report_normalizes_internal_status() {
        let status = team_status_from_report(&sample_report());

        assert_eq!(status.kind, TEAM_STATUS_KIND);
        assert_eq!(status.lifecycle, TeamLifecycle::Degraded);
        assert_eq!(status.pipeline.active_task_count, 1);
        assert_eq!(status.pipeline.review_queue_count, 1);
        assert_eq!(status.pipeline.triage_backlog_count, 2);
        assert_eq!(status.members[0].state, MemberState::Working);
        assert_eq!(status.members[0].health, MemberHealth::Warning);
        assert_eq!(status.members[0].backend_health, BackendHealth::Degraded);
        assert_eq!(status.members[0].supervisory_digest_count, 2);
        assert_eq!(
            status.members[0].stall_reason.as_deref(),
            Some("supervisory_stalled")
        );
        assert_eq!(
            status.members[0].stall_summary.as_deref(),
            Some("eng-1 stayed in Working for 5m (timeout=60s)")
        );
        assert_eq!(status.members[1].review_task_ids, vec!["43".to_string()]);
    }

    #[test]
    fn member_health_warns_on_supervisory_stall_only() {
        let row = status::TeamStatusRow {
            name: "eng-1".to_string(),
            role: "engineer".to_string(),
            role_type: "engineer".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("manager".to_string()),
            state: "working".to_string(),
            pending_inbox: 0,
            triage_backlog: 0,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: Some("working".to_string()),
            worktree_staleness: None,
            health: status::AgentHealthSummary {
                stall_reason: Some("supervisory_stalled".to_string()),
                stall_summary: Some("eng-1 stayed in Working for 5m".to_string()),
                ..status::AgentHealthSummary::default()
            },
            health_summary: "stall:supervisory_stalled".to_string(),
            eta: "-".to_string(),
        };

        assert_eq!(member_health(&row), MemberHealth::Warning);
    }

    #[test]
    fn project_event_from_internal_uses_explicit_event_kind() {
        let project = RegisteredProject {
            project_id: "fixture".to_string(),
            name: "Fixture".to_string(),
            aliases: Vec::new(),
            project_root: std::path::PathBuf::from("/tmp/fixture"),
            board_dir: std::path::PathBuf::from("/tmp/fixture/.batty/team_config/board"),
            team_name: "fixture-team".to_string(),
            session_name: "batty-fixture-team".to_string(),
            channel_bindings: Vec::new(),
            owner: None,
            tags: vec!["openclaw".to_string()],
            policy_flags: crate::project_registry::ProjectPolicyFlags {
                allow_openclaw_supervision: true,
                allow_cross_project_routing: false,
                allow_shared_service_routing: false,
                archived: false,
            },
            created_at: 0,
            updated_at: 0,
        };
        let event = events::TeamEvent::task_escalated("eng-1", "42", Some("tests_failed"));

        let envelope = project_event_from_internal(&project, &event).unwrap();

        assert_eq!(envelope.kind, TEAM_EVENT_KIND);
        assert_eq!(envelope.event.topic, TeamEventTopic::Escalation);
        assert_eq!(envelope.event.event_kind, TeamEventKind::TaskEscalated);
        assert_eq!(envelope.event.member_name.as_deref(), Some("eng-1"));
        assert_eq!(envelope.event.task_id.as_deref(), Some("42"));
    }

    #[test]
    fn capability_negotiation_rejects_incompatible_schema_versions() {
        let result = negotiate_capabilities(&CapabilityNegotiationRequest {
            kind: CAPABILITY_NEGOTIATION_KIND.to_string(),
            requested_schema_version: 0,
            min_compatible_schema_version: 0,
            requested_capabilities: vec![OpenClawCapability::TeamStatus],
        });

        assert!(!result.compatible);
        assert!(result.negotiated_schema_version.is_none());
    }

    #[test]
    fn team_command_serialization_keeps_action_explicit() {
        let command = TeamCommand::merge("fixture", "openclaw", "42", MergeStrategy::Manual);
        let json = serde_json::to_value(&command).unwrap();

        assert_eq!(json["kind"], TEAM_COMMAND_KIND);
        assert_eq!(json["action"], "merge");
        assert_eq!(json["taskId"], "42");
        assert_eq!(json["approval"]["level"], "required");
    }
}
