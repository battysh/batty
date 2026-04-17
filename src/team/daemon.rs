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
use std::process::Command;
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
    preserve_worktree_with_commit_for, setup_engineer_worktree,
};
use super::verification::VerificationState;
use super::watcher::{SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent::{self, BackendHealth};
use crate::tmux;
use dispatch::DispatchQueueEntry;

const STALLED_MID_TURN_MARKER: &str = "stalled mid-turn";
const STALLED_MID_TURN_RETRY_BACKOFF_SECS: [u64; 2] = [30, 60];
/// Upper bound on total stall attempts (backoffs + restarts) before the
/// task is escalated to blocked. Defense against unbounded restart loops
/// that burn context tokens without making progress. Observed pre-cap
/// 2026-04-17 on batty-marketing: alex-dev-1-1 was on attempt=6 for
/// task #546 after 37 minutes of restart cycling.
const STALLED_MID_TURN_MAX_ATTEMPTS: u32 = 5;

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
pub(crate) mod health;
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
#[cfg(any(test, feature = "scenario-test"))]
#[path = "daemon/scenario_api.rs"]
pub mod scenario_api;
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
    PersistedDaemonState, PersistedNudgeState, PersistedRescueRecord, daemon_state_path,
    load_daemon_state, save_daemon_state,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MainSmokeState {
    pub broken: bool,
    pub pause_dispatch: bool,
    pub last_run_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken_commit: Option<String>,
    #[serde(default)]
    pub suspects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// #686: per-task record used to derive the orphan-rescue dispatch
/// cooldown. `count` grows each time a task is rescued while still in
/// its current cascade-observation window; the effective quiet-time
/// becomes `orphan_rescue_cooldown_secs * 2^min(count-1, 4)` (i.e. 1×,
/// 2×, 4×, 8×, 16× — capped at 16× to avoid a pinned-forever task).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RescueRecord {
    pub last_rescued_at: Instant,
    pub count: u32,
}

impl RescueRecord {
    pub(crate) fn effective_cooldown(&self, base: Duration) -> Duration {
        let multiplier_shift = self.count.saturating_sub(1).min(4);
        base.saturating_mul(1u32 << multiplier_shift)
    }

    /// #689: cooldown gates dispatch; because the dispatch gate keeps the
    /// task off-queue for the whole `effective_cooldown`, any subsequent
    /// rescue will by definition fire *after* that window — so growing
    /// `count` only when `elapsed < effective_cooldown` (old `is_active`)
    /// never triggered in production. The "same cascade" check now uses a
    /// wider window: two full effective cooldowns since the last rescue.
    /// That keeps the count climbing when the rescue→dispatch→reject
    /// loop repeats near-immediately after the cooldown expires, and
    /// resets the counter only when the task actually held stable for a
    /// full extra cooldown worth of time.
    pub(crate) fn cascade_window(&self, base: Duration) -> Duration {
        self.effective_cooldown(base).saturating_mul(2)
    }

    /// True while the dispatch gate must keep this task off the queue.
    pub(crate) fn dispatch_blocked(&self, base: Duration) -> bool {
        self.last_rescued_at.elapsed() < self.effective_cooldown(base)
    }

    /// True while we still consider this task to be part of an active
    /// cascade — used to (a) decide whether the next rescue grows the
    /// counter or resets it, and (b) retain the record in memory.
    pub(crate) fn in_cascade_window(&self, base: Duration) -> bool {
        self.last_rescued_at.elapsed() < self.cascade_window(base)
    }
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
    pub(super) last_main_smoke_check: Instant,
    pub(super) pipeline_starvation_fired: bool,
    pub(super) pipeline_starvation_last_fired: Option<Instant>,
    pub(super) planning_cycle_last_fired: Option<Instant>,
    pub(super) planning_cycle_active: bool,
    /// #681: Consecutive planning cycles that produced zero new tasks.
    /// Used to exponentially back off the architect planning cadence so
    /// that a stuck board (everything blocked, no dispatchable work) does
    /// not burn orchestrator tokens with a ping storm of empty cycles.
    pub(super) planning_cycle_consecutive_empty: u32,
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
    /// #684 / #686: tasks the auto/runtime orphan rescue just moved back
    /// to todo. Dispatch skips these for an exponentially-growing cooldown
    /// window derived from `orphan_rescue_cooldown_secs` × 2^(count-1)
    /// (capped at 16×) so repeated rescues of the same task widen the
    /// quiet period instead of re-cascading every base window.
    pub(super) recently_rescued_tasks: HashMap<u32, RescueRecord>,
    /// #697: tasks recently released by a specific engineer. Populated
    /// when state reconciliation detects an engineer cleared their claim
    /// (reason `task no longer claimed by this engineer`). Dispatch
    /// excludes that engineer from re-dispatch of the same task for
    /// `dispatch_release_exclusion_secs` so they do not immediately
    /// receive back a task they just parked. Other engineers remain
    /// eligible once the task-level rescue cooldown expires.
    pub(super) recently_released_by: HashMap<(u32, String), Instant>,
    /// Tracks recent escalation keys to suppress repeated alerts.
    pub(super) recent_escalations: HashMap<String, Instant>,
    /// Latest periodic main smoke-test outcome.
    pub(super) main_smoke_state: Option<MainSmokeState>,
    /// SQLite telemetry database connection (None if open failed).
    pub(super) telemetry_db: Option<rusqlite::Connection>,
    /// Timestamp of the last manual assignment per engineer (for cooldown).
    pub(super) manual_assign_cooldowns: HashMap<String, Instant>,
    /// Per-member agent backend health state.
    pub(super) backend_health: HashMap<String, BackendHealth>,
    /// Per-member quota retry-at deadline (epoch seconds). Populated when a
    /// shim emits `Event::QuotaBlocked` with a retry_at. Consulted by the
    /// backend-health transition, dispatch selection, and aging alerts to
    /// keep an engineer marked as `quota_exhausted` (i.e. parked) until the
    /// reset deadline passes. A successful poll_shim ping is NOT evidence
    /// of quota recovery — only the deadline or operator intervention is.
    pub(super) backend_quota_retry_at: HashMap<String, u64>,
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
    /// Tracks consecutive shim completions whose worktree still has zero diff
    /// against main, so the daemon can break completion loops proactively.
    pub(super) zero_diff_completion_counts: HashMap<u32, u32>,
    /// Messages deferred because the target agent was still starting.
    /// Drained automatically when the agent transitions to ready.
    pub(super) pending_delivery_queue: HashMap<String, Vec<PendingMessage>>,
    /// Per-agent shim handles (only populated when `use_shim` is true).
    pub(super) shim_handles: HashMap<String, agent_handle::AgentHandle>,
    /// #690: When each member most recently transitioned Idle→Working.
    /// Cleared when they transition back to Idle. Consulted by the
    /// zero-output restart check so an agent that JUST became Working
    /// (e.g. an inbox message arrived 8s before the poll tick) is not
    /// killed for lifetime-zero-output on a shim that has been Idle
    /// for the preceding 18 minutes.
    pub(super) working_since: HashMap<String, Instant>,
    /// When the last shim health check (Ping) was sent.
    pub(super) last_shim_health_check: Instant,
    /// Serial daemon-owned merge queue for auto-merge execution.
    pub(super) merge_queue: MergeQueue,
    /// When the last binary freshness check ran (#675). Gated to at most once per hour.
    pub(super) last_binary_freshness_check: Instant,
    /// When the last tiered inbox expiry sweep ran (#658). Gated to at most once per minute.
    pub(super) last_tiered_inbox_sweep: Instant,
}

#[cfg(any(test, feature = "scenario-test"))]
impl TeamDaemon {
    /// Acquire the scenario framework's test-API hooks. Gated by
    /// `#[cfg(any(test, feature = "scenario-test"))]` so it does not
    /// exist in release builds. See
    /// [`scenario_api::ScenarioHooks`](crate::team::daemon::scenario_api::ScenarioHooks)
    /// for the list of supported operations.
    pub fn scenario_hooks(&mut self) -> scenario_api::ScenarioHooks<'_> {
        scenario_api::ScenarioHooks::new(self)
    }
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

    #[allow(dead_code)]
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

        match preserve_worktree_with_commit_for(
            &worktree_dir,
            commit_message,
            Duration::from_secs(policy.graceful_shutdown_timeout_secs),
            "restart-or-shutdown preservation",
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
            last_main_smoke_check: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            planning_cycle_consecutive_empty: 0,
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
            recently_rescued_tasks: HashMap::new(),
            recently_released_by: HashMap::new(),
            recent_escalations: HashMap::new(),
            main_smoke_state: None,
            telemetry_db,
            manual_assign_cooldowns: HashMap::new(),
            backend_health: HashMap::new(),
            backend_quota_retry_at: HashMap::new(),
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
            zero_diff_completion_counts: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
            shim_handles: HashMap::new(),
            working_since: HashMap::new(),
            last_shim_health_check: Instant::now(),
            merge_queue: MergeQueue::default(),
            // Start far enough in the past to trigger an immediate check at startup.
            last_binary_freshness_check: Instant::now() - Duration::from_secs(7200),
            // First sweep runs on the first tick after startup.
            last_tiered_inbox_sweep: Instant::now() - Duration::from_secs(120),
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

    /// Returns true when the member's backend is parked due to a quota block.
    /// A member is parked when either (a) cached health is `QuotaExhausted`
    /// or (b) the tracked quota `retry_at` deadline is still in the future.
    /// Dispatch selection, stall-timer reclaim, and aging-alert escalation
    /// gate on this to avoid churn against an engineer that simply cannot
    /// make progress until the quota window elapses (#674).
    pub(in crate::team) fn member_backend_parked(&self, member_name: &str) -> bool {
        if matches!(
            self.backend_health.get(member_name),
            Some(BackendHealth::QuotaExhausted | BackendHealth::AuthRequired)
        ) {
            return true;
        }
        if let Some(&retry_at) = self.backend_quota_retry_at.get(member_name) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d: std::time::Duration| d.as_secs())
                .unwrap_or(0);
            if retry_at > now {
                return true;
            }
        }
        false
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
        match preserve_worktree_with_commit_for(
            worktree_dir,
            "wip: auto-save before restart [batty]",
            timeout,
            reason,
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

    pub(super) fn dispatch_paused_by_main_smoke(&self) -> bool {
        self.main_smoke_state
            .as_ref()
            .is_some_and(|state| state.broken && state.pause_dispatch)
    }

    pub(super) fn maybe_run_main_smoke(&mut self) -> Result<()> {
        const DEFAULT_MAIN_SMOKE_SUSPECT_COMMITS: usize = 5;

        let policy = self.config.team_config.workflow_policy.main_smoke.clone();
        if !policy.enabled {
            return Ok(());
        }

        let interval = Duration::from_secs(policy.interval_secs);
        if self.last_main_smoke_check.elapsed() < interval {
            return Ok(());
        }
        self.last_main_smoke_check = Instant::now();

        if !self.is_git_repo || self.is_multi_repo {
            return Ok(());
        }

        let command = policy.command.trim();
        if command.is_empty() {
            warn!("main smoke command is empty; skipping");
            return Ok(());
        }

        let head = Self::short_head_commit(self.project_root())?;
        self.record_orchestrator_action(format!("main smoke: running `{command}` at {head}"));

        let test_run =
            crate::team::task_loop::run_tests_in_worktree(self.project_root(), Some(command))
                .with_context(|| {
                    format!(
                        "failed while running main smoke command `{command}` in {}",
                        self.project_root().display()
                    )
                })?;

        if test_run.passed {
            let was_broken = self
                .main_smoke_state
                .as_ref()
                .is_some_and(|state| state.broken);
            self.main_smoke_state = Some(MainSmokeState {
                broken: false,
                pause_dispatch: policy.pause_dispatch_on_failure,
                last_run_at: now_unix(),
                last_success_commit: Some(head.clone()),
                broken_commit: None,
                suspects: Vec::new(),
                summary: Some(format!("`{command}` passed on {head}")),
            });
            if was_broken {
                self.emit_event(TeamEvent::main_smoke_recovered(&head, command));
                self.record_orchestrator_action(format!(
                    "main smoke: recovered on {head}; dispatch gate cleared"
                ));
            }
            return Ok(());
        }

        let suspects =
            Self::recent_main_suspects(self.project_root(), DEFAULT_MAIN_SMOKE_SUSPECT_COMMITS)?;
        let summary = Self::summarize_smoke_output(&test_run.output);
        let should_emit = self.main_smoke_state.as_ref().is_none_or(|state| {
            state.broken_commit.as_deref() != Some(head.as_str())
                || state.summary.as_deref() != Some(summary.as_str())
        });

        let last_success_commit = self
            .main_smoke_state
            .as_ref()
            .and_then(|state| state.last_success_commit.clone());
        self.main_smoke_state = Some(MainSmokeState {
            broken: true,
            pause_dispatch: policy.pause_dispatch_on_failure,
            last_run_at: now_unix(),
            last_success_commit,
            broken_commit: Some(head.clone()),
            suspects: suspects.clone(),
            summary: Some(summary.clone()),
        });

        if should_emit {
            self.emit_event(TeamEvent::main_broken(&head, &suspects, &summary));
        }
        self.record_orchestrator_action(format!(
            "main smoke: BROKEN at {head}; suspects [{}]; {summary}",
            suspects.join(", ")
        ));

        if policy.auto_revert {
            self.maybe_auto_revert_broken_main(&head)?;
        }
        Ok(())
    }

    fn maybe_auto_revert_broken_main(&mut self, broken_commit: &str) -> Result<()> {
        let parent_line = super::git_cmd::run_git(
            self.project_root(),
            &["rev-list", "--parents", "-n", "1", "HEAD"],
        )
        .with_context(|| {
            format!("failed to inspect parents for broken main commit {broken_commit}")
        })?
        .stdout;
        let parent_count = parent_line.split_whitespace().count().saturating_sub(1);
        let revert_args = if parent_count > 1 {
            vec!["revert", "-m", "1", "--no-edit", "HEAD"]
        } else {
            vec!["revert", "--no-edit", "HEAD"]
        };
        let output = Command::new("git")
            .args(&revert_args)
            .current_dir(self.project_root())
            .output()
            .with_context(|| {
                format!("failed to launch auto-revert for broken main commit {broken_commit}")
            })?;
        if output.status.success() {
            let reverted_to = Self::short_head_commit(self.project_root())?;
            info!(
                broken_commit,
                reverted_to, "main smoke auto-reverted most recent main commit"
            );
            self.record_orchestrator_action(format!(
                "main smoke: auto-reverted broken commit {broken_commit}; main is now {reverted_to}"
            ));
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                broken_commit,
                error = %stderr.trim(),
                "main smoke auto-revert failed"
            );
            self.record_orchestrator_action(format!(
                "main smoke: auto-revert failed for {broken_commit} ({})",
                stderr.trim()
            ));
        }
        Ok(())
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

    fn short_head_commit(project_root: &Path) -> Result<String> {
        Ok(
            super::git_cmd::run_git(project_root, &["rev-parse", "--short", "HEAD"])?
                .stdout
                .trim()
                .to_string(),
        )
    }

    fn recent_main_suspects(project_root: &Path, count: usize) -> Result<Vec<String>> {
        let limit = count.max(1).to_string();
        let output = super::git_cmd::run_git(
            project_root,
            &["log", "--format=%h %s", "-n", limit.as_str(), "main"],
        )?;
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn summarize_smoke_output(output: &str) -> String {
        // Cargo emits ANSI color codes when CARGO_TERM_COLOR=always (set by CI),
        // which would bypass the starts_with checks below. Strip them first.
        fn strip_ansi(s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' && chars.peek() == Some(&'[') {
                    chars.next();
                    for esc in chars.by_ref() {
                        if esc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }

        let summary = output
            .lines()
            .map(|line| strip_ansi(line).trim().to_string())
            .find(|line| {
                !line.is_empty()
                    && !line.starts_with("Compiling ")
                    && !line.starts_with("Checking ")
                    && !line.starts_with("Blocking waiting for file lock")
                    && !line.starts_with("Finished ")
                    && !line.starts_with("Running ")
            })
            .unwrap_or_else(|| "main smoke command failed".to_string());
        summary.chars().take(240).collect()
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
    pub(super) fn set_retry_count_for_test(&mut self, engineer: &str, count: u32) {
        self.retry_counts.insert(engineer.to_string(), count);
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
        if attempt > STALLED_MID_TURN_MAX_ATTEMPTS {
            warn!(
                member = member_name,
                attempt,
                max_attempts = STALLED_MID_TURN_MAX_ATTEMPTS,
                "shim stall retries exhausted; blocking task and releasing engineer"
            );
            self.record_orchestrator_action(format!(
                "stall: attempts exhausted for {member_name} (attempt {attempt}/{max}); blocking task",
                max = STALLED_MID_TURN_MAX_ATTEMPTS
            ));

            let task_id = self.active_task_id(member_name);
            if let Some(task_id) = task_id {
                let board_dir = self.board_dir();
                let reason = format!(
                    "stall-retry cap: SDK stalled mid-turn {attempt}x on {member_name}; needs human triage",
                );
                if let Err(error) =
                    super::task_cmd::block_task_with_reason(&board_dir, task_id, &reason)
                {
                    warn!(error = %error, task_id, "failed to block task after stall cap");
                }
                if let Err(error) = super::task_cmd::assign_task_owners(
                    &board_dir,
                    task_id,
                    Some(""),
                    None,
                ) {
                    warn!(error = %error, task_id, "failed to release claim after stall cap");
                }
                if let Some(manager) = self.manager_name(member_name) {
                    let notice = format!(
                        "Task #{task_id} blocked after {attempt} consecutive stall-retries on {member_name}. Claim released. Needs human triage.",
                    );
                    let _ = self.queue_message("daemon", &manager, &notice);
                }
            }

            self.clear_active_task(member_name);
            return Ok(true);
        }
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
            self.zero_diff_completion_counts.remove(&task_id);
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

    pub(super) fn note_zero_diff_completion(&mut self, task_id: u32) -> u32 {
        let count = self.zero_diff_completion_counts.entry(task_id).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn clear_zero_diff_completion(&mut self, task_id: u32) {
        self.zero_diff_completion_counts.remove(&task_id);
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
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, manager_member, write_owned_task_file,
    };

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

    #[test]
    fn stalled_mid_turn_blocks_task_after_max_attempts() {
        // #693: without a cap, each SDK stall restarts the agent and burns
        // context tokens indefinitely. After MAX_ATTEMPTS (5), block the
        // task with a reason, release the claim, and notify the manager.
        let tmp = tempfile::tempdir().unwrap();
        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 77, "sdk-cap", "in-progress", member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", None),
                engineer_member(member_name, Some("manager"), true),
            ])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 77);
        // Simulate that MAX_ATTEMPTS stalls have already occurred.
        daemon.set_retry_count_for_test(member_name, STALLED_MID_TURN_MAX_ATTEMPTS);

        let handled = daemon
            .handle_stalled_mid_turn_completion(
                member_name,
                "stalled mid-turn: no stdout from Claude SDK for 120s while working.",
            )
            .unwrap();

        assert!(handled, "capped stall must still be handled");

        // Task file should be blocked and claim released.
        let task_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("077-sdk-cap.md");
        let task_body = std::fs::read_to_string(&task_path).unwrap();
        assert!(
            task_body.contains("status: blocked"),
            "task must be marked blocked after stall cap; got:\n{task_body}"
        );
        assert!(
            task_body.contains("stall-retry cap"),
            "blocked_reason must mention stall cap; got:\n{task_body}"
        );

        // Manager receives a notice.
        let manager_inbox = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(
            manager_inbox
                .iter()
                .any(|msg| msg.body.contains("blocked after") && msg.body.contains("#77")),
            "manager must be notified of the capped stall"
        );

        // Active task tracking and retry counter are cleared.
        assert_eq!(daemon.active_task_id(member_name), None);
        assert_eq!(daemon.retry_count_for_test(member_name), None);
    }
}
