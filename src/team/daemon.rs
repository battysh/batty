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
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::board;
use super::board_cmd;
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
    engineer_base_branch_name, is_worktree_safe_to_mutate, setup_engineer_worktree,
};
use super::watcher::{SessionTrackerConfig, SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent::{self, BackendHealth};
use crate::tmux;
use dispatch::DispatchQueueEntry;

#[path = "daemon/automation.rs"]
mod automation;
#[path = "dispatch/mod.rs"]
mod dispatch;
#[path = "daemon/error_handling.rs"]
mod error_handling;
#[path = "daemon/health.rs"]
mod health;
#[path = "daemon/interventions/mod.rs"]
mod interventions;
#[path = "launcher.rs"]
mod launcher;
#[path = "telegram_bridge.rs"]
mod telegram_bridge;
#[path = "daemon/telemetry.rs"]
mod telemetry;

#[cfg(test)]
use self::dispatch::normalized_assignment_dir;
pub(crate) use self::interventions::NudgeSchedule;
use self::interventions::OwnedTaskInterventionState;
use self::launcher::{
    duplicate_claude_session_ids, load_launch_state, member_session_tracker_config,
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

const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

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
    /// When the last periodic health check was run.
    pub(super) last_health_check: Instant,
    /// Rate-limiting: last time each engineer received an uncommitted-work warning.
    pub(super) last_uncommitted_warn: HashMap<String, Instant>,
    /// Messages deferred because the target agent was still starting.
    /// Drained automatically when the agent transitions to ready.
    pub(super) pending_delivery_queue: HashMap<String, Vec<PendingMessage>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberWorktreeContext {
    path: PathBuf,
    branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedNudgeState {
    idle_elapsed_secs: Option<u64>,
    fired_this_idle: bool,
    paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedDaemonState {
    clean_shutdown: bool,
    saved_at: u64,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
    retry_counts: HashMap<String, u32>,
    #[serde(default)]
    dispatch_queue: Vec<DispatchQueueEntry>,
    paused_standups: HashSet<String>,
    last_standup_elapsed_secs: HashMap<String, u64>,
    nudge_state: HashMap<String, PersistedNudgeState>,
    pipeline_starvation_fired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryFingerprint {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl BinaryFingerprint {
    fn capture(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            modified,
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }

    fn changed_from(&self, previous: &Self) -> bool {
        self.modified != previous.modified || self.len != previous.len || {
            #[cfg(unix)]
            {
                self.inode != previous.inode
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
struct HotReloadMonitor {
    binary: BinaryFingerprint,
    last_checked: Instant,
    last_reload_attempt: Option<Instant>,
}

impl HotReloadMonitor {
    fn new(binary: BinaryFingerprint) -> Self {
        Self {
            binary,
            last_checked: Instant::now(),
            last_reload_attempt: None,
        }
    }

    fn for_current_exe() -> Result<Self> {
        let path = std::env::current_exe().context("failed to resolve current executable")?;
        Ok(Self::new(BinaryFingerprint::capture(&path)?))
    }

    fn should_check(&self) -> bool {
        self.last_checked.elapsed() >= HOT_RELOAD_CHECK_INTERVAL
    }

    fn changed_binary(&mut self) -> Result<Option<BinaryFingerprint>> {
        self.last_checked = Instant::now();
        let current = BinaryFingerprint::capture(&self.binary.path)?;
        Ok(current.changed_from(&self.binary).then_some(current))
    }

    fn can_attempt_reload(&self) -> bool {
        self.last_reload_attempt
            .map(|instant| instant.elapsed() >= HOT_RELOAD_MIN_INTERVAL)
            .unwrap_or(true)
    }

    fn mark_reload_attempt(&mut self) {
        self.last_reload_attempt = Some(Instant::now());
    }
}

impl TeamDaemon {
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
            // Start far enough in the past to trigger an immediate check.
            last_health_check: Instant::now() - Duration::from_secs(3600),
            last_uncommitted_warn: HashMap::new(),
            pending_delivery_queue: HashMap::new(),
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

    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    ///
    /// If `resume` is true, agents are launched with session-resume flags
    /// (`claude --resume <session-id>` / `codex resume --last`) instead of fresh starts.
    pub fn run(&mut self, resume: bool) -> Result<()> {
        self.record_daemon_started();
        self.acknowledge_hot_reload_marker();
        info!(session = %self.config.session, resume, "daemon started");
        self.record_orchestrator_action(format!(
            "runtime: orchestrator started (mode={}, resume={resume})",
            self.config.team_config.workflow_mode.as_str()
        ));

        // Install signal handler so we log clean shutdowns
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = shutdown_flag.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        }) {
            warn!(error = %e, "failed to install signal handler");
        }

        self.run_startup_preflight()?;

        // Spawn agents in all panes
        self.spawn_all_agents(resume)?;
        if resume {
            self.restore_runtime_state();
        }
        self.persist_runtime_state(false)?;

        let started_at = Instant::now();
        let heartbeat_interval = Duration::from_secs(300); // 5 minutes
        let mut last_heartbeat = Instant::now();
        let mut hot_reload = match HotReloadMonitor::for_current_exe() {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                warn!(error = %error, "failed to initialize daemon hot-reload monitor");
                None
            }
        };

        // Main polling loop
        let shutdown_reason;
        loop {
            // Check for signal-based shutdown
            if shutdown_flag.load(std::sync::atomic::Ordering::SeqCst) {
                shutdown_reason = "signal";
                info!("received shutdown signal");
                break;
            }

            if !tmux::session_exists(&self.config.session) {
                shutdown_reason = "session_gone";
                info!("tmux session gone, shutting down");
                break;
            }

            // -- Recoverable subsystems: log-and-skip with consecutive-failure tracking --
            self.run_recoverable_step("poll_watchers", |daemon| daemon.poll_watchers());
            self.run_recoverable_step("restart_dead_members", |daemon| {
                daemon.restart_dead_members()
            });
            self.run_recoverable_step("sync_launch_state_session_ids", |daemon| {
                daemon.sync_launch_state_session_ids()
            });
            self.run_recoverable_step("drain_legacy_command_queue", |daemon| {
                daemon.drain_legacy_command_queue()
            });

            // -- Critical subsystems: errors logged but no consecutive-failure tracking --
            self.run_loop_step("deliver_inbox_messages", |daemon| {
                daemon.deliver_inbox_messages()
            });
            self.run_loop_step("retry_failed_deliveries", |daemon| {
                daemon.retry_failed_deliveries()
            });

            // -- Recoverable subsystems --
            self.run_recoverable_step("maybe_intervene_triage_backlog", |daemon| {
                daemon.maybe_intervene_triage_backlog()
            });
            self.run_recoverable_step("maybe_intervene_owned_tasks", |daemon| {
                daemon.maybe_intervene_owned_tasks()
            });
            self.run_recoverable_step("maybe_intervene_review_backlog", |daemon| {
                daemon.maybe_intervene_review_backlog()
            });
            self.run_recoverable_step("maybe_escalate_stale_reviews", |daemon| {
                daemon.maybe_escalate_stale_reviews()
            });
            self.run_recoverable_step("maybe_auto_unblock_blocked_tasks", |daemon| {
                daemon.maybe_auto_unblock_blocked_tasks()
            });

            // -- Critical subsystems --
            self.run_loop_step("reconcile_active_tasks", |daemon| {
                daemon.reconcile_active_tasks()
            });
            self.run_loop_step("maybe_auto_dispatch", |daemon| daemon.maybe_auto_dispatch());
            self.run_recoverable_step("maybe_recycle_cron_tasks", |daemon| {
                daemon.maybe_recycle_cron_tasks()
            });

            // -- Recoverable subsystems --
            self.run_recoverable_step("maybe_intervene_manager_dispatch_gap", |daemon| {
                daemon.maybe_intervene_manager_dispatch_gap()
            });
            self.run_recoverable_step("maybe_intervene_architect_utilization", |daemon| {
                daemon.maybe_intervene_architect_utilization()
            });
            self.run_recoverable_step("maybe_intervene_board_replenishment", |daemon| {
                daemon.maybe_intervene_board_replenishment()
            });
            self.run_recoverable_step("maybe_detect_pipeline_starvation", |daemon| {
                daemon.maybe_detect_pipeline_starvation()
            });

            // -- Recoverable with catch_unwind (panic-safe) --
            self.run_recoverable_step_with_catch_unwind("process_telegram_queue", |daemon| {
                daemon.process_telegram_queue()
            });
            self.run_recoverable_step("maybe_fire_nudges", |daemon| daemon.maybe_fire_nudges());
            self.run_recoverable_step("check_backend_health", |daemon| {
                daemon.check_backend_health()
            });
            self.run_recoverable_step("maybe_reconcile_stale_worktrees", |daemon| {
                daemon.maybe_reconcile_stale_worktrees()
            });
            self.run_recoverable_step("check_worktree_staleness", |daemon| {
                daemon.check_worktree_staleness()
            });
            self.run_recoverable_step("maybe_warn_uncommitted_work", |daemon| {
                daemon.maybe_warn_uncommitted_work()
            });
            self.run_recoverable_step_with_catch_unwind("maybe_generate_standup", |daemon| {
                let generated =
                    standup::maybe_generate_standup(standup::StandupGenerationContext {
                        project_root: &daemon.config.project_root,
                        team_config: &daemon.config.team_config,
                        members: &daemon.config.members,
                        watchers: &daemon.watchers,
                        states: &daemon.states,
                        pane_map: &daemon.config.pane_map,
                        telegram_bot: daemon.telegram_bot.as_ref(),
                        paused_standups: &daemon.paused_standups,
                        last_standup: &mut daemon.last_standup,
                        backend_health: &daemon.backend_health,
                    })?;
                for recipient in generated {
                    daemon.record_standup_generated(&recipient);
                }
                Ok(())
            });
            self.run_recoverable_step("maybe_rotate_board", |daemon| daemon.maybe_rotate_board());
            self.run_recoverable_step("maybe_auto_archive", |daemon| daemon.maybe_auto_archive());
            self.run_recoverable_step_with_catch_unwind("maybe_generate_retrospective", |daemon| {
                daemon.maybe_generate_retrospective()
            });
            self.run_recoverable_step("maybe_notify_failure_patterns", |daemon| {
                daemon.maybe_notify_failure_patterns()
            });
            self.run_recoverable_step("maybe_reload_binary", |daemon| {
                daemon.maybe_hot_reload_binary(hot_reload.as_mut())
            });
            status::update_pane_status_labels(status::PaneStatusLabelUpdateContext {
                project_root: &self.config.project_root,
                members: &self.config.members,
                pane_map: &self.config.pane_map,
                states: &self.states,
                nudges: &self.nudges,
                last_standup: &self.last_standup,
                paused_standups: &self.paused_standups,
                standup_interval_for_member: |member_name| {
                    standup::standup_interval_for_member_name(
                        &self.config.team_config,
                        &self.config.members,
                        member_name,
                    )
                },
            });

            // Periodic heartbeat
            if last_heartbeat.elapsed() >= heartbeat_interval {
                let uptime = started_at.elapsed().as_secs();
                self.record_daemon_heartbeat(uptime);
                if let Err(error) = self.persist_runtime_state(false) {
                    warn!(error = %error, "failed to persist daemon checkpoint");
                }
                debug!(uptime_secs = uptime, "daemon heartbeat");
                last_heartbeat = Instant::now();
            }

            std::thread::sleep(self.poll_interval);
        }

        let uptime = started_at.elapsed().as_secs();
        if let Err(error) = self.persist_runtime_state(true) {
            warn!(error = %error, "failed to persist final daemon checkpoint");
        }
        self.record_daemon_stopped(shutdown_reason, uptime);
        Ok(())
    }

    fn maybe_hot_reload_binary(&mut self, monitor: Option<&mut HotReloadMonitor>) -> Result<()> {
        let Some(monitor) = monitor else {
            return Ok(());
        };
        if !monitor.should_check() {
            return Ok(());
        }

        let Some(updated_binary) = monitor.changed_binary()? else {
            return Ok(());
        };

        if !monitor.can_attempt_reload() {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed again but reload attempt is rate-limited"
            );
            return Ok(());
        }

        if !binary_is_reloadable(&updated_binary.path) {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed but is not safe to hot-reload yet"
            );
            return Ok(());
        }

        monitor.mark_reload_attempt();
        self.persist_runtime_state(false)?;
        self.record_daemon_reloading();
        self.record_orchestrator_action(format!(
            "runtime: daemon reloading after binary change ({})",
            updated_binary.path.display()
        ));
        write_hot_reload_marker(&self.config.project_root)?;

        if let Err(error) = exec_reloaded_daemon(&updated_binary.path, &self.config.project_root) {
            let _ = clear_hot_reload_marker(&self.config.project_root);
            warn!(
                path = %updated_binary.path.display(),
                error = %error,
                "failed to exec updated daemon binary; continuing on existing process"
            );
            self.record_orchestrator_action(format!("runtime: daemon reload failed ({error})"));
        }

        Ok(())
    }

    pub(super) fn mark_member_working(&mut self, member_name: &str) {
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.activate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Working);
    }

    pub(super) fn set_member_idle(&mut self, member_name: &str) {
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
        self.config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer)
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
        self.active_tasks.remove(engineer);
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

    fn restore_runtime_state(&mut self) {
        let Some(state) = load_daemon_state(&self.config.project_root) else {
            return;
        };

        self.states = state.states;
        self.idle_started_at = self
            .states
            .iter()
            .filter(|(_, state)| matches!(state, MemberState::Idle))
            .map(|(member, _)| (member.clone(), Instant::now()))
            .collect();
        self.active_tasks = state.active_tasks;
        self.retry_counts = state.retry_counts;
        self.dispatch_queue = state.dispatch_queue;
        self.paused_standups = state.paused_standups;
        self.last_standup = standup::restore_timer_state(state.last_standup_elapsed_secs);

        for (member_name, persisted) in state.nudge_state {
            let Some(schedule) = self.nudges.get_mut(&member_name) else {
                continue;
            };
            schedule.idle_since = persisted.idle_elapsed_secs.map(|elapsed_secs| {
                Instant::now()
                    .checked_sub(Duration::from_secs(elapsed_secs))
                    .unwrap_or_else(Instant::now)
            });
            schedule.fired_this_idle = persisted.fired_this_idle;
            schedule.paused = persisted.paused;
        }
        self.pipeline_starvation_fired = state.pipeline_starvation_fired;
    }

    fn persist_runtime_state(&self, clean_shutdown: bool) -> Result<()> {
        let state = PersistedDaemonState {
            clean_shutdown,
            saved_at: now_unix(),
            states: self.states.clone(),
            active_tasks: self.active_tasks.clone(),
            retry_counts: self.retry_counts.clone(),
            dispatch_queue: self.dispatch_queue.clone(),
            paused_standups: self.paused_standups.clone(),
            last_standup_elapsed_secs: standup::snapshot_timer_state(&self.last_standup),
            nudge_state: self
                .nudges
                .iter()
                .map(|(member, schedule)| {
                    (
                        member.clone(),
                        PersistedNudgeState {
                            idle_elapsed_secs: schedule.idle_since.map(|t| t.elapsed().as_secs()),
                            fired_this_idle: schedule.fired_this_idle,
                            paused: schedule.paused,
                        },
                    )
                })
                .collect(),
            pipeline_starvation_fired: self.pipeline_starvation_fired,
        };
        save_daemon_state(&self.config.project_root, &state)
    }
}

fn describe_command_failure(command: &str, args: &[&str], output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with status {}", output.status)
    };

    format!("`{command} {}` failed: {details}", args.join(" "))
}

fn default_prompt_file_for_role(role_type: RoleType) -> &'static str {
    match role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    }
}

fn role_prompt_path(
    team_config_dir: &Path,
    prompt_override: Option<&str>,
    role_type: RoleType,
) -> PathBuf {
    team_config_dir.join(prompt_override.unwrap_or(default_prompt_file_for_role(role_type)))
}

/// Extract the `## Nudge` section from a prompt .md file.
///
/// Returns the text after `## Nudge` up to the next `## ` heading or EOF.
/// Returns `None` if no `## Nudge` section is found.
fn extract_nudge_section(prompt_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(prompt_path).ok()?;
    let mut in_nudge = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge {
            // Stop at next heading
            if line.starts_with("## ") {
                break;
            }
            lines.push(line);
        }
    }

    if lines.is_empty() {
        return None;
    }

    let text = lines.join("\n").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Strip the `## Nudge` section from prompt text so it's not sent to the agent.
///
/// The nudge content is daemon-only — injected periodically, not part of the
/// initial prompt.
fn format_stuck_duration(stuck_age_secs: u64) -> String {
    if stuck_age_secs >= 3600 {
        let hours = stuck_age_secs / 3600;
        let mins = (stuck_age_secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if stuck_age_secs >= 60 {
        let mins = stuck_age_secs / 60;
        let secs = stuck_age_secs % 60;
        format!("{mins}m {secs}s")
    } else {
        format!("{stuck_age_secs}s")
    }
}

fn ensure_tmux_session_ready(session: &str) -> Result<()> {
    if tmux::session_exists(session) {
        Ok(())
    } else {
        bail!("daemon startup pre-flight failed: tmux session '{session}' is missing")
    }
}

const KANBAN_MD_VERSION: &str = "0.33.0";

fn kanban_md_download_url() -> Option<String> {
    let (os, arch) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin", "arm64"),
        ("macos", "x86_64") => ("darwin", "amd64"),
        ("linux", "x86_64") => ("linux", "amd64"),
        ("linux", "aarch64") => ("linux", "arm64"),
        _ => return None,
    };
    Some(format!(
        "https://github.com/antopolskiy/kanban-md/releases/download/v{v}/kanban-md_{v}_{os}_{arch}.tar.gz",
        v = KANBAN_MD_VERSION,
    ))
}

fn auto_install_kanban_md() -> Result<()> {
    let url = kanban_md_download_url()
        .context("unsupported platform for automatic kanban-md install")?;

    // Install into the same directory as the batty binary, or fall back to ~/.local/bin
    let bin_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| {
            let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
            p.push(".local/bin");
            p
        });
    std::fs::create_dir_all(&bin_dir)?;
    let dest = bin_dir.join("kanban-md");

    info!(%url, dest = %dest.display(), "auto-installing kanban-md");

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -sL '{url}' | tar xz -C '{dir}'",
            dir = bin_dir.display(),
        ))
        .status()
        .context("failed to run curl | tar for kanban-md install")?;

    if !status.success() {
        bail!("kanban-md download failed (exit {status})");
    }
    if !dest.exists() {
        bail!("kanban-md binary not found at {} after extraction", dest.display());
    }
    info!(path = %dest.display(), "kanban-md installed successfully");
    Ok(())
}

