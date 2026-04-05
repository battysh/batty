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
    branch_is_merged_into, checkout_worktree_branch_from_main, current_worktree_branch,
    engineer_base_branch_name, is_worktree_safe_to_mutate, preserve_worktree_with_commit,
    setup_engineer_worktree,
};
use super::watcher::{SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent::{self, BackendHealth};
use crate::tmux;
use dispatch::DispatchQueueEntry;

#[path = "daemon/agent_handle.rs"]
pub(super) mod agent_handle;
#[path = "daemon/automation.rs"]
mod automation;
#[path = "daemon/config_reload.rs"]
mod config_reload;
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
#[path = "daemon/poll.rs"]
mod poll;
#[path = "daemon/reconcile.rs"]
mod reconcile;
#[path = "daemon/shim_spawn.rs"]
mod shim_spawn;
#[path = "daemon/shim_state.rs"]
mod shim_state;
#[path = "daemon/state.rs"]
mod state;
#[path = "telegram_bridge.rs"]
mod telegram_bridge;
#[path = "daemon/telemetry.rs"]
mod telemetry;

#[cfg(test)]
use self::dispatch::normalized_assignment_dir;
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
    pub(super) poll_interval: Duration,
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
    /// Tracks consecutive "no commits ahead of main" rejections per engineer.
    /// Used to detect and auto-recover from branches that never diverged.
    pub(super) completion_rejection_counts: HashMap<String, u32>,
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
}

impl TeamDaemon {
    pub(super) fn preserve_member_worktree(&self, member_name: &str, commit_message: &str) -> bool {
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

        // Create Telegram bot for inbound polling (if configured)
        let telegram_bot = telegram_bridge::build_telegram_bot(&config.team_config);
        let narration_detection_threshold = config
            .team_config
            .workflow_policy
            .narration_detection_threshold;

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
            .context_pressure_threshold_bytes;
        let context_pressure_delay = config
            .team_config
            .workflow_policy
            .context_pressure_restart_delay_secs;

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
            poll_interval: Duration::from_secs(5),
            is_git_repo,
            is_multi_repo,
            sub_repo_names,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            narration_tracker: health::narration::NarrationTracker::new(
                12,
                narration_detection_threshold,
            ),
            context_pressure_tracker: health::context::ContextPressureTracker::new(
                context_pressure_threshold,
                context_pressure_delay,
            ),
            // Start far enough in the past to trigger an immediate check.
            last_health_check: Instant::now() - Duration::from_secs(3600),
            last_uncommitted_warn: HashMap::new(),
            completion_rejection_counts: HashMap::new(),
            narration_rejection_counts: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
            shim_handles: HashMap::new(),
            last_shim_health_check: Instant::now(),
        })
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
        &self,
        member_name: &str,
        worktree_dir: &Path,
        reason: &str,
    ) {
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
        match super::git_cmd::auto_commit_if_dirty(
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
            Err(error) => warn!(
                member = member_name,
                worktree = %worktree_dir.display(),
                reason,
                error = %error,
                "failed to auto-save dirty worktree before restart"
            ),
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
        let handoff_root = self.handoff_dir();
        if path.starts_with(&member_root) || path.starts_with(&handoff_root) {
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

    pub(super) fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn clear_active_task(&mut self, engineer: &str) {
        if let Some(task_id) = self.active_tasks.remove(engineer) {
            self.narration_rejection_counts.remove(&task_id);
        }
        self.retry_counts.remove(engineer);
        // Clean up any progress checkpoint left from a prior restart.
        super::checkpoint::remove_checkpoint(&self.config.project_root, engineer);
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

#[cfg(test)]
#[path = "daemon/tests.rs"]
mod tests;
