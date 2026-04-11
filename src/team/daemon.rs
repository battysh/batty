//! Core team daemon: poll loop, lifecycle coordination, and routing.
//!
//! `TeamDaemon` owns the long-running control loop for a Batty team session.
//! It starts and resumes member agents, polls tmux-backed watchers, routes
//! messages across panes, inboxes, and external channels, persists runtime
//! state, and runs periodic automation such as standups and board rotation.
//!
//! Focused subsystems that were extracted from this file stay close to the
//! daemon boundary:
//! - `merge` handles engineer completion, test gating, and merge/escalation
//!   flow once a task is reported done.
//! - `interventions` handles idle nudges and manager/architect intervention
//!   automation without changing the daemon's main control flow.
//!
//! This module remains the integration layer that sequences those subsystems
//! inside each poll iteration.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::SystemTime;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::board;
use super::comms::{self, Channel};
#[cfg(test)]
use super::config::OrchestratorPosition;
use super::config::{RoleType, TeamConfig};
use super::delivery::{FailedDelivery, PendingMessage};
use super::events::EventSink;
use super::events::TeamEvent;
use super::failure_patterns::FailureTracker;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::merge;
use super::standup::{self, MemberState};
use super::status;
use super::task_cmd;
#[cfg(test)]
use super::task_loop::next_unclaimed_task;
use super::task_loop::{
    branch_is_merged_into, current_worktree_branch, engineer_base_branch_name,
    preserve_worktree_with_commit, setup_engineer_worktree,
};
use super::verification::VerificationState;
use super::watcher::{SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent::{self, BackendHealth};
use crate::tmux;
use dispatch::DispatchQueueEntry;

const STALLED_MID_TURN_MARKER: &str = "stalled mid-turn";
const STALLED_MID_TURN_RETRY_BACKOFF_SECS: [u64; 2] = [30, 60];

#[path = "daemon/agent_handle.rs"]
pub(super) mod agent_handle;
#[path = "daemon/automation.rs"]
mod automation;
#[path = "daemon/config_reload.rs"]
mod config_reload;
#[path = "discord_bridge.rs"]
mod discord_bridge;
#[path = "dispatch/mod.rs"]
mod dispatch;
#[path = "daemon/error_handling.rs"]
mod error_handling;
#[path = "daemon/health/mod.rs"]
mod health;
#[path = "daemon/helpers.rs"]
mod helpers;
#[path = "daemon/hot_reload.rs"]
mod hot_reload;
#[path = "daemon/interventions/mod.rs"]
mod interventions;
#[path = "launcher.rs"]
mod launcher;
#[path = "daemon/merge_queue.rs"]
mod merge_queue;
#[path = "daemon/poll.rs"]
mod poll;
#[path = "daemon/reconcile.rs"]
mod reconcile;
#[path = "daemon/shim_spawn.rs"]
mod shim_spawn;
#[path = "daemon/shim_state.rs"]
mod shim_state;
#[path = "daemon/spec_gen.rs"]
mod spec_gen;
#[path = "daemon/state.rs"]
mod state;
#[path = "telegram_bridge.rs"]
mod telegram_bridge;
#[path = "daemon/telemetry.rs"]
pub(crate) mod telemetry;
#[path = "daemon/tick_report.rs"]
pub mod tick_report;
#[path = "daemon/verification.rs"]
pub(crate) mod verification;

pub(crate) use self::discord_bridge::{
    build_shutdown_snapshot, send_discord_shutdown_notice, send_discord_shutdown_summary,
};
#[cfg(test)]
use self::dispatch::normalized_assignment_dir;
pub(crate) use self::error_handling::{optional_subsystem_for_step, optional_subsystem_names};
use self::helpers::{extract_nudge_section, role_prompt_path};
use self::hot_reload::consume_hot_reload_marker;
#[cfg(test)]
use self::hot_reload::{
    BinaryFingerprint, hot_reload_daemon_args, hot_reload_marker_path, write_hot_reload_marker,
};
pub(crate) use self::interventions::NudgeSchedule;
use self::interventions::OwnedTaskInterventionState;
use self::launcher::{
    duplicate_claude_session_ids, load_launch_state, member_session_tracker_config,
};
pub(crate) use self::merge_queue::{MergeQueue, MergeRequest};
pub use self::state::load_dispatch_queue_snapshot;
#[cfg(test)]
use self::state::{
    PersistedDaemonState, PersistedNudgeState, daemon_state_path, load_daemon_state,
    save_daemon_state,
};
pub(super) use super::delivery::MessageDelivery;

/// Daemon configuration derived from TeamConfig.
pub struct DaemonConfig {
    pub project_root: PathBuf,
    pub team_config: TeamConfig,
    pub session: String,
    pub members: Vec<MemberInstance>,
    pub pane_map: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CleanroomBackend {
    SkoolKit,
    Ghidra,
}

impl CleanroomBackend {
    fn detect(input_path: &Path) -> Result<Self> {
        let extension = input_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        match extension.as_deref() {
            Some("z80" | "sna") => Ok(Self::SkoolKit),
            Some("nes" | "gb" | "gbc" | "com" | "exe") => Ok(Self::Ghidra),
            _ => bail!(
                "unsupported clean-room input '{}': expected one of .z80, .sna, .nes, .gb, .gbc, .com, or .exe",
                input_path.display()
            ),
        }
    }
}

/// The running team daemon.
pub struct TeamDaemon {
    pub(super) config: DaemonConfig,
    pub(super) watchers: HashMap<String, SessionWatcher>,
    pub(super) states: HashMap<String, MemberState>,
    pub(super) idle_started_at: HashMap<String, Instant>,
    pub(super) active_tasks: HashMap<String, u32>,
    pub(super) retry_counts: HashMap<String, u32>,
    pub(super) dispatch_queue: Vec<DispatchQueueEntry>,
    pub(super) triage_idle_epochs: HashMap<String, u64>,
    pub(super) triage_interventions: HashMap<String, u64>,
    pub(super) owned_task_interventions: HashMap<String, OwnedTaskInterventionState>,
    pub(super) intervention_cooldowns: HashMap<String, Instant>,
    pub(super) channels: HashMap<String, Box<dyn Channel>>,
    pub(super) nudges: HashMap<String, NudgeSchedule>,
    pub(super) discord_bot: Option<super::discord::DiscordBot>,
    pub(super) discord_event_cursor: usize,
    pub(super) telegram_bot: Option<super::telegram::TelegramBot>,
    pub(super) failure_tracker: FailureTracker,
    pub(super) event_sink: EventSink,
    pub(super) paused_standups: HashSet<String>,
    pub(super) last_standup: HashMap<String, Instant>,
    pub(super) last_board_rotation: Instant,
    pub(super) last_auto_archive: Instant,
    pub(super) last_auto_dispatch: Instant,
    pub(super) pipeline_starvation_fired: bool,
    pub(super) pipeline_starvation_last_fired: Option<Instant>,
    pub(super) planning_cycle_last_fired: Option<Instant>,
    pub(super) planning_cycle_active: bool,
    pub(super) retro_generated: bool,
    pub(super) failed_deliveries: Vec<FailedDelivery>,
    pub(super) review_first_seen: HashMap<u32, u64>,
    pub(super) review_nudge_sent: HashSet<u32>,
    pub(super) poll_cycle_count: u64,
    pub(super) poll_interval: Duration,
    /// Errors recorded during the current `tick()` call. Cleared at the
    /// start of each tick and drained into the returned `TickReport`.
    /// Always populated (cheap: empty `Vec` cost), so non-test builds can
    /// also surface tick-level diagnostics from the future
    /// `batty debug tick` subcommand.
    pub(super) current_tick_errors: Vec<(String, String)>,
    pub(super) is_git_repo: bool,
    /// True when the project root is not a git repo but contains git sub-repos.
    pub(super) is_multi_repo: bool,
    /// Cached list of sub-repo directory names (relative to project root) for multi-repo projects.
    pub(super) sub_repo_names: Vec<String>,
    /// Consecutive error counts per recoverable subsystem name.
    pub(super) subsystem_error_counts: HashMap<String, u32>,
    pub(super) auto_merge_overrides: HashMap<u32, bool>,
    /// Tracks recent (task_id, engineer) dispatch pairs for deduplication.
    pub(super) recent_dispatches: HashMap<(u32, String), Instant>,
    /// Tracks recent escalation keys to suppress repeated alerts.
    pub(super) recent_escalations: HashMap<String, Instant>,
    /// SQLite telemetry database connection (None if open failed).
    pub(super) telemetry_db: Option<rusqlite::Connection>,
    /// Timestamp of the last manual assignment per engineer (for cooldown).
    pub(super) manual_assign_cooldowns: HashMap<String, Instant>,
    /// Per-member agent backend health state.
    pub(super) backend_health: HashMap<String, BackendHealth>,
    /// Rolling capture history used to detect narration loops.
    pub(super) narration_tracker: health::narration::NarrationTracker,
    /// Per-session output tracking used for proactive context-pressure handling.
    pub(super) context_pressure_tracker: health::context::ContextPressureTracker,
    /// When the last periodic health check was run.
    pub(super) last_health_check: Instant,
    /// Rate-limiting: last time each engineer received an uncommitted-work warning.
    pub(super) last_uncommitted_warn: HashMap<String, Instant>,
    /// Last time the daemon checked for stale per-worktree cargo targets to prune.
    pub(super) last_shared_target_cleanup: Instant,
    /// Last time the daemon ran a full disk hygiene pass.
    pub(super) last_disk_hygiene_check: Instant,
    /// Per-engineer completion verification loop state.
    pub(super) verification_states: HashMap<String, VerificationState>,
    /// Tracks consecutive narration-only rejections per task (commits exist
    /// but the branch still has no file diff). After threshold, escalates.
    pub(super) narration_rejection_counts: HashMap<u32, u32>,
    /// Messages deferred because the target agent was still starting.
    /// Drained automatically when the agent transitions to ready.
    pub(super) pending_delivery_queue: HashMap<String, Vec<PendingMessage>>,
    /// Per-agent shim handles (only populated when `use_shim` is true).
    pub(super) shim_handles: HashMap<String, agent_handle::AgentHandle>,
    /// When the last shim health check (Ping) was sent.
    pub(super) last_shim_health_check: Instant,
    /// Serial daemon-owned merge queue for auto-merge execution.
    pub(super) merge_queue: MergeQueue,
}

impl TeamDaemon {
    pub(in crate::team) fn report_preserve_failure(
        &mut self,
        member_name: &str,
        task_id: Option<u32>,
        context: &str,
        detail: &str,
    ) {
        let reason = match task_id {
            Some(task_id) => format!(
                "Task #{task_id} is blocked because Batty could not safely auto-save {member_name}'s dirty worktree before {context}. {detail}"
            ),
            None => format!(
                "Batty could not safely auto-save {member_name}'s dirty worktree before {context}. {detail}"
            ),
        };

        // Deduplicate repeated preserve-failure alerts for the same
        // (member, task, context, detail). Without this, the daemon fires the
        // same alert to engineer + manager on every reconciliation cycle as
        // long as the stale branch condition persists, creating tight
        // acknowledgement loops that flood the inbox without forward progress.
        let detail_digest: u64 = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            detail.hash(&mut hasher);
            hasher.finish()
        };
        let task_key = task_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let dedup_key = format!("preserve:{member_name}:{task_key}:{context}:{detail_digest}");
        if self.suppress_recent_escalation(dedup_key, Duration::from_secs(600)) {
            return;
        }

        if let Some(task_id) = task_id {
            if let Err(error) =
                task_cmd::block_task_with_reason(&self.board_dir(), task_id, &reason)
            {
                warn!(
                    member = member_name,
                    task_id,
                    error = %error,
                    "failed to block task after dirty worktree preservation failure"
                );
            }
        }
        let manager = self.assignment_sender(member_name);
        let _ = self.queue_daemon_message(member_name, &reason);
        let _ = self.queue_daemon_message(&manager, &reason);
        self.record_orchestrator_action(format!("blocked recovery: {reason}"));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn preserve_member_worktree(
        &mut self,
        member_name: &str,
        commit_message: &str,
    ) -> bool {
        let policy = &self.config.team_config.workflow_policy;
        if !policy.auto_commit_on_restart {
            return false;
        }

        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
        else {
            return false;
        };
        if member.role_type != RoleType::Engineer || !member.use_worktrees {
            return false;
        }

        let worktree_dir = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        if !worktree_dir.exists() {
            return false;
        }

        match preserve_worktree_with_commit(
            &worktree_dir,
            commit_message,
            Duration::from_secs(policy.graceful_shutdown_timeout_secs),
        ) {
            Ok(saved) => {
                if saved {
                    info!(
                        member = member_name,
                        worktree = %worktree_dir.display(),
                        "auto-saved worktree before restart/shutdown"
                    );
                }
                saved
            }
            Err(error) => {
                warn!(
                    member = member_name,
                    worktree = %worktree_dir.display(),
                    error = %error,
                    "failed to auto-save worktree before restart/shutdown"
                );
                self.report_preserve_failure(
                    member_name,
                    self.active_task_id(member_name),
                    "restart or shutdown",
                    &error.to_string(),
                );
                false
            }
        }
    }

    #[allow(dead_code)]
    pub(super) fn watcher_mut(&mut self, name: &str) -> Result<&mut SessionWatcher> {
        self.watchers
            .get_mut(name)
            .with_context(|| format!("watcher registry missing member '{name}'"))
    }

    /// Create a new daemon from resolved config and layout.
    pub fn new(config: DaemonConfig) -> Result<Self> {
        let is_git_repo = super::git_cmd::is_git_repo(&config.project_root);
        let (is_multi_repo, sub_repo_names) = if is_git_repo {
            (false, Vec::new())
        } else {
            let subs = super::git_cmd::discover_sub_repos(&config.project_root);
            if subs.is_empty() {
                (false, Vec::new())
            } else {
                let names: Vec<String> = subs
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                info!(
                    sub_repos = ?names,
                    "Detected multi-repo project with {} sub-repos",
                    names.len()
                );
                (true, names)
            }
        };
        if !is_git_repo && !is_multi_repo {
            info!("Project is not a git repository \u{2014} git operations disabled");
        }

        let team_config_dir = config.project_root.join(".batty").join("team_config");
        let events_path = team_config_dir.join("events.jsonl");
        let event_sink =
            EventSink::new_with_max_bytes(&events_path, config.team_config.event_log_max_bytes)?;

        // Create watchers for each pane member
        let mut watchers = HashMap::new();
        let stale_secs = config.team_config.standup.interval_secs * 2;
        for (name, pane_id) in &config.pane_map {
            let session_tracker = config
                .members
                .iter()
                .find(|member| member.name == *name)
                .and_then(|member| member_session_tracker_config(&config.project_root, member));
            watchers.insert(
                name.clone(),
                SessionWatcher::new(pane_id, name, stale_secs, session_tracker),
            );
        }

        // Create channels for user roles
        let mut channels: HashMap<String, Box<dyn Channel>> = HashMap::new();
        for role in &config.team_config.roles {
            if role.role_type == RoleType::User {
                if let (Some(ch_type), Some(ch_config)) = (&role.channel, &role.channel_config) {
                    match comms::channel_from_config(ch_type, ch_config) {
                        Ok(ch) => {
                            channels.insert(role.name.clone(), ch);
                        }
                        Err(e) => {
                            warn!(role = %role.name, error = %e, "failed to create channel");
                        }
                    }
                }
            }
        }

        // Create Discord bot for inbound polling and event mirroring (if configured)
        let discord_bot = discord_bridge::build_discord_bot(&config.team_config);
        // Create Telegram bot for inbound polling (if configured)
        let telegram_bot = telegram_bridge::build_telegram_bot(&config.team_config);
        let narration_detection_enabled = config
            .team_config
            .workflow_policy
            .narration_detection_enabled;
        let narration_threshold_polls =
            config.team_config.workflow_policy.narration_threshold_polls;

        let states = HashMap::new();

        // Build nudge schedules from role configs + prompt files
        let mut nudges = HashMap::new();
        for role in &config.team_config.roles {
            if let Some(interval_secs) = role.nudge_interval_secs {
                let prompt_path =
                    role_prompt_path(&team_config_dir, role.prompt.as_deref(), role.role_type);
                if let Some(nudge_text) = extract_nudge_section(&prompt_path) {
                    // Apply nudge to all instances of this role
                    let instance_names: Vec<String> = config
                        .members
                        .iter()
                        .filter(|m| m.role_name == role.name)
                        .map(|m| m.name.clone())
                        .collect();
                    for name in instance_names {
                        info!(member = %name, interval_secs, "registered nudge");
                        nudges.insert(
                            name,
                            NudgeSchedule {
                                text: nudge_text.clone(),
                                interval: Duration::from_secs(interval_secs),
                                // All roles start idle, so begin the countdown
                                idle_since: Some(Instant::now()),
                                fired_this_idle: false,
                                paused: false,
                            },
                        );
                    }
                }
            }
        }

        // Open telemetry database (best-effort — log and continue if it fails).
        let telemetry_db = match super::telemetry_db::open(&config.project_root) {
            Ok(conn) => {
                info!("telemetry database opened");
                Some(conn)
            }
            Err(error) => {
                warn!(error = %error, "failed to open telemetry database; telemetry disabled");
                None
            }
        };

        let context_pressure_threshold = config
            .team_config
            .workflow_policy
            .context_pressure_threshold;
        let context_pressure_threshold_bytes = config
            .team_config
            .workflow_policy
            .context_pressure_threshold_bytes;

        Ok(Self {
            config,
            watchers,
            states,
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels,
            nudges,
            discord_bot,
            // Skip backlog on startup — only send events that happen after boot.
            // Reading the entire history on first start hammers Discord API with
            // hundreds of events and triggers rate limits (429).
            discord_event_cursor: crate::team::events::read_events(event_sink.path())
                .map(|events| events.len())
                .unwrap_or(0),
            telegram_bot,
            failure_tracker: FailureTracker::new(20),
            event_sink,
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_cycle_count: 0,
            current_tick_errors: Vec::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo,
            is_multi_repo,
            sub_repo_names,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            recent_escalations: HashMap::new(),
            telemetry_db,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            narration_tracker: health::narration::NarrationTracker::new(
                narration_detection_enabled,
                narration_threshold_polls,
            ),
            context_pressure_tracker: health::context::ContextPressureTracker::new(
                context_pressure_threshold,
                context_pressure_threshold_bytes,
            ),
            // Start far enough in the past to trigger an immediate check.
            last_health_check: Instant::now() - Duration::from_secs(3600),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: Instant::now() - Duration::from_secs(3600),
            last_disk_hygiene_check: Instant::now() - Duration::from_secs(3600),
            verification_states: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
            shim_handles: HashMap::new(),
            last_shim_health_check: Instant::now(),
            merge_queue: MergeQueue::default(),
        })
    }

    pub(crate) fn suppress_recent_escalation(
        &mut self,
        key: impl Into<String>,
        window: Duration,
    ) -> bool {
        let now = Instant::now();
        self.recent_escalations
            .retain(|_, seen_at| now.duration_since(*seen_at) < window);

        let key = key.into();
        if self.recent_escalations.contains_key(&key) {
            return true;
        }

        self.recent_escalations.insert(key, now);
        false
    }

    pub(super) fn member_nudge_text(&self, member: &MemberInstance) -> Option<String> {
        let prompt_path = role_prompt_path(
            &super::team_config_dir(&self.config.project_root),
            member.prompt.as_deref(),
            member.role_type,
        );
        extract_nudge_section(&prompt_path)
    }

    pub(super) fn prepend_member_nudge(
        &self,
        member: &MemberInstance,
        body: impl AsRef<str>,
    ) -> String {
        let body = body.as_ref();
        match self.member_nudge_text(member) {
            Some(nudge) => format!("{nudge}\n\n{body}"),
            None => body.to_string(),
        }
    }

    pub(super) fn mark_member_working(&mut self, member_name: &str) {
        // When shim mode is active, the shim is the single source of truth for
        // agent state. Speculative mark_member_working calls from delivery,
        // completion, and interventions must not override the shim's verdict —
        // doing so causes the daemon to permanently think the agent is "working"
        // when the shim classifier sees it as idle.
        if self.shim_handles.contains_key(member_name) {
            return;
        }
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.activate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Working);
    }

    pub(super) fn set_member_idle(&mut self, member_name: &str) {
        // For shim agents: don't override the state (shim is source of truth),
        // but DO update automation timers so idle_started_at gets populated.
        // Without this, interventions like review_backlog never fire because
        // automation_idle_grace_elapsed returns false.
        if self.shim_handles.contains_key(member_name) {
            if self.states.get(member_name) == Some(&MemberState::Idle) {
                self.update_automation_timers_for_state(member_name, MemberState::Idle);
            }
            return;
        }
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Idle);
    }

    pub(super) fn active_task_id(&self, engineer: &str) -> Option<u32> {
        self.active_tasks.get(engineer).copied()
    }

    pub(super) fn preserve_worktree_before_restart(
        &mut self,
        member_name: &str,
        worktree_dir: &Path,
        reason: &str,
    ) {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
        else {
            return;
        };
        if member.role_type != RoleType::Engineer || !member.use_worktrees {
            return;
        }
        if !self
            .config
            .team_config
            .workflow_policy
            .auto_commit_on_restart
            || !worktree_dir.exists()
        {
            return;
        }

        let timeout = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .graceful_shutdown_timeout_secs,
        );
        match preserve_worktree_with_commit(
            worktree_dir,
            "wip: auto-save before restart [batty]",
            timeout,
        ) {
            Ok(true) => info!(
                member = member_name,
                worktree = %worktree_dir.display(),
                reason,
                "auto-saved dirty worktree before restart"
            ),
            Ok(false) => {}
            Err(error) => {
                warn!(
                    member = member_name,
                    worktree = %worktree_dir.display(),
                    reason,
                    error = %error,
                    "failed to auto-save dirty worktree before restart"
                );
                self.report_preserve_failure(
                    member_name,
                    self.active_task_id(member_name),
                    reason,
                    &error.to_string(),
                );
            }
        }
    }

    pub(super) fn project_root(&self) -> &Path {
        &self.config.project_root
    }

    #[cfg(test)]
    pub(super) fn set_auto_merge_override(&mut self, task_id: u32, enabled: bool) {
        self.auto_merge_overrides.insert(task_id, enabled);
    }

    pub(super) fn auto_merge_override(&self, task_id: u32) -> Option<bool> {
        // In-memory overrides take priority, then check disk
        if let Some(&value) = self.auto_merge_overrides.get(&task_id) {
            return Some(value);
        }
        let disk_overrides = super::auto_merge::load_overrides(&self.config.project_root);
        disk_overrides.get(&task_id).copied()
    }

    pub(super) fn worktree_dir(&self, engineer: &str) -> PathBuf {
        let base = self.config.project_root.join(".batty").join("worktrees");
        match self.member_barrier_group(engineer) {
            Some(group) if self.config.team_config.workflow_policy.clean_room_mode => {
                base.join(group).join(engineer)
            }
            _ => base.join(engineer),
        }
    }

    pub(super) fn board_dir(&self) -> PathBuf {
        self.config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board")
    }

    pub(super) fn member_uses_worktrees(&self, engineer: &str) -> bool {
        if !self.is_git_repo && !self.is_multi_repo {
            return false;
        }
        self.config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .map(|member| member.use_worktrees)
            .unwrap_or(false)
    }

    pub(super) fn handoff_dir(&self) -> PathBuf {
        self.config.project_root.join(
            self.config
                .team_config
                .workflow_policy
                .handoff_directory
                .as_str(),
        )
    }

    pub(super) fn member_barrier_group(&self, member_name: &str) -> Option<&str> {
        let member = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)?;
        self.config
            .team_config
            .role_barrier_group(&member.role_name)
    }

    fn barrier_worktree_root(&self, barrier_group: &str) -> PathBuf {
        self.config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(barrier_group)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn analysis_dir(&self, member_name: &str) -> Result<PathBuf> {
        let Some(group) = self.member_barrier_group(member_name) else {
            bail!(
                "member '{}' is not assigned to a clean-room barrier group",
                member_name
            );
        };
        if group != "analysis" {
            bail!(
                "member '{}' is in barrier group '{}' and cannot write analysis artifacts",
                member_name,
                group
            );
        }

        Ok(self.worktree_dir(member_name).join("analysis"))
    }

    pub(super) fn validate_member_work_dir(
        &self,
        member_name: &str,
        work_dir: &Path,
    ) -> Result<()> {
        if !self.config.team_config.workflow_policy.clean_room_mode {
            return Ok(());
        }

        let expected = self.worktree_dir(member_name);
        if work_dir == expected {
            return Ok(());
        }
        bail!(
            "clean-room barrier violation: member '{}' launch dir '{}' does not match '{}'",
            member_name,
            work_dir.display(),
            expected.display()
        );
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn validate_member_barrier_path(
        &mut self,
        member_name: &str,
        path: &Path,
        access: &str,
    ) -> Result<()> {
        if !self.config.team_config.workflow_policy.clean_room_mode {
            return Ok(());
        }

        let Some(group) = self.member_barrier_group(member_name) else {
            return Ok(());
        };
        let member_root = self.worktree_dir(member_name);
        let barrier_root = self.barrier_worktree_root(group);
        let handoff_root = self.handoff_dir();
        if path.starts_with(&member_root)
            || path.starts_with(&barrier_root)
            || path.starts_with(&handoff_root)
        {
            return Ok(());
        }

        self.record_barrier_violation_attempt(
            member_name,
            &path.display().to_string(),
            &format!("{access} outside barrier group '{group}'"),
        );
        bail!(
            "clean-room barrier violation: '{}' cannot {} '{}'",
            member_name,
            access,
            path.display()
        );
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn write_handoff_artifact(
        &mut self,
        author_role: &str,
        relative_path: &Path,
        content: &[u8],
    ) -> Result<PathBuf> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            self.record_barrier_violation_attempt(
                author_role,
                &relative_path.display().to_string(),
                "handoff writes must stay within the shared handoff directory",
            );
            bail!(
                "invalid handoff artifact path '{}': must be relative and stay under handoff/",
                relative_path.display()
            );
        }
        let handoff_root = self.handoff_dir();
        let artifact_path = handoff_root.join(relative_path);
        let Some(parent) = artifact_path.parent() else {
            bail!(
                "handoff artifact path '{}' has no parent",
                artifact_path.display()
            );
        };
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        std::fs::write(&artifact_path, content)
            .with_context(|| format!("failed to write {}", artifact_path.display()))?;

        let content_hash = format!("{:x}", Sha256::digest(content));
        self.record_barrier_artifact_created(
            author_role,
            &artifact_path.display().to_string(),
            &content_hash,
        );
        Ok(artifact_path)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn write_analysis_artifact(
        &mut self,
        author_role: &str,
        relative_path: &Path,
        content: &[u8],
    ) -> Result<PathBuf> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            self.record_barrier_violation_attempt(
                author_role,
                &relative_path.display().to_string(),
                "analysis artifact writes must stay within the analysis worktree",
            );
            bail!(
                "invalid analysis artifact path '{}': must be relative and stay under analysis/",
                relative_path.display()
            );
        }

        let artifact_path = self.analysis_dir(author_role)?.join(relative_path);
        let Some(parent) = artifact_path.parent() else {
            bail!(
                "analysis artifact path '{}' has no parent",
                artifact_path.display()
            );
        };
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        std::fs::write(&artifact_path, content)
            .with_context(|| format!("failed to write {}", artifact_path.display()))?;

        let content_hash = format!("{:x}", Sha256::digest(content));
        self.record_barrier_artifact_created(
            author_role,
            &artifact_path.display().to_string(),
            &content_hash,
        );
        Ok(artifact_path)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn run_skoolkit_disassembly(
        &mut self,
        author_role: &str,
        snapshot_path: &Path,
        output_relative_path: &Path,
    ) -> Result<PathBuf> {
        let snapshot_extension = snapshot_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        if !matches!(snapshot_extension.as_deref(), Some("z80" | "sna")) {
            bail!(
                "unsupported SkoolKit snapshot '{}': expected .z80 or .sna",
                snapshot_path.display()
            );
        }

        let sna2skool =
            std::env::var("BATTY_SKOOLKIT_SNA2SKOOL").unwrap_or_else(|_| "sna2skool".to_string());
        let output = std::process::Command::new(&sna2skool)
            .arg(snapshot_path)
            .output()
            .with_context(|| {
                format!(
                    "failed to launch '{}' for snapshot '{}'",
                    sna2skool,
                    snapshot_path.display()
                )
            })?;
        if !output.status.success() {
            bail!(
                "SkoolKit disassembly failed for '{}': {}",
                snapshot_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        self.write_analysis_artifact(author_role, output_relative_path, &output.stdout)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn run_ghidra_disassembly(
        &mut self,
        author_role: &str,
        binary_path: &Path,
        output_relative_path: &Path,
    ) -> Result<PathBuf> {
        let backend = CleanroomBackend::detect(binary_path)?;
        if backend != CleanroomBackend::Ghidra {
            bail!(
                "unsupported Ghidra target '{}': expected .nes, .gb, .gbc, .com, or .exe",
                binary_path.display()
            );
        }

        let analyze_headless = std::env::var("BATTY_GHIDRA_HEADLESS")
            .unwrap_or_else(|_| "analyzeHeadless".to_string());
        let output = std::process::Command::new(&analyze_headless)
            .arg(binary_path)
            .output()
            .with_context(|| {
                format!(
                    "failed to launch '{}' for target '{}'",
                    analyze_headless,
                    binary_path.display()
                )
            })?;
        if !output.status.success() {
            bail!(
                "Ghidra disassembly failed for '{}': {}",
                binary_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        self.write_analysis_artifact(author_role, output_relative_path, &output.stdout)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn run_cleanroom_disassembly(
        &mut self,
        author_role: &str,
        input_path: &Path,
        output_relative_path: &Path,
    ) -> Result<PathBuf> {
        match CleanroomBackend::detect(input_path)? {
            CleanroomBackend::SkoolKit => {
                self.run_skoolkit_disassembly(author_role, input_path, output_relative_path)
            }
            CleanroomBackend::Ghidra => {
                self.run_ghidra_disassembly(author_role, input_path, output_relative_path)
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn read_handoff_artifact(
        &mut self,
        reader_role: &str,
        relative_path: &Path,
    ) -> Result<Vec<u8>> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            self.record_barrier_violation_attempt(
                reader_role,
                &relative_path.display().to_string(),
                "handoff reads must stay within the shared handoff directory",
            );
            bail!(
                "invalid handoff artifact path '{}': must be relative and stay under handoff/",
                relative_path.display()
            );
        }
        let artifact_path = self.handoff_dir().join(relative_path);
        self.validate_member_barrier_path(reader_role, &artifact_path, "read")?;
        let content = std::fs::read(&artifact_path)
            .with_context(|| format!("failed to read {}", artifact_path.display()))?;
        let content_hash = format!("{:x}", Sha256::digest(&content));
        self.record_barrier_artifact_read(
            reader_role,
            &artifact_path.display().to_string(),
            &content_hash,
        );
        Ok(content)
    }

    pub(super) fn manager_name(&self, engineer: &str) -> Option<String> {
        self.config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .and_then(|member| member.reports_to.clone())
    }

    pub(super) fn architect_names(&self) -> Vec<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn set_active_task_for_test(&mut self, engineer: &str, task_id: u32) {
        self.active_tasks.insert(engineer.to_string(), task_id);
    }

    #[cfg(test)]
    pub(super) fn retry_count_for_test(&self, engineer: &str) -> Option<u32> {
        self.retry_counts.get(engineer).copied()
    }

    #[cfg(test)]
    pub(super) fn member_state_for_test(&self, engineer: &str) -> Option<MemberState> {
        self.states.get(engineer).copied()
    }

    #[cfg(test)]
    pub(super) fn set_member_state_for_test(&mut self, engineer: &str, state: MemberState) {
        self.states.insert(engineer.to_string(), state);
    }

    #[cfg(test)]
    pub(crate) fn queued_merge_count_for_test(&self) -> usize {
        self.merge_queue.queued_len()
    }

    #[cfg(test)]
    pub(crate) fn process_merge_queue_for_test(&mut self) -> Result<()> {
        self.process_merge_queue()
    }

    pub(super) fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn response_is_stalled_mid_turn(&self, response: &str) -> bool {
        response
            .lines()
            .next()
            .is_some_and(|line| line.contains(STALLED_MID_TURN_MARKER))
    }

    pub(super) fn handle_stalled_mid_turn_completion(
        &mut self,
        member_name: &str,
        response: &str,
    ) -> Result<bool> {
        if !self.response_is_stalled_mid_turn(response) {
            return Ok(false);
        }

        let attempt = self.increment_retry(member_name);
        if let Some(backoff_secs) = stalled_mid_turn_backoff_secs(attempt) {
            warn!(
                member = member_name,
                attempt,
                backoff_secs,
                "shim reported stalled mid-turn completion; retrying after backoff"
            );
            self.record_orchestrator_action(format!(
                "stall: shim reported stalled mid-turn for {member_name}; retry {attempt} after {backoff_secs}s"
            ));
            sleep_stalled_mid_turn_backoff(Duration::from_secs(backoff_secs));

            let retry_notice = format!(
                "Claude SDK stalled mid-turn and Batty released the stuck turn. Waited {backoff_secs}s before retrying.\n{response}\n\nContinue from the current worktree state. Do not restart or discard prior work unless the task requires it."
            );
            self.queue_message("daemon", member_name, &retry_notice)?;
            self.mark_member_working(member_name);
            return Ok(true);
        }

        warn!(
            member = member_name,
            attempt, "shim reported stalled mid-turn completion; restarting agent"
        );
        self.record_orchestrator_action(format!(
            "stall: shim reported stalled mid-turn for {member_name}; restarting on attempt {attempt}"
        ));
        self.restart_member_with_task_context(member_name, "stalled mid-turn")?;
        if let Some(task_id) = self.active_task_id(member_name) {
            self.record_agent_restarted(
                member_name,
                task_id.to_string(),
                "stalled_mid_turn",
                attempt,
            );
        }
        Ok(true)
    }

    pub(super) fn clear_active_task(&mut self, engineer: &str) {
        if let Some(task_id) = self.active_tasks.remove(engineer) {
            self.narration_rejection_counts.remove(&task_id);
        }
        self.retry_counts.remove(engineer);
        self.verification_states.remove(engineer);
        // Clean up any progress checkpoint left from a prior restart.
        super::checkpoint::remove_checkpoint(&self.config.project_root, engineer);
        let work_dir = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .map(|member| self.member_work_dir(member))
            .unwrap_or_else(|| self.config.project_root.clone());
        super::checkpoint::remove_restart_context(&work_dir);
    }

    /// Remove active_task entries for tasks that are done, archived, or no longer on the board.
    pub(super) fn notify_reports_to(&mut self, from_role: &str, msg: &str) -> Result<()> {
        let parent = self
            .config
            .members
            .iter()
            .find(|m| m.name == from_role)
            .and_then(|m| m.reports_to.clone());
        let Some(parent_name) = parent else {
            return Ok(());
        };
        self.queue_message(from_role, &parent_name, msg)?;
        self.mark_member_working(&parent_name);
        Ok(())
    }

    pub(super) fn notify_architects(&mut self, msg: &str) -> Result<()> {
        for architect in self.architect_names() {
            self.queue_message("daemon", &architect, msg)?;
            self.mark_member_working(&architect);
        }
        Ok(())
    }

    /// Update automation countdowns when a member's state changes.
    pub(super) fn update_automation_timers_for_state(
        &mut self,
        member_name: &str,
        new_state: MemberState,
    ) {
        match new_state {
            MemberState::Idle => {
                self.idle_started_at
                    .insert(member_name.to_string(), Instant::now());
            }
            MemberState::Working => {
                self.idle_started_at.remove(member_name);
            }
        }
        self.update_nudge_for_state(member_name, new_state);
        standup::update_timer_for_state(
            &self.config.team_config,
            &self.config.members,
            &mut self.paused_standups,
            &mut self.last_standup,
            member_name,
            new_state,
        );
        self.update_triage_intervention_for_state(member_name, new_state);
    }
}

fn stalled_mid_turn_backoff_secs(attempt: u32) -> Option<u64> {
    STALLED_MID_TURN_RETRY_BACKOFF_SECS
        .get(attempt.saturating_sub(1) as usize)
        .copied()
}

#[cfg(not(test))]
fn sleep_stalled_mid_turn_backoff(duration: Duration) {
    std::thread::sleep(duration);
}

#[cfg(test)]
fn sleep_stalled_mid_turn_backoff(_duration: Duration) {}

#[cfg(test)]
#[path = "daemon/tests.rs"]
mod tests;

#[cfg(test)]
mod stalled_mid_turn_tests {
    use super::*;
    use crate::team::inbox;
    use crate::team::test_support::{TestDaemonBuilder, engineer_member, write_owned_task_file};

    #[test]
    fn stalled_mid_turn_backoff_schedule_matches_task_requirements() {
        assert_eq!(stalled_mid_turn_backoff_secs(1), Some(30));
        assert_eq!(stalled_mid_turn_backoff_secs(2), Some(60));
        assert_eq!(stalled_mid_turn_backoff_secs(3), None);
    }

    #[test]
    fn stalled_mid_turn_detection_matches_marker_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        assert!(daemon.response_is_stalled_mid_turn(
            "stalled mid-turn: no stdout from Claude SDK for 120s while working."
        ));
        assert!(!daemon.response_is_stalled_mid_turn("normal completion"));
    }

    #[test]
    fn stalled_mid_turn_first_retry_requeues_message_after_backoff() {
        let tmp = tempfile::tempdir().unwrap();
        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 42, "sdk-stall", "in-progress", member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(member_name, Some("manager"), true)])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        let handled = daemon
            .handle_stalled_mid_turn_completion(
                member_name,
                "stalled mid-turn: no stdout from Claude SDK for 120s while working.\nlast_sent_message_from: manager",
            )
            .unwrap();

        assert!(handled);
        assert_eq!(daemon.retry_count_for_test(member_name), Some(1));
        assert_eq!(
            daemon.member_state_for_test(member_name),
            Some(MemberState::Working)
        );

        let inbox_entries = inbox::pending_messages(&inbox_root, member_name).unwrap();
        assert_eq!(inbox_entries.len(), 1);
        assert!(inbox_entries[0].body.contains("Waited 30s before retrying"));
        assert!(inbox_entries[0].body.contains("stalled mid-turn"));
    }
}