fn ensure_kanban_available() -> Result<()> {
    let output = std::process::Command::new("kanban-md")
        .arg("--help")
        .output();

    match output {
        Ok(o) if o.status.success() => return Ok(()),
        _ => {}
    }

    // Not found or failed — try to auto-install
    info!("kanban-md not found, attempting automatic install");
    auto_install_kanban_md().context(
        "kanban-md is required but not installed and automatic install failed.\n\
         Install manually: https://github.com/antopolskiy/kanban-md/releases"
    )?;

    // Verify it works now
    let output = std::process::Command::new("kanban-md")
        .arg("--help")
        .output()
        .context("kanban-md still not found after install — is the install directory in PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("kanban-md installed but `kanban-md --help` failed: {stderr}");
    }
    Ok(())
}

fn board_dir(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("board")
}

fn ensure_board_initialized(project_root: &Path) -> Result<bool> {
    let board_dir = board_dir(project_root);
    if board_dir.join("tasks").is_dir() {
        return Ok(false);
    }

    board_cmd::init(&board_dir).map_err(|error| {
        anyhow::anyhow!(
            "daemon startup pre-flight failed: unable to initialize board at '{}': {error}",
            board_dir.display()
        )
    })?;
    Ok(true)
}

fn hot_reload_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("reload")
}

fn write_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, now_unix().to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn clear_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

fn consume_hot_reload_marker(project_root: &Path) -> bool {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return false;
    }
    clear_hot_reload_marker(project_root).is_ok()
}

fn hot_reload_daemon_args(project_root: &Path) -> Vec<String> {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    vec![
        "-v".to_string(),
        "daemon".to_string(),
        "--project-root".to_string(),
        root,
        "--resume".to_string(),
    ]
}

fn binary_is_reloadable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(status) = std::process::Command::new("codesign")
            .args(["--verify", path.to_string_lossy().as_ref()])
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
    }

    true
}

#[cfg(unix)]
fn exec_reloaded_daemon(executable: &Path, project_root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(executable)
        .args(hot_reload_daemon_args(project_root))
        .exec();
    Err(anyhow::Error::new(error).context(format!("failed to exec {}", executable.display())))
}

#[cfg(not(unix))]
fn exec_reloaded_daemon(_executable: &Path, _project_root: &Path) -> Result<()> {
    bail!("daemon hot reload via exec is only supported on unix")
}

fn daemon_state_path(project_root: &Path) -> PathBuf {
    super::daemon_state_path(project_root)
}

fn load_daemon_state(project_root: &Path) -> Option<PersistedDaemonState> {
    let path = daemon_state_path(project_root);
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };

    match serde_json::from_str(&content) {
        Ok(state) => Some(state),
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse daemon state, ignoring");
            None
        }
    }
}

pub fn load_dispatch_queue_snapshot(project_root: &Path) -> Vec<DispatchQueueEntry> {
    load_daemon_state(project_root)
        .map(|state| state.dispatch_queue)
        .unwrap_or_default()
}

fn save_daemon_state(project_root: &Path, state: &PersistedDaemonState) -> Result<()> {
    let path = daemon_state_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize daemon state")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AutomationConfig;
    use crate::team::config::{BoardConfig, RoleDef, StandupConfig, WorkflowMode, WorkflowPolicy};
    use crate::team::events::EventSink;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, backdate_idle_grace, engineer_member, init_git_repo,
        manager_member, write_board_task_file, write_open_task_file, write_owned_task_file,
    };
    use std::time::UNIX_EPOCH;

    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    fn production_unwrap_expect_count(path: &Path) -> usize {
        let content = std::fs::read_to_string(path).unwrap();
        let test_split = content.split("\n#[cfg(test)]").next().unwrap_or(&content);
        test_split
            .lines()
            .filter(|line| line.contains(".unwrap(") || line.contains(".expect("))
            .count()
    }
    fn setup_fake_codex(
        project_root: &Path,
        log_root: &Path,
        member_name: &str,
    ) -> (PathBuf, PathBuf) {
        let project_slug = project_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
        let _ = std::fs::remove_dir_all(&fake_bin);
        std::fs::create_dir_all(&fake_bin).unwrap();

        let fake_log = log_root.join(format!("{member_name}-fake-codex.log"));
        let fake_codex = fake_bin.join("codex");
        std::fs::write(
            &fake_codex,
            format!(
                "#!/bin/bash\nprintf 'PWD:%s\\nARGS:%s\\n' \"$PWD\" \"$*\" >> '{}'\nsleep 1\n",
                fake_log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        (fake_bin, fake_log)
    }

    fn write_codex_session_meta(cwd: &Path) -> PathBuf {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set for tests"));
        let session_dir = home
            .join(".codex")
            .join("sessions")
            .join("2099")
            .join("12")
            .join("31");
        std::fs::create_dir_all(&session_dir).unwrap();

        let unique = format!(
            "batty-daemon-lifecycle-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let session_file = session_dir.join(unique);
        std::fs::write(
            &session_file,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();
        session_file
    }

    fn append_codex_task_complete(session_file: &Path) {
        let mut handle = OpenOptions::new().append(true).open(session_file).unwrap();
        writeln!(
            handle,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        handle.flush().unwrap();
    }

    fn wait_for_log_contains(log_path: &Path, needle: &str) -> String {
        (0..300)
            .find_map(|_| {
                let content = match std::fs::read_to_string(log_path) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains(needle) {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| panic!("log {} never contained `{needle}`", log_path.display()))
    }

    fn starvation_test_daemon(tmp: &tempfile::TempDir, threshold: Option<usize>) -> TeamDaemon {
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                engineer_member("eng-1", Some("architect"), false),
                engineer_member("eng-2", Some("architect"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                pipeline_starvation_threshold: threshold,
                ..WorkflowPolicy::default()
            })
            .build();
        daemon.states = HashMap::from([
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]);
        daemon
    }

    #[test]
    fn extract_nudge_from_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "# Architect\n\n## Nudge\n\nCheck work.\nUpdate roadmap.\n\n## Other\n\nstuff\n",
        )
        .unwrap();
        let nudge = extract_nudge_section(tmp.path()).unwrap();
        assert!(nudge.contains("Check work."));
        assert!(nudge.contains("Update roadmap."));
        assert!(!nudge.contains("## Other"));
    }

    #[test]
    fn extract_nudge_returns_none_when_absent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# Engineer\n\n## Workflow\n\n- code\n").unwrap();
        assert!(extract_nudge_section(tmp.path()).is_none());
    }

    #[test]
    fn extract_nudge_returns_none_when_malformed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "# Engineer\n\n## Nudge\n\n## Workflow\n\n- code\n",
        )
        .unwrap();
        assert!(extract_nudge_section(tmp.path()).is_none());
    }

    #[test]
    fn daemon_registers_per_role_nudge_intervals_from_prompt_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();
        std::fs::write(
            team_config_dir.join("manager.md"),
            "# Manager\n\n## Nudge\n\nManager follow-up.\n",
        )
        .unwrap();
        std::fs::write(
            team_config_dir.join("engineer.md"),
            "# Engineer\n\n## Nudge\n\nEngineer follow-up.\n",
        )
        .unwrap();

        let daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                workflow_policy: WorkflowPolicy::default(),
                cost: Default::default(),
                grafana: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: vec![
                    RoleDef {
                        name: "manager".to_string(),
                        role_type: RoleType::Manager,
                        agent: Some("claude".to_string()),
                        instances: 1,
                        prompt: None,
                        talks_to: vec![],
                        channel: None,
                        channel_config: None,
                        nudge_interval_secs: Some(120),
                        receives_standup: None,
                        standup_interval_secs: None,
                        owns: Vec::new(),
                        use_worktrees: false,
                    },
                    RoleDef {
                        name: "engineer".to_string(),
                        role_type: RoleType::Engineer,
                        agent: Some("codex".to_string()),
                        instances: 1,
                        prompt: None,
                        talks_to: vec![],
                        channel: None,
                        channel_config: None,
                        nudge_interval_secs: Some(300),
                        receives_standup: None,
                        standup_interval_secs: None,
                        owns: Vec::new(),
                        use_worktrees: false,
                    },
                ],
            },
            session: "test".to_string(),
            members: vec![
                MemberInstance {
                    name: "lead".to_string(),
                    role_name: "manager".to_string(),
                    role_type: RoleType::Manager,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                },
                MemberInstance {
                    name: "eng-1".to_string(),
                    role_name: "engineer".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("codex".to_string()),
                    prompt: None,
                    reports_to: Some("lead".to_string()),
                    use_worktrees: true,
                },
            ],
            pane_map: HashMap::new(),
        })
        .unwrap();

        assert_eq!(
            daemon
                .nudges
                .get("lead")
                .map(|schedule| schedule.text.as_str()),
            Some("Manager follow-up.")
        );
        assert_eq!(
            daemon.nudges.get("lead").map(|schedule| schedule.interval),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            daemon
                .nudges
                .get("eng-1")
                .map(|schedule| schedule.text.as_str()),
            Some("Engineer follow-up.")
        );
        assert_eq!(
            daemon.nudges.get("eng-1").map(|schedule| schedule.interval),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn format_nudge_status_marks_sent_after_fire() {
        let schedule = NudgeSchedule {
            text: "check in".to_string(),
            interval: Duration::from_secs(600),
            idle_since: Some(Instant::now() - Duration::from_secs(601)),
            fired_this_idle: true,
            paused: false,
        };

        assert_eq!(
            status::format_nudge_status(Some(&schedule)),
            " #[fg=magenta]nudge sent#[default]"
        );
    }

    #[test]
    fn daemon_state_round_trip_preserves_runtime_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let state = PersistedDaemonState {
            clean_shutdown: false,
            saved_at: 123,
            states: HashMap::from([("eng-1".to_string(), MemberState::Working)]),
            active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
            retry_counts: HashMap::from([("eng-1".to_string(), 2)]),
            dispatch_queue: vec![DispatchQueueEntry {
                engineer: "eng-1".to_string(),
                task_id: 77,
                task_title: "queued".to_string(),
                queued_at: 999,
                validation_failures: 1,
                last_failure: Some("waiting for stabilization".to_string()),
            }],
            paused_standups: HashSet::from(["manager".to_string()]),
            last_standup_elapsed_secs: HashMap::from([("architect".to_string(), 55)]),
            nudge_state: HashMap::from([(
                "eng-1".to_string(),
                PersistedNudgeState {
                    idle_elapsed_secs: Some(88),
                    fired_this_idle: true,
                    paused: false,
                },
            )]),
            pipeline_starvation_fired: true,
        };

        save_daemon_state(tmp.path(), &state).unwrap();

        let loaded = load_daemon_state(tmp.path()).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn watcher_mut_returns_context_for_unknown_member() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let mut daemon = make_test_daemon(tmp.path(), vec![manager_member("manager", None)]);

        let error = match daemon.watcher_mut("missing") {
            Ok(_) => panic!("expected missing watcher to return an error"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("watcher registry missing member 'missing'")
        );
    }

    #[test]
    fn test_auto_dispatch_filters_idle_engineers_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-2".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-2".to_string(),
                role_name: "eng-2".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
        ];

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
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
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon
            .states
            .insert("architect".to_string(), MemberState::Idle);
        daemon
            .states
            .insert("manager".to_string(), MemberState::Idle);
        daemon.states.insert("eng-1".to_string(), MemberState::Idle);
        daemon
            .states
            .insert("eng-2".to_string(), MemberState::Working);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("001-auto-task.md"),
            "---\nid: 1\ntitle: auto-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        assert_eq!(daemon.idle_engineer_names(), vec!["eng-1".to_string()]);
        let task = next_unclaimed_task(&board_dir).unwrap().unwrap();
        assert_eq!(task.id, 1);
    }

    #[test]
    fn test_maybe_auto_dispatch_respects_rate_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let before = daemon.last_auto_dispatch;
        daemon.maybe_auto_dispatch().unwrap();
        assert_eq!(daemon.last_auto_dispatch, before);
    }

    #[test]
    fn test_maybe_auto_dispatch_skips_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .board(BoardConfig {
                auto_dispatch: false,
                ..BoardConfig::default()
            })
            .build();
        daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);

        let before = daemon.last_auto_dispatch;
        daemon.maybe_auto_dispatch().unwrap();
        assert_eq!(daemon.last_auto_dispatch, before);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn daemon_lifecycle_happy_path_exercises_decomposed_modules() {
        let session = format!("batty-test-daemon-lifecycle-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-daemon-lifecycle");
        write_open_task_file(&repo, 42, "lifecycle-task", "todo");

        let member_name = "eng-lifecycle";
        let (fake_bin, fake_log) = setup_fake_codex(&repo, tmp.path(), member_name);

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
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
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon.spawn_all_agents(false).unwrap();
        let spawn_log = wait_for_log_contains(&fake_log, "PWD:");
        assert!(spawn_log.contains("PWD:"));
        std::thread::sleep(Duration::from_millis(1200));

        let assignment = "Task #42: lifecycle-task\n\nTask description.";
        daemon
            .assign_task_with_task_id(member_name, assignment, Some(42))
            .unwrap();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        assert_eq!(daemon.active_task_id(member_name), Some(42));
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Working));

        let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
        assert!(worktree_dir.exists());
        assert_eq!(
            crate::team::test_support::git_stdout(&worktree_dir, &["branch", "--show-current"]),
            format!("{member_name}/42")
        );

        let codex_cwd = worktree_dir
            .join(".batty")
            .join("codex-context")
            .join(member_name);
        let session_file = write_codex_session_meta(&codex_cwd);

        daemon.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());
        daemon.run_loop_step("sync_launch_state_session_ids", |daemon| {
            daemon.sync_launch_state_session_ids()
        });

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "note.txt"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "finish task"]);
        append_codex_task_complete(&session_file);

        daemon.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());

        assert_eq!(daemon.active_task_id(member_name), None);
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_assigned"
                && event.role.as_deref() == Some(member_name)
                && event
                    .task
                    .as_deref()
                    .is_some_and(|task| task.contains("Task #42: lifecycle-task"))
        }));
        assert!(events.iter().any(|event| {
            event.event == "task_completed" && event.role.as_deref() == Some(member_name)
        }));

        let launch_state = load_launch_state(&repo);
        let identity = launch_state.get(member_name).expect("missing launch state");
        assert_eq!(identity.agent, "codex-cli");
        assert_eq!(
            identity.session_id.as_deref(),
            session_file.file_stem().and_then(|stem| stem.to_str())
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_file(&session_file);
        let _ = std::fs::remove_dir_all(&fake_bin);
    }
    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let session = "batty-test-nudge-live-delivery";
        let mut delivered_live = false;

        // A freshly created tmux pane can occasionally reject the first live
        // injection under heavy suite load. Retry the full setup a few times so
        // this test only fails on a real regression in the live-delivery path.
        for _attempt in 0..5 {
            let _ = crate::tmux::kill_session(session);

            crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
            let pane_id = crate::tmux::pane_id(session).unwrap();
            std::thread::sleep(Duration::from_millis(300));

            let tmp = tempfile::tempdir().unwrap();
            let mut watchers = HashMap::new();
            let mut scientist_watcher = SessionWatcher::new(&pane_id, "scientist", 300, None);
            scientist_watcher.confirm_ready();
            watchers.insert("scientist".to_string(), scientist_watcher);
            let mut daemon = TestDaemonBuilder::new(tmp.path())
                .session(session)
                .members(vec![architect_member("scientist")])
                .pane_map(HashMap::from([("scientist".to_string(), pane_id.clone())]))
                .watchers(watchers)
                .states(HashMap::from([(
                    "scientist".to_string(),
                    MemberState::Idle,
                )]))
                .nudges(HashMap::from([(
                    "scientist".to_string(),
                    NudgeSchedule {
                        text: "Please make progress.".to_string(),
                        interval: Duration::from_secs(1),
                        idle_since: Some(Instant::now() - Duration::from_secs(5)),
                        fired_this_idle: false,
                        paused: false,
                    },
                )]))
                .build();

            backdate_idle_grace(&mut daemon, "scientist");
            daemon.maybe_fire_nudges().unwrap();

            if daemon.states.get("scientist") == Some(&MemberState::Working) {
                let schedule = daemon.nudges.get("scientist").unwrap();
                assert!(schedule.paused);
                assert!(schedule.idle_since.is_none());
                assert!(!schedule.fired_this_idle);
                delivered_live = true;
                crate::tmux::kill_session(session).unwrap();
                break;
            }

            crate::tmux::kill_session(session).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }

        assert!(
            delivered_live,
            "expected at least one successful live nudge delivery"
        );
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let session = format!("batty-test-triage-live-delivery-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        let mut lead_watcher = SessionWatcher::new(&pane_id, "lead", 300, None);
        lead_watcher.confirm_ready();
        watchers.insert("lead".to_string(), lead_watcher);
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .session(&session)
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), pane_id.clone())]))
            .watchers(watchers)
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        if daemon.states.get("lead") == Some(&MemberState::Working) {
            let pane = (0..50)
                .find_map(|_| {
                    let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                    if pane.contains("batty inbox lead") {
                        Some(pane)
                    } else {
                        std::thread::sleep(Duration::from_millis(200));
                        None
                    }
                })
                .unwrap_or_else(|| tmux::capture_pane(&pane_id).unwrap_or_default());
            assert!(pane.contains("batty inbox lead"));
            assert!(pane.contains("batty read lead <ref>"));
            assert!(pane.contains("batty send eng-1"));
            assert!(pane.contains("batty assign eng-1"));
            assert!(pane.contains("batty send architect"));
            assert!(pane.contains("next time you become idle"));
        } else {
            let pending = inbox::pending_messages(&root, "lead").unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].body.contains("batty inbox lead"));
        }

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn maybe_intervene_triage_backlog_queues_when_live_delivery_falls_back_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([(
                "lead".to_string(),
                "%9999999".to_string(),
            )]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Triage backlog detected"));
        assert!(pending[0].body.contains("batty inbox lead"));
        assert!(pending[0].body.contains("batty read lead <ref>"));
        assert!(pending[0].body.contains("batty send eng-1"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(pending[0].body.contains("next time you become idle"));
    }

    #[test]
    fn maybe_intervene_triage_backlog_does_not_fire_on_startup_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.maybe_intervene_triage_backlog().unwrap();

        assert!(!daemon.triage_interventions.contains_key("lead"));
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    }

    #[test]
    fn maybe_intervene_owned_tasks_queues_when_idle_member_owns_unfinished_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(1)
        );
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Task #191"));
        assert!(
            pending[0]
                .body
                .contains("Owned active task backlog detected")
        );
        assert!(pending[0].body.contains("kanban-md list --dir"));
        assert!(pending[0].body.contains("kanban-md show --dir"));
        assert!(pending[0].body.contains("191"));
        assert!(pending[0].body.contains("sed -n '1,220p'"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(pending[0].body.contains("kanban-md move --dir"));
        assert!(pending[0].body.contains("next time you become idle"));
    }

    #[test]
    fn maybe_intervene_owned_tasks_engineer_message_captures_initial_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "lead");
        assert!(
            pending[0]
                .body
                .contains("Owned active task backlog detected")
        );
        assert!(pending[0].body.contains("Task #191"));
        assert!(pending[0].body.contains("batty send lead"));

        let state = daemon.owned_task_interventions.get("eng-1").unwrap();
        assert_eq!(state.idle_epoch, 1);
        assert_eq!(state.signature, "191:in-progress");
        assert!(!state.escalation_sent);
    }

    #[test]
    fn maybe_intervene_owned_tasks_fires_for_persistent_startup_idle_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Task #191"));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(0)
        );
        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    }

    #[test]
    fn maybe_intervene_owned_tasks_waits_for_idle_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        daemon.maybe_intervene_owned_tasks().unwrap();
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        assert_eq!(inbox::pending_messages(&root, "lead").unwrap().len(), 1);
    }

    #[test]
    fn maybe_intervene_owned_tasks_skips_when_pending_inbox_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        let message = inbox::InboxMessage::new_send("architect", "lead", "Check this first.");
        inbox::deliver_to_inbox(&root, &message).unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(
            !daemon.owned_task_interventions.contains_key("lead"),
            "pending inbox should block new interventions"
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_ignores_review_only_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "review-task", "review", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        daemon.maybe_intervene_owned_tasks().unwrap();

        assert!(!daemon.owned_task_interventions.contains_key("lead"));
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    }

    #[test]
    fn maybe_intervene_owned_tasks_dedupes_same_active_signature_across_idle_epochs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        daemon
            .states
            .insert("lead".to_string(), MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.states.insert("lead".to_string(), MemberState::Idle);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(2)
        );
    }

    fn backdate_intervention_cooldown(daemon: &mut TeamDaemon, key: &str) {
        let cooldown = Duration::from_secs(
            daemon
                .config
                .team_config
                .automation
                .intervention_cooldown_secs,
        ) + Duration::from_secs(1);
        daemon
            .intervention_cooldowns
            .insert(key.to_string(), Instant::now() - cooldown);
    }

    #[test]
    fn owned_task_intervention_updates_signature_when_board_state_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        write_owned_task_file(tmp.path(), 191, "first-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let initial = daemon.owned_task_interventions.get("eng-1").unwrap();
        assert_eq!(initial.signature, "191:in-progress");

        for message in inbox::pending_messages(&root, "eng-1").unwrap() {
            inbox::mark_delivered(&root, "eng-1", &message.id).unwrap();
        }

        write_owned_task_file(tmp.path(), 192, "second-task", "in-progress", "eng-1");
        backdate_intervention_cooldown(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #191"));
        assert!(pending[0].body.contains("#192 (in-progress) second-task"));

        let updated = daemon.owned_task_interventions.get("eng-1").unwrap();
        assert_eq!(updated.signature, "191:in-progress|192:in-progress");
        assert!(!updated.escalation_sent);
    }

    #[test]
    fn owned_task_intervention_respects_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "first-task", "in-progress", "lead");

        // First fire: should deliver intervention.
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "first intervention should fire");

        // Acknowledge the message so inbox is clear for next check.
        for msg in pending {
            inbox::mark_delivered(&root, "lead", &msg.id).unwrap();
        }

        // Change signature (add another task) — should still be blocked by cooldown.
        write_owned_task_file(tmp.path(), 192, "second-task", "in-progress", "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 0, "cooldown should prevent refire");

        // Expire the cooldown — should fire again.
        backdate_intervention_cooldown(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "should fire after cooldown expires");
    }

    #[test]
    fn triage_intervention_respects_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([
                ("lead".to_string(), "%999".to_string()),
                ("eng-1".to_string(), "%998".to_string()),
            ]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();
        daemon.triage_idle_epochs = HashMap::from([("lead".to_string(), 1)]);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        // Deliver a message from eng-1 to lead's inbox so triage finds something.
        let msg = inbox::InboxMessage::new_send("eng-1", "lead", "done with task 42");
        let msg_id = inbox::deliver_to_inbox(&root, &msg).unwrap();
        inbox::mark_delivered(&root, "lead", &msg_id).unwrap();

        // First fire: should work.
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "first triage intervention should fire");

        // Acknowledge so inbox is clear.
        for p in pending {
            inbox::mark_delivered(&root, "lead", &p.id).unwrap();
        }

        // Advance epoch (Working → Idle transition).
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");

        // New epoch should normally allow refire, but cooldown blocks it.
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 0, "cooldown should prevent triage refire");

        // Expire cooldown — should fire.
        backdate_intervention_cooldown(&mut daemon, "triage::lead");
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "triage should fire after cooldown expires"
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_escalates_stuck_signature_to_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .workflow_policy(WorkflowPolicy {
                escalation_threshold_secs: 120,
                ..WorkflowPolicy::default()
            })
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
        state.detected_at = Instant::now() - Duration::from_secs(121);

        daemon.maybe_intervene_owned_tasks().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        let lead_pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(lead_pending.len(), 1);
        assert_eq!(lead_pending[0].from, "daemon");
        assert!(lead_pending[0].body.contains("Stuck task escalation"));
        assert!(lead_pending[0].body.contains("eng-1"));
        assert!(lead_pending[0].body.contains("Task #191"));
        assert!(lead_pending[0].body.contains("kanban-md edit --dir"));
        assert!(lead_pending[0].body.contains("batty assign eng-1"));
        assert!(
            daemon
                .owned_task_interventions
                .get("eng-1")
                .is_some_and(|state| state.escalation_sent)
        );

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(
            events.iter().any(|event| {
                event.event == "task_escalated"
                    && event.role.as_deref() == Some("eng-1")
                    && event.task.as_deref() == Some("191")
            }),
            "expected task_escalated event for stuck owned task"
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_only_escalates_stuck_signature_once() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .workflow_policy(WorkflowPolicy {
                escalation_threshold_secs: 120,
                ..WorkflowPolicy::default()
            })
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
        state.detected_at = Instant::now() - Duration::from_secs(121);

        daemon.maybe_intervene_owned_tasks().unwrap();
        daemon.maybe_intervene_owned_tasks().unwrap();

        let lead_pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(lead_pending.len(), 1);
        assert!(lead_pending[0].body.contains("Stuck task escalation"));
        assert!(
            daemon
                .owned_task_interventions
                .get("eng-1")
                .is_some_and(|state| state.escalation_sent)
        );

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event == "task_escalated"
                        && event.role.as_deref() == Some("eng-1")
                        && event.task.as_deref() == Some("191")
                })
                .count(),
            1
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_waits_for_escalation_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .workflow_policy(WorkflowPolicy {
                escalation_threshold_secs: 120,
                ..WorkflowPolicy::default()
            })
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
        state.detected_at = Instant::now() - Duration::from_secs(119);

        daemon.maybe_intervene_owned_tasks().unwrap();

        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(
            daemon
                .owned_task_interventions
                .get("eng-1")
                .is_some_and(|state| !state.escalation_sent)
        );
    }

    #[test]
    fn maybe_intervene_review_backlog_queues_for_idle_manager_with_branch_and_worktree_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-daemon-test");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        write_owned_task_file(&repo, 191, "review-task", "review", "eng-1");

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), true),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&root, "lead").unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_review_backlog().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Review backlog detected"));
        assert!(pending[0].body.contains("#191 by eng-1"));
        assert!(pending[0].body.contains("batty inbox lead"));
        assert!(pending[0].body.contains("batty read lead <ref>"));
        assert!(pending[0].body.contains("batty merge eng-1"));
        assert!(pending[0].body.contains("kanban-md move --dir"));
        assert!(pending[0].body.contains("191 done"));
        assert!(pending[0].body.contains("191 archived"));
        assert!(pending[0].body.contains("191 in-progress"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(
            pending[0]
                .body
                .contains(worktree_dir.to_string_lossy().as_ref())
        );
        assert!(pending[0].body.contains("branch: eng-1"));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("review::lead")
                .map(|state| state.idle_epoch),
            Some(1)
        );
    }

    #[test]
    fn maybe_intervene_review_backlog_does_not_fire_on_startup_idle() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 191, "review-task", "review", "eng-1");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();

        daemon.maybe_intervene_review_backlog().unwrap();

        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(!daemon.owned_task_interventions.contains_key("review::lead"));
    }

    #[test]
    fn maybe_intervene_manager_dispatch_gap_queues_for_idle_lead_with_idle_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
                engineer_member("eng-2", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "eng-2").unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Dispatch recovery needed"));
        assert!(pending[0].body.contains("eng-1 on #191"));
        assert!(pending[0].body.contains("eng-2"));
        assert!(pending[0].body.contains("batty status"));
        assert!(pending[0].body.contains("batty send eng-1"));
        assert!(pending[0].body.contains("batty assign eng-2"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(
            daemon
                .owned_task_interventions
                .contains_key("dispatch::lead")
        );
    }

    #[test]
    fn maybe_intervene_architect_utilization_queues_for_underloaded_idle_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
                engineer_member("eng-2", Some("lead"), false),
            ])
            .pane_map(HashMap::from([(
                "architect".to_string(),
                "%999".to_string(),
            )]))
            .states(HashMap::from([
                ("architect".to_string(), MemberState::Idle),
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: backlog\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        backdate_idle_grace(&mut daemon, "architect");
        daemon.maybe_intervene_architect_utilization().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "daemon");
        assert!(pending[0].body.contains("Utilization recovery needed"));
        assert!(pending[0].body.contains("eng-1 on #191"));
        assert!(pending[0].body.contains("eng-2"));
        assert!(pending[0].body.contains("batty status"));
        assert!(pending[0].body.contains("batty send lead"));
        assert!(pending[0].body.contains("Start Task #192 on eng-2"));
        assert!(
            daemon
                .owned_task_interventions
                .contains_key("utilization::architect")
        );
    }

    #[test]
    fn zero_engineers_topology_skips_executor_interventions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
            ])
            .pane_map(HashMap::from([
                ("architect".to_string(), "%998".to_string()),
                ("lead".to_string(), "%999".to_string()),
            ]))
            .states(HashMap::from([
                ("architect".to_string(), MemberState::Idle),
                ("lead".to_string(), MemberState::Idle),
            ]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_open_task_file(tmp.path(), 191, "queued-task", "todo");

        backdate_idle_grace(&mut daemon, "architect");
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();
        daemon.maybe_intervene_architect_utilization().unwrap();

        assert!(
            inbox::pending_messages(&root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(
            !daemon
                .owned_task_interventions
                .contains_key("dispatch::lead")
        );
        assert!(
            !daemon
                .owned_task_interventions
                .contains_key("utilization::architect")
        );
    }

    #[test]
    fn single_role_topology_nudges_idle_member() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect_member("solo")])
            .pane_map(HashMap::from([("solo".to_string(), "%999".to_string())]))
            .states(HashMap::from([("solo".to_string(), MemberState::Idle)]))
            .nudges(HashMap::from([(
                "solo".to_string(),
                NudgeSchedule {
                    text: "Solo mode should keep moving.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "solo").unwrap();

        backdate_idle_grace(&mut daemon, "solo");
        daemon.maybe_fire_nudges().unwrap();

        let pending = inbox::pending_messages(&root, "solo").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "daemon");
        assert!(pending[0].body.contains("Solo mode should keep moving."));
        assert!(pending[0].body.contains("Idle nudge:"));
        assert_eq!(daemon.states.get("solo"), Some(&MemberState::Idle));
        assert!(
            daemon
                .nudges
                .get("solo")
                .is_some_and(|schedule| schedule.fired_this_idle)
        );
    }

    #[test]
    fn all_members_working_suppresses_interventions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
                engineer_member("eng-2", Some("lead"), false),
            ])
            .pane_map(HashMap::from([
                ("architect".to_string(), "%997".to_string()),
                ("lead".to_string(), "%998".to_string()),
                ("eng-1".to_string(), "%999".to_string()),
                ("eng-2".to_string(), "%996".to_string()),
            ]))
            .states(HashMap::from([
                ("architect".to_string(), MemberState::Working),
                ("lead".to_string(), MemberState::Working),
                ("eng-1".to_string(), MemberState::Working),
                ("eng-2".to_string(), MemberState::Working),
            ]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "eng-2").unwrap();

        let mut triage_message = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        triage_message.timestamp = super::now_unix();
        let triage_id = inbox::deliver_to_inbox(&root, &triage_message).unwrap();
        inbox::mark_delivered(&root, "lead", &triage_id).unwrap();

        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");
        write_owned_task_file(tmp.path(), 192, "review-task", "review", "eng-2");
        write_open_task_file(tmp.path(), 193, "open-task", "todo");

        daemon.maybe_intervene_triage_backlog().unwrap();
        daemon.maybe_intervene_owned_tasks().unwrap();
        daemon.maybe_intervene_review_backlog().unwrap();
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();
        daemon.maybe_intervene_architect_utilization().unwrap();

        assert!(
            inbox::pending_messages(&root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(inbox::pending_messages(&root, "eng-1").unwrap().is_empty());
        assert!(inbox::pending_messages(&root, "eng-2").unwrap().is_empty());
        assert!(daemon.triage_interventions.is_empty());
        assert!(daemon.owned_task_interventions.is_empty());
    }

    #[test]
    fn manager_dispatch_gap_skips_when_pending_inbox_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
                engineer_member("eng-2", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "eng-2").unwrap();
        let message = inbox::InboxMessage::new_send("architect", "lead", "Handle this first.");
        inbox::deliver_to_inbox(&root, &message).unwrap();

        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
        write_open_task_file(tmp.path(), 192, "open-task", "todo");

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(
            !daemon
                .owned_task_interventions
                .contains_key("dispatch::lead")
        );
    }

    #[test]
    fn owned_task_intervention_refires_at_exact_cooldown_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("lead", Some("architect"))])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        let cooldown = Duration::from_secs(
            daemon
                .config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );

        backdate_idle_grace(&mut daemon, "lead");
        daemon.intervention_cooldowns.insert(
            "lead".to_string(),
            Instant::now() - (cooldown - Duration::from_secs(1)),
        );
        daemon.maybe_intervene_owned_tasks().unwrap();
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());

        daemon
            .intervention_cooldowns
            .insert("lead".to_string(), Instant::now() - cooldown);
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .body
                .contains("Owned active task backlog detected")
        );
        assert!(daemon.owned_task_interventions.contains_key("lead"));
    }

    #[test]
    fn empty_board_skips_interventions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([
                ("architect".to_string(), "%997".to_string()),
                ("lead".to_string(), "%998".to_string()),
                ("eng-1".to_string(), "%999".to_string()),
            ]))
            .states(HashMap::from([
                ("architect".to_string(), MemberState::Idle),
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
            ]))
            .build();

        std::fs::create_dir_all(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        backdate_idle_grace(&mut daemon, "architect");
        backdate_idle_grace(&mut daemon, "lead");
        backdate_idle_grace(&mut daemon, "eng-1");

        daemon.maybe_intervene_triage_backlog().unwrap();
        daemon.maybe_intervene_owned_tasks().unwrap();
        daemon.maybe_intervene_review_backlog().unwrap();
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();
        daemon.maybe_intervene_architect_utilization().unwrap();

        assert!(
            inbox::pending_messages(&root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(inbox::pending_messages(&root, "eng-1").unwrap().is_empty());
        assert!(daemon.triage_interventions.is_empty());
        assert!(daemon.owned_task_interventions.is_empty());
    }

    #[test]
    fn test_starvation_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "daemon");
        assert_eq!(
            pending[0].body,
            "Pipeline running dry: 2 idle engineers, 1 todo tasks."
        );
        assert!(daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_debounce() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_threshold_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(2));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(
            inbox::pending_messages(&root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(!daemon.pipeline_starvation_fired);

        let disabled_tmp = tempfile::tempdir().unwrap();
        let mut disabled_daemon = starvation_test_daemon(&disabled_tmp, None);
        let disabled_root = inbox::inboxes_root(disabled_tmp.path());
        inbox::init_inbox(&disabled_root, "architect").unwrap();
        write_open_task_file(disabled_tmp.path(), 101, "queued-task", "todo");

        disabled_daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(
            inbox::pending_messages(&disabled_root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(!disabled_daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_reset_when_work_added() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(daemon.pipeline_starvation_fired);

        // Parity (2 tasks == 2 engineers) does NOT clear — need surplus to reset
        write_open_task_file(tmp.path(), 102, "queued-task-2", "backlog");
        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(daemon.pipeline_starvation_fired);

        // Surplus (3 tasks > 2 engineers) clears the flag
        write_open_task_file(tmp.path(), 103, "queued-task-3", "backlog");
        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(!daemon.pipeline_starvation_fired);

        // Remove surplus — back to 1 task for 2 engineers, starvation re-fires after cooldown
        std::fs::remove_file(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("102-queued-task-2.md"),
        )
        .unwrap();
        std::fs::remove_file(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("103-queued-task-3.md"),
        )
        .unwrap();
        daemon.pipeline_starvation_last_fired = Some(Instant::now() - Duration::from_secs(301));
        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 2);
        assert!(daemon.pipeline_starvation_fired);
    }

    #[test]
    fn starvation_suppressed_when_engineer_has_active_board_item() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();

        // Create one unclaimed todo task and one in-review task claimed by eng-1
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");
        write_board_task_file(
            tmp.path(),
            102,
            "review-task",
            "review",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.maybe_detect_pipeline_starvation().unwrap();

        // eng-1 has an active board item (review), so only eng-2 is truly idle.
        // 1 idle engineer, 1 unclaimed todo task => no deficit => no alert
        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert!(pending.is_empty());
        assert!(!daemon.pipeline_starvation_fired);
    }

    #[test]
    fn starvation_suppressed_when_manager_working() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
                engineer_member("eng-2", Some("lead"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                pipeline_starvation_threshold: Some(1),
                ..WorkflowPolicy::default()
            })
            .build();
        daemon.states = HashMap::from([
            ("lead".to_string(), MemberState::Working),
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]);
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();

        // Manager is working, so starvation alert should be suppressed
        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn maybe_intervene_triage_backlog_does_not_refire_while_prior_intervention_remains_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        daemon
            .states
            .insert("lead".to_string(), MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.states.insert("lead".to_string(), MemberState::Idle);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().all(|message| message.from == "architect"));
    }

    #[test]
    fn maybe_fire_nudges_keeps_member_idle_when_delivery_falls_back_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect_member("scientist")])
            .pane_map(HashMap::from([(
                "scientist".to_string(),
                "%999".to_string(),
            )]))
            .states(HashMap::from([(
                "scientist".to_string(),
                MemberState::Idle,
            )]))
            .nudges(HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]))
            .build();

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
        let schedule = daemon.nudges.get("scientist").unwrap();
        assert!(!schedule.paused);
        assert!(schedule.fired_this_idle);

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "scientist").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("Please make progress."));
        assert!(messages[0].body.contains("Idle nudge:"));
    }

    #[test]
    fn maybe_fire_nudges_skips_when_pending_inbox_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect_member("scientist")])
            .pane_map(HashMap::from([(
                "scientist".to_string(),
                "%999".to_string(),
            )]))
            .states(HashMap::from([(
                "scientist".to_string(),
                MemberState::Idle,
            )]))
            .nudges(HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now()),
                    fired_this_idle: false,
                    paused: false,
                },
            )]))
            .build();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "scientist").unwrap();
        let message =
            inbox::InboxMessage::new_send("architect", "scientist", "Process this first.");
        inbox::deliver_to_inbox(&root, &message).unwrap();

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        let messages = inbox::pending_messages(&root, "scientist").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "architect");
        let schedule = daemon.nudges.get("scientist").unwrap();
        assert!(!schedule.fired_this_idle);
        assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
    }

    #[test]
    fn automation_sender_prefers_direct_manager_and_config_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .build();
        daemon.config.team_config.automation_sender = Some("human".to_string());

        assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
        assert_eq!(daemon.automation_sender_for("lead"), "architect");
        assert_eq!(daemon.automation_sender_for("architect"), "human");

        daemon.config.team_config.automation_sender = None;
        assert_eq!(daemon.automation_sender_for("architect"), "daemon");
    }

    #[test]
    fn hot_reload_marker_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = hot_reload_marker_path(tmp.path());

        write_hot_reload_marker(tmp.path()).unwrap();
        assert!(marker.exists());
        assert!(consume_hot_reload_marker(tmp.path()));
        assert!(!marker.exists());
        assert!(!consume_hot_reload_marker(tmp.path()));
    }

    #[test]
    fn hot_reload_resume_args_include_resume_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let args = hot_reload_daemon_args(tmp.path());
        let canonical_root = tmp.path().canonicalize().unwrap();
        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                "daemon".to_string(),
                "--project-root".to_string(),
                canonical_root.to_string_lossy().to_string(),
                "--resume".to_string(),
            ]
        );
    }

    #[test]
    fn hot_reload_fingerprint_detects_binary_change() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("batty");
        fs::write(&binary, "old-binary").unwrap();
        let before = BinaryFingerprint::capture(&binary).unwrap();

        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&binary, "new-binary-build").unwrap();
        let after = BinaryFingerprint::capture(&binary).unwrap();

        assert!(after.changed_from(&before));
    }

    #[test]
    fn resume_decision_logged_to_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Hybrid,
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
                grafana: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: vec![member],
            pane_map: HashMap::from([("architect".to_string(), "%999".to_string())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("resume: architect=no (resume disabled)"));
    }

    #[test]
    fn reconcile_clears_done_task() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        write_owned_task_file(tmp.path(), 42, "finished-work", "done", "eng-1");
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn reconcile_keeps_in_progress_task() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        write_owned_task_file(tmp.path(), 42, "active-work", "in-progress", "eng-1");
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        daemon.active_tasks.insert("eng-1".to_string(), 42);

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn reconcile_clears_missing_task() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        // No task file for ID 99 — it doesn't exist on the board
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        daemon.active_tasks.insert("eng-1".to_string(), 99);

        daemon.reconcile_active_tasks().unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn production_daemon_has_no_unwrap_or_expect_calls() {
        let count = production_unwrap_expect_count(Path::new(file!()));
        assert_eq!(count, 0, "production daemon.rs should avoid unwrap/expect");
    }

    #[test]
    fn non_git_repo_disables_worktrees() {
        use crate::team::harness::TestHarness;
        use crate::team::test_support::engineer_member;

        let harness = TestHarness::new()
            .with_member(engineer_member("eng-1", Some("manager"), true))
            .with_member_state("eng-1", MemberState::Idle);
        let daemon = harness.build_daemon().unwrap();

        // Test harness temp dir is not a git repo
        assert!(!daemon.is_git_repo);
        // member_uses_worktrees should return false even though the member config has use_worktrees=true
        assert!(
            !daemon.member_uses_worktrees("eng-1"),
            "worktrees should be disabled when project is not a git repo"
        );
    }

    #[test]
    fn git_repo_enables_worktrees() {
        use crate::team::harness::TestHarness;
        use crate::team::test_support::engineer_member;

        let harness = TestHarness::new()
            .with_member(engineer_member("eng-1", Some("manager"), true))
            .with_member_state("eng-1", MemberState::Idle);
        let mut daemon = harness.build_daemon().unwrap();

        // Simulate being in a git repo
        daemon.is_git_repo = true;

        assert!(
            daemon.member_uses_worktrees("eng-1"),
            "worktrees should be enabled when project is a git repo and member has use_worktrees=true"
        );
    }

    // --- Stale review escalation tests ---

    fn write_review_task(project_root: &Path, id: u32, review_owner: &str) {
        write_review_task_with_priority(project_root, id, review_owner, "high");
    }

    fn write_review_task_with_priority(
        project_root: &Path,
        id: u32,
        review_owner: &str,
        priority: &str,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-review-task-{id}.md")),
            format!(
                "---\nid: {id}\ntitle: review-task-{id}\nstatus: review\npriority: {priority}\nclass: standard\nclaimed_by: eng-1\nreview_owner: {review_owner}\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn stale_review_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                review_nudge_threshold_secs: 1800,
                review_timeout_secs: 7200,
                ..WorkflowPolicy::default()
            })
            .build()
    }

    #[test]
    fn stale_review_sends_nudge_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task(tmp.path(), 42, "manager");
        let mut daemon = stale_review_daemon(&tmp);

        // Seed the first_seen time to 1801 seconds ago
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        daemon.review_first_seen.insert(42, now - 1801);

        daemon.maybe_escalate_stale_reviews().unwrap();

        // Nudge should have been sent
        assert!(daemon.review_nudge_sent.contains(&42));

        // Event should be emitted (check event sink wrote something)
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = std::fs::read_to_string(&events_path).unwrap_or_default();
        assert!(events.contains("review_nudge_sent"));
    }

    #[test]
    fn stale_review_escalates_at_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task(tmp.path(), 42, "manager");
        let mut daemon = stale_review_daemon(&tmp);

        // Seed the first_seen time to 7201 seconds ago
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        daemon.review_first_seen.insert(42, now - 7201);

        daemon.maybe_escalate_stale_reviews().unwrap();

        // Task should no longer be tracked (it was escalated)
        assert!(!daemon.review_first_seen.contains_key(&42));
        assert!(!daemon.review_nudge_sent.contains(&42));

        // Task should be transitioned to blocked
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
        let task = tasks.iter().find(|t| t.id == 42).unwrap();
        assert_eq!(task.status, "blocked");
        assert_eq!(
            task.blocked_on.as_deref(),
            Some("review timeout escalated to architect")
        );

        // Event should be emitted
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = std::fs::read_to_string(&events_path).unwrap_or_default();
        assert!(events.contains("review_escalated"));
    }

    #[test]
    fn nudge_only_sent_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task(tmp.path(), 42, "manager");
        let mut daemon = stale_review_daemon(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        daemon.review_first_seen.insert(42, now - 1801);

        // First call: nudge sent
        daemon.maybe_escalate_stale_reviews().unwrap();
        assert!(daemon.review_nudge_sent.contains(&42));

        // Count events
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events_before = std::fs::read_to_string(&events_path)
            .unwrap_or_default()
            .matches("review_nudge_sent")
            .count();

        // Second call: nudge should NOT fire again
        daemon.maybe_escalate_stale_reviews().unwrap();
        let events_after = std::fs::read_to_string(&events_path)
            .unwrap_or_default()
            .matches("review_nudge_sent")
            .count();

        assert_eq!(events_before, events_after, "nudge should not fire twice");
    }

    #[test]
    fn config_nudge_threshold_defaults() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.review_nudge_threshold_secs, 1800);
        assert_eq!(policy.review_timeout_secs, 7200);
    }

    // --- Per-priority review timeout override tests ---

    fn stale_review_daemon_with_overrides(tmp: &tempfile::TempDir) -> TeamDaemon {
        use crate::team::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "critical".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(300),
                review_timeout_secs: Some(600),
            },
        );
        overrides.insert(
            "high".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(900),
                review_timeout_secs: Some(3600),
            },
        );
        TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                review_nudge_threshold_secs: 1800,
                review_timeout_secs: 7200,
                review_timeout_overrides: overrides,
                ..WorkflowPolicy::default()
            })
            .build()
    }

    #[test]
    fn critical_task_nudges_at_priority_override_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
        let mut daemon = stale_review_daemon_with_overrides(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // 301s > critical nudge threshold of 300s
        daemon.review_first_seen.insert(50, now - 301);

        daemon.maybe_escalate_stale_reviews().unwrap();

        assert!(
            daemon.review_nudge_sent.contains(&50),
            "critical task should be nudged at 300s override"
        );
    }

    #[test]
    fn critical_task_not_nudged_below_override_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
        let mut daemon = stale_review_daemon_with_overrides(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // 200s < critical nudge threshold of 300s
        daemon.review_first_seen.insert(50, now - 200);

        daemon.maybe_escalate_stale_reviews().unwrap();

        assert!(
            !daemon.review_nudge_sent.contains(&50),
            "critical task should not be nudged before 300s"
        );
    }

    #[test]
    fn critical_task_escalates_at_priority_override_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
        let mut daemon = stale_review_daemon_with_overrides(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // 601s > critical escalation threshold of 600s
        daemon.review_first_seen.insert(50, now - 601);

        daemon.maybe_escalate_stale_reviews().unwrap();

        // Task escalated — removed from tracking
        assert!(!daemon.review_first_seen.contains_key(&50));

        // Task should be blocked
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
        let task = tasks.iter().find(|t| t.id == 50).unwrap();
        assert_eq!(task.status, "blocked");
    }

    #[test]
    fn medium_task_uses_global_thresholds_when_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task_with_priority(tmp.path(), 51, "manager", "medium");
        let mut daemon = stale_review_daemon_with_overrides(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // 1000s > critical override (300s) but < global nudge (1800s)
        daemon.review_first_seen.insert(51, now - 1000);

        daemon.maybe_escalate_stale_reviews().unwrap();

        assert!(
            !daemon.review_nudge_sent.contains(&51),
            "medium task should use global 1800s threshold, not critical 300s"
        );
    }

    #[test]
    fn mixed_priority_tasks_get_different_thresholds() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task_with_priority(tmp.path(), 60, "manager", "critical");
        write_review_task_with_priority(tmp.path(), 61, "manager", "medium");
        let mut daemon = stale_review_daemon_with_overrides(&tmp);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Both at 400s age: exceeds critical nudge (300s) but not medium nudge (1800s)
        daemon.review_first_seen.insert(60, now - 400);
        daemon.review_first_seen.insert(61, now - 400);

        daemon.maybe_escalate_stale_reviews().unwrap();

        assert!(
            daemon.review_nudge_sent.contains(&60),
            "critical task should be nudged at 400s (threshold 300s)"
        );
        assert!(
            !daemon.review_nudge_sent.contains(&61),
            "medium task should NOT be nudged at 400s (threshold 1800s)"
        );
    }

    // ── Error-path sentinel tests ──────────────────────────────────────

    #[test]
    fn load_daemon_state_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        // No daemon state file exists
        let result = load_daemon_state(tmp.path());
        assert!(result.is_none(), "missing state file should return None");
    }

    #[test]
    fn load_daemon_state_returns_none_for_corrupt_json() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = super::daemon_state_path(tmp.path());
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&state_path, "not valid json {{{").unwrap();

        let result = load_daemon_state(tmp.path());
        assert!(
            result.is_none(),
            "corrupt JSON should return None, not panic"
        );
    }

    #[test]
    fn save_daemon_state_returns_error_on_readonly_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let lock_dir = tmp.path().join(".batty");
            std::fs::create_dir_all(&lock_dir).unwrap();
            // Make directory read-only so write fails
            std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o444)).unwrap();

            let state = PersistedDaemonState {
                clean_shutdown: false,
                saved_at: 0,
                states: HashMap::new(),
                active_tasks: HashMap::new(),
                retry_counts: HashMap::new(),
                dispatch_queue: Vec::new(),
                paused_standups: HashSet::new(),
                last_standup_elapsed_secs: HashMap::new(),
                nudge_state: HashMap::new(),
                pipeline_starvation_fired: false,
            };

            let result = save_daemon_state(tmp.path(), &state);
            // Restore permissions for cleanup
            std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                result.is_err(),
                "writing to read-only directory should return error, not panic"
            );
        }
    }

    #[test]
    fn watcher_mut_missing_member_returns_error_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let mut daemon = make_test_daemon(tmp.path(), vec![manager_member("manager", None)]);

        let result = daemon.watcher_mut("nonexistent-member");
        match result {
            Ok(_) => panic!("expected error for missing member"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("nonexistent-member"),
                    "error should name the missing member, got: {msg}"
                );
            }
        }
    }

    #[test]
    fn extract_nudge_missing_file_returns_none_not_panic() {
        let result = extract_nudge_section(Path::new("/nonexistent/path/prompt.md"));
        assert!(
            result.is_none(),
            "missing prompt file should return None, not panic"
        );
    }

    #[test]
    fn binary_fingerprint_capture_missing_file_returns_error() {
        let result = BinaryFingerprint::capture(Path::new("/nonexistent/binary"));
        assert!(
            result.is_err(),
            "capturing fingerprint of missing file should return error, not panic"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("/nonexistent/binary"),
            "error should include the file path"
        );
    }

    #[test]
    fn load_dispatch_queue_graceful_when_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = load_dispatch_queue_snapshot(tmp.path());
        assert!(
            queue.is_empty(),
            "dispatch queue from missing state should be empty, not panic"
        );
    }
}
