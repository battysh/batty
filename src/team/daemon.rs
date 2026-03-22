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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use super::delivery::FailedDelivery;
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
use super::task_loop::{engineer_base_branch_name, setup_engineer_worktree};
use super::watcher::{SessionTrackerConfig, SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent;
use crate::tmux;
use dispatch::DispatchQueueEntry;

#[path = "dispatch.rs"]
mod dispatch;
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

const CONTEXT_RESTART_COOLDOWN: Duration = Duration::from_secs(30);

const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

const STARTUP_PREFLIGHT_RESPAWN_DELAY: Duration = Duration::from_millis(200);

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
    pub(super) last_auto_dispatch: Instant,
    pub(super) pipeline_starvation_fired: bool,
    pub(super) pipeline_starvation_last_fired: Option<Instant>,
    pub(super) retro_generated: bool,
    pub(super) failed_deliveries: Vec<FailedDelivery>,
    pub(super) review_first_seen: HashMap<u32, u64>,
    pub(super) review_nudge_sent: HashSet<u32>,
    pub(super) poll_interval: Duration,
    pub(super) is_git_repo: bool,
    /// Consecutive error counts per recoverable subsystem name.
    pub(super) subsystem_error_counts: HashMap<String, u32>,
    pub(super) auto_merge_overrides: HashMap<u32, bool>,
    /// Tracks recent (task_id, engineer) dispatch pairs for deduplication.
    pub(super) recent_dispatches: HashMap<(u32, String), Instant>,
    /// SQLite telemetry database connection (None if open failed).
    pub(super) telemetry_db: Option<rusqlite::Connection>,
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
    fn watcher_mut(&mut self, name: &str) -> Result<&mut SessionWatcher> {
        self.watchers
            .get_mut(name)
            .with_context(|| format!("watcher registry missing member '{name}'"))
    }

    /// Create a new daemon from resolved config and layout.
    pub fn new(config: DaemonConfig) -> Result<Self> {
        let is_git_repo = super::git_cmd::is_git_repo(&config.project_root);
        if !is_git_repo {
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
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
            poll_interval: Duration::from_secs(5),
            is_git_repo,
            subsystem_error_counts: HashMap::new(),
            auto_merge_overrides: HashMap::new(),
            recent_dispatches: HashMap::new(),
            telemetry_db,
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
                    })?;
                for recipient in generated {
                    daemon.record_standup_generated(&recipient);
                }
                Ok(())
            });
            self.run_recoverable_step("maybe_rotate_board", |daemon| daemon.maybe_rotate_board());
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

    fn run_startup_preflight(&mut self) -> Result<()> {
        ensure_tmux_session_ready(&self.config.session)?;
        self.ensure_member_panes_ready()?;
        ensure_kanban_available()?;
        if ensure_board_initialized(&self.config.project_root)? {
            let board_dir = board_dir(&self.config.project_root);
            info!(
                board = %board_dir.display(),
                "initialized missing board during daemon startup preflight"
            );
            self.record_orchestrator_action(format!(
                "startup: initialized board at {}",
                board_dir.display()
            ));
        }
        self.validate_member_panes_on_startup();
        Ok(())
    }

    fn ensure_member_panes_ready(&mut self) -> Result<()> {
        let members = self.config.members.clone();
        for member in &members {
            if member.role_type == RoleType::User {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member.name) else {
                bail!(
                    "daemon startup pre-flight failed: no tmux pane mapped for member '{}'",
                    member.name
                );
            };
            if !tmux::pane_exists(pane_id) {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' is missing",
                    pane_id,
                    member.name
                );
            }

            if !tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect pane '{pane_id}'"))?
            {
                continue;
            }

            warn!(
                member = %member.name,
                pane = %pane_id,
                "respawning dead pane during daemon startup preflight"
            );
            tmux::respawn_pane(pane_id, "bash")
                .with_context(|| format!("failed to respawn pane '{pane_id}'"))?;
            std::thread::sleep(STARTUP_PREFLIGHT_RESPAWN_DELAY);

            if tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect respawned pane '{pane_id}'"))?
            {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' stayed dead after respawn",
                    pane_id,
                    member.name
                );
            }

            self.record_orchestrator_action(format!(
                "startup: respawned dead pane for {}",
                member.name
            ));
            self.emit_event(TeamEvent::pane_respawned(&member.name));
        }

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
    fn restart_dead_members(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();
        for name in member_names {
            let Some(pane_id) = self.config.pane_map.get(&name) else {
                continue;
            };
            if !tmux::pane_exists(pane_id) {
                continue;
            }
            if !tmux::pane_dead(pane_id).unwrap_or(false) {
                continue;
            }
            self.restart_member(&name)?;
        }
        Ok(())
    }

    fn restart_member(&mut self, member_name: &str) -> Result<()> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        warn!(member = %member_name, pane = %pane_id, "detected dead pane, restarting member");
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let plan = self.prepare_member_launch(
            &member,
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )?;
        self.apply_member_launch(&member, &pane_id, &plan)?;
        if let Err(error) = self.persist_member_launch_identity(&member.name, plan.identity.clone())
        {
            warn!(member = %member.name, error = %error, "failed to persist restarted launch identity");
        }
        self.record_orchestrator_action(format!(
            "restart: respawned pane and relaunched {} after pane death",
            member.name
        ));
        self.emit_event(TeamEvent::pane_respawned(&member.name));
        self.record_member_crashed(&member.name, true);
        Ok(())
    }

    fn handle_context_exhaustion(&mut self, member_name: &str) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            warn!(member = %member_name, "context exhausted but no active task is recorded");
            self.states
                .insert(member_name.to_string(), MemberState::Idle);
            return Ok(());
        };
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };
        let restart_cooldown_key = Self::context_restart_cooldown_key(member_name);
        let restart_on_cooldown = self
            .intervention_cooldowns
            .get(&restart_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        let escalation_cooldown_key = Self::context_escalation_cooldown_key(member_name);
        let escalation_on_cooldown = self
            .intervention_cooldowns
            .get(&escalation_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);

        let prior_restarts = self.context_restart_count(task.id)?;
        if prior_restarts >= 1 {
            if escalation_on_cooldown {
                info!(
                    member = %member_name,
                    task_id = task.id,
                    "context exhaustion escalation suppressed by cooldown"
                );
                return Ok(());
            }
            self.escalate_context_exhaustion(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_cooldown_key, Instant::now());
            return Ok(());
        }

        if restart_on_cooldown {
            info!(
                member = %member_name,
                task_id = task.id,
                "context exhaustion restart suppressed by cooldown"
            );
            return Ok(());
        }

        warn!(
            member = %member_name,
            task_id = task.id,
            "context exhausted; restarting agent with task context"
        );
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = Self::restart_assignment_message(&task);
        let launch =
            self.launch_task_assignment(member_name, &assignment, Some(task.id), false, false)?;
        let mut restart_notice = format!(
            "Restarted after context exhaustion. Continue task #{} from the current worktree state.",
            task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject restart notice");
        }
        self.record_orchestrator_action(format!(
            "restart: relaunched {} on task #{} after context exhaustion",
            member_name, task.id
        ));
        self.intervention_cooldowns
            .insert(restart_cooldown_key, Instant::now());
        self.record_agent_restarted(
            member_name,
            task.id.to_string(),
            "context_exhausted",
            prior_restarts + 1,
        );
        if let Some(branch) = launch.branch.as_deref() {
            info!(member = %member_name, task_id = task.id, branch, "context restart relaunched assignment");
        }
        Ok(())
    }

    fn escalate_context_exhaustion(
        &mut self,
        member: &MemberInstance,
        task: &crate::task::Task,
        restart_count: u32,
    ) -> Result<()> {
        let Some(manager) = member.reports_to.as_deref() else {
            warn!(
                member = %member.name,
                task_id = task.id,
                restart_count,
                "context exhaustion exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} exhausted context {restart_count} times. Batty restarted it once already and will not restart it again automatically.\n\
Task: {title}\n\
Next step: decide whether to split the task, redirect the engineer, or intervene directly in the lane.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "restart: escalated context exhaustion for {} on task #{} after {} exhaustions",
            member.name, task.id, restart_count
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("context_exhausted"));
        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    fn load_prompt(&self, member: &MemberInstance, config_dir: &Path) -> String {
        let prompt_file = member.prompt.as_deref().unwrap_or(match member.role_type {
            RoleType::Architect => "architect.md",
            RoleType::Manager => "manager.md",
            RoleType::Engineer => "engineer.md",
            RoleType::User => "architect.md", // shouldn't happen
        });

        let path = config_dir.join(prompt_file);
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .replace("{{member_name}}", &member.name)
                .replace("{{role_name}}", &member.role_name)
                .replace(
                    "{{reports_to}}",
                    member.reports_to.as_deref().unwrap_or("none"),
                ),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to load prompt template");
                format!(
                    "You are {} (role: {:?}). Work on assigned tasks.",
                    member.name, member.role_type
                )
            }
        }
    }

    /// Poll all watchers and handle state transitions.
    fn poll_watchers(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.watchers.keys().cloned().collect();

        for name in &member_names {
            let prev_state = self.states.get(name).copied();
            let prev_watcher_state = self
                .watchers
                .get(name)
                .map(|watcher| watcher.state)
                .unwrap_or(WatcherState::Idle);
            let (new_state, completion_observed, session_size_bytes) = {
                let watcher = match self.watcher_mut(name) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        warn!(member = %name, error = %error, "watcher missing during poll");
                        continue;
                    }
                };
                match watcher.poll() {
                    Ok(new_state) => (
                        new_state,
                        watcher.take_completion_event(),
                        watcher.current_session_size_bytes(),
                    ),
                    Err(e) => {
                        warn!(member = %name, error = %e, "watcher poll failed");
                        continue;
                    }
                }
            };

            let member_state = match new_state {
                WatcherState::Active => MemberState::Working,
                WatcherState::Idle => MemberState::Idle,
                WatcherState::PaneDead => MemberState::Idle,
                WatcherState::ContextExhausted => MemberState::Working,
            };

            if prev_state != Some(member_state) {
                self.states.insert(name.clone(), member_state);

                // Update automation countdowns on state transitions.
                self.update_automation_timers_for_state(name, member_state);
            }

            if new_state == WatcherState::PaneDead {
                if prev_watcher_state != WatcherState::PaneDead {
                    warn!(member = %name, "detected pane death");
                    self.emit_event(TeamEvent::pane_death(name));
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected pane death for {}",
                        name
                    ));
                }
                if let Err(error) = self.handle_pane_death(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "pane-death respawn handling failed; continuing"
                    );
                }
                continue;
            }

            if new_state == WatcherState::ContextExhausted {
                if prev_watcher_state != WatcherState::ContextExhausted {
                    let task_id = self.active_task_id(name);
                    warn!(
                        member = %name,
                        task_id,
                        session_size_bytes,
                        "detected context exhaustion"
                    );
                    self.record_context_exhausted(name, task_id, session_size_bytes);
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected context exhaustion for {} (task={:?}, session_size_bytes={:?})",
                        name, task_id, session_size_bytes
                    ));
                }
                if let Err(error) = self.handle_context_exhaustion(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "context-exhausted restart handling failed; continuing"
                    );
                }
                continue;
            }

            if completion_observed && self.active_task_id(name).is_some() {
                info!(member = %name, "detected task completion");
                if let Err(error) = merge::handle_engineer_completion(self, name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "engineer completion handling failed; continuing"
                    );
                }
            }
        }

        Ok(())
    }

    fn handle_pane_death(&mut self, member_name: &str) -> Result<()> {
        self.restart_member(member_name)
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
        if !self.is_git_repo {
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

    fn active_task(&self, member_name: &str) -> Result<Option<crate::task::Task>> {
        let Some(task_id) = self.active_task_id(member_name) else {
            return Ok(None);
        };
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        Ok(tasks.into_iter().find(|task| task.id == task_id))
    }

    fn context_restart_cooldown_key(member_name: &str) -> String {
        format!("context-restart::{member_name}")
    }

    fn context_escalation_cooldown_key(member_name: &str) -> String {
        format!("context-escalation::{member_name}")
    }

    fn context_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .count() as u32;
        Ok(count)
    }

    fn restart_assignment_message(task: &crate::task::Task) -> String {
        let mut message = format!(
            "Continuing Task #{}: {}\nPrevious session exhausted context; resume from the current worktree state and continue.\n\n{}",
            task.id, task.title, task.description
        );
        if let Some(branch) = task.branch.as_deref() {
            message.push_str(&format!("\n\nBranch: {branch}"));
        }
        if let Some(worktree_path) = task.worktree_path.as_deref() {
            message.push_str(&format!("\nWorktree: {worktree_path}"));
        }
        message
    }

    pub(super) fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn clear_active_task(&mut self, engineer: &str) {
        self.active_tasks.remove(engineer);
        self.retry_counts.remove(engineer);
    }

    /// Remove active_task entries for tasks that are done, archived, or no longer on the board.
    fn reconcile_active_tasks(&mut self) -> Result<()> {
        if self.active_tasks.is_empty() {
            return Ok(());
        }
        let tasks_dir = self.board_dir().join("tasks");
        let board_tasks = if tasks_dir.exists() {
            crate::task::load_tasks_from_dir(&tasks_dir)?
        } else {
            Vec::new()
        };
        let stale: Vec<(String, u32)> = self
            .active_tasks
            .iter()
            .filter(|(_engineer, task_id)| {
                let task_id = **task_id;
                match board_tasks.iter().find(|t| t.id == task_id) {
                    Some(task) => task.status == "done" || task.status == "archived",
                    None => true, // task no longer exists
                }
            })
            .map(|(engineer, task_id)| (engineer.clone(), *task_id))
            .collect();
        for (engineer, task_id) in stale {
            info!(
                engineer = %engineer,
                task_id,
                "Reconciled stale active_task: {engineer} was tracking done task #{task_id}"
            );
            self.clear_active_task(&engineer);
        }
        Ok(())
    }

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

    fn maybe_escalate_stale_reviews(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }
        let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let nudge_threshold = self
            .config
            .team_config
            .workflow_policy
            .review_nudge_threshold_secs;
        let timeout_threshold = self.config.team_config.workflow_policy.review_timeout_secs;

        // Collect IDs of tasks currently in review
        let review_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|t| t.status == "review")
            .map(|t| t.id)
            .collect();

        // Prune tracking maps for tasks no longer in review
        self.review_first_seen
            .retain(|id, _| review_task_ids.contains(id));
        self.review_nudge_sent
            .retain(|id| review_task_ids.contains(id));

        for task in &tasks {
            if task.status != "review" {
                continue;
            }

            let first_seen = *self.review_first_seen.entry(task.id).or_insert(now);
            let age = now.saturating_sub(first_seen);

            // Check escalation first (higher threshold)
            if age >= timeout_threshold {
                // Escalate to architect
                let architect = self
                    .config
                    .members
                    .iter()
                    .find(|m| m.role_type == RoleType::Architect)
                    .map(|m| m.name.clone());

                if let Some(architect_name) = architect {
                    let msg = format!(
                        "Review timeout: task #{} has been in review for {}s (threshold: {}s). \
                         Escalating for resolution.",
                        task.id, age, timeout_threshold,
                    );
                    let _ = self.queue_daemon_message(&architect_name, &msg);
                    self.record_orchestrator_action(format!(
                        "review_escalated: task #{} -> {architect_name}",
                        task.id,
                    ));
                }

                if let Err(error) = self.event_sink.emit(TeamEvent::review_escalated(
                    &task.id.to_string(),
                    &format!("review timeout after {age}s"),
                )) {
                    warn!(error = %error, "failed to emit review_escalated event");
                }

                // Transition to blocked
                let _ = super::task_cmd::transition_task(&board_dir, task.id, "blocked");
                let _ = super::task_cmd::cmd_update(
                    &board_dir,
                    task.id,
                    std::collections::HashMap::from([(
                        "blocked_on".to_string(),
                        "review timeout escalated to architect".to_string(),
                    )]),
                );

                // Remove from tracking since it's no longer in review
                self.review_first_seen.remove(&task.id);
                self.review_nudge_sent.remove(&task.id);
                continue;
            }

            // Check nudge threshold
            if age >= nudge_threshold && !self.review_nudge_sent.contains(&task.id) {
                let reviewer = task.review_owner.as_deref().unwrap_or("manager");
                let msg = format!(
                    "Review nudge: task #{} has been in review for {}s (nudge threshold: {}s). \
                     Please review or escalate.",
                    task.id, age, nudge_threshold,
                );
                let _ = self.queue_daemon_message(reviewer, &msg);
                self.record_orchestrator_action(format!(
                    "review_nudge_sent: task #{} -> {reviewer}",
                    task.id,
                ));

                if let Err(error) = self
                    .event_sink
                    .emit(TeamEvent::review_nudge_sent(reviewer, &task.id.to_string()))
                {
                    warn!(error = %error, "failed to emit review_nudge_sent event");
                }

                self.review_nudge_sent.insert(task.id);
            }
        }

        Ok(())
    }

    fn maybe_auto_unblock_blocked_tasks(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let done_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|task| task.status == "done")
            .map(|task| task.id)
            .collect();
        let unblocked_tasks = tasks
            .iter()
            .filter(|task| task.status == "blocked")
            .filter(|task| !task.depends_on.is_empty())
            .filter(|task| {
                task.depends_on
                    .iter()
                    .all(|dependency| done_task_ids.contains(dependency))
            })
            .map(|task| {
                (
                    task.id,
                    task.title.clone(),
                    task.depends_on.clone(),
                    self.auto_unblock_notification_recipient(task),
                )
            })
            .collect::<Vec<_>>();

        for (task_id, title, dependencies, recipient) in unblocked_tasks {
            task_cmd::cmd_transition(&board_dir, task_id, "todo")
                .with_context(|| format!("failed to auto-unblock task #{task_id}"))?;

            let dependency_list = dependencies
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let event_role = recipient.as_deref().unwrap_or("daemon");
            self.record_task_unblocked(event_role, task_id.to_string());
            self.record_orchestrator_action(format!(
                "dependency resolution: auto-unblocked task #{} ({}) after dependencies [{}] completed",
                task_id, title, dependency_list
            ));
            info!(
                task_id,
                task_title = %title,
                dependencies = %dependency_list,
                recipient = recipient.as_deref().unwrap_or("none"),
                "auto-unblocked blocked task"
            );

            let Some(recipient) = recipient else {
                continue;
            };
            let body = format!(
                "Task #{task_id} ({title}) was automatically moved from `blocked` to `todo` because dependencies [{dependency_list}] are done."
            );
            if let Err(error) = self.queue_daemon_message(&recipient, &body) {
                warn!(
                    task_id,
                    to = %recipient,
                    error = %error,
                    "failed to notify auto-unblocked task recipient"
                );
            }
        }

        Ok(())
    }

    /// Update automation countdowns when a member's state changes.
    fn update_automation_timers_for_state(&mut self, member_name: &str, new_state: MemberState) {
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

    fn manager_for_member_name(&self, member_name: &str) -> Option<&str> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.reports_to.as_deref())
    }

    fn auto_unblock_notification_recipient(&self, task: &crate::task::Task) -> Option<String> {
        task.claimed_by
            .as_deref()
            .filter(|owner| {
                self.config
                    .members
                    .iter()
                    .any(|member| member.name == *owner)
            })
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.role_type == RoleType::Manager)
                    .map(|member| member.name.clone())
            })
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

    fn maybe_detect_pipeline_starvation(&mut self) -> Result<()> {
        let Some(threshold) = self
            .config
            .team_config
            .workflow_policy
            .pipeline_starvation_threshold
        else {
            self.pipeline_starvation_fired = false;
            return Ok(());
        };

        // Already fired — stay suppressed until condition fully clears
        if self.pipeline_starvation_fired {
            // Only reset when enough unclaimed work exists for all idle engineers
            let board_dir = self
                .config
                .project_root
                .join(".batty")
                .join("team_config")
                .join("board");
            let all_tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
            let unclaimed_todo = all_tasks
                .iter()
                .filter(|t| matches!(t.status.as_str(), "todo" | "backlog"))
                .filter(|t| t.claimed_by.is_none())
                .count();
            let truly_idle = self.truly_idle_engineer_count(&all_tasks);
            if truly_idle == 0 || unclaimed_todo > truly_idle {
                self.pipeline_starvation_fired = false;
                self.pipeline_starvation_last_fired = None;
            } else {
                return Ok(());
            }
        }

        // Hard cooldown: never fire more than once per 5 minutes
        const STARVATION_COOLDOWN: Duration = Duration::from_secs(300);
        if let Some(last) = self.pipeline_starvation_last_fired {
            if last.elapsed() < STARVATION_COOLDOWN {
                return Ok(());
            }
        }

        // Suppress if manager is actively working (likely processing directives)
        let manager_working = self.config.members.iter().any(|m| {
            m.role_type == RoleType::Manager
                && self.states.get(&m.name) == Some(&MemberState::Working)
        });
        if manager_working {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let all_tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let idle_count = self.truly_idle_engineer_count(&all_tasks);
        if idle_count == 0 {
            return Ok(());
        }

        let todo_count = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
            .filter(|task| task.claimed_by.is_none())
            .count();

        let deficit = idle_count.saturating_sub(todo_count);
        if todo_count >= idle_count || deficit < threshold {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let architects: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .collect();
        if architects.is_empty() {
            return Ok(());
        }

        let message =
            format!("Pipeline running dry: {idle_count} idle engineers, {todo_count} todo tasks.");
        for architect in &architects {
            let visible_sender = self.automation_sender_for(architect);
            let inbox_msg = inbox::InboxMessage::new_send(&visible_sender, architect, &message);
            inbox::deliver_to_inbox(&inbox_root, &inbox_msg)?;
        }
        self.pipeline_starvation_fired = true;
        self.pipeline_starvation_last_fired = Some(Instant::now());
        Ok(())
    }

    /// Count engineers that are tmux-idle AND have no active board items.
    fn truly_idle_engineer_count(&self, all_tasks: &[crate::task::Task]) -> usize {
        let engineers_with_active_items: std::collections::HashSet<String> = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "in-progress" | "review"))
            .filter_map(|task| task.claimed_by.as_ref())
            .map(|name| name.trim_start_matches('@').to_string())
            .collect();

        self.idle_engineer_names()
            .into_iter()
            .filter(|name| !engineers_with_active_items.contains(name))
            .count()
    }

    fn member_worktree_context(&self, member_name: &str) -> Option<MemberWorktreeContext> {
        if !self.member_uses_worktrees(member_name) {
            return None;
        }
        let worktree_path = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        if !worktree_path.exists() {
            return None;
        }

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&worktree_path)
            .output()
            .ok()
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            })
            .filter(|branch| !branch.is_empty());

        Some(MemberWorktreeContext {
            path: worktree_path,
            branch,
        })
    }

    /// Rotate the board if enough time has passed.
    ///
    /// When using kanban-md (board/ directory), rotation is not needed — each
    /// task is an individual file. Only rotates the legacy plain kanban.md.
    fn maybe_rotate_board(&mut self) -> Result<()> {
        // Check every 10 minutes
        if self.last_board_rotation.elapsed() < Duration::from_secs(600) {
            return Ok(());
        }

        self.last_board_rotation = Instant::now();

        let config_dir = self.config.project_root.join(".batty").join("team_config");

        // kanban-md uses a board/ directory — no rotation needed
        let board_dir = config_dir.join("board");
        if board_dir.is_dir() {
            return Ok(());
        }

        // Legacy plain kanban.md — rotate done items
        let kanban_path = config_dir.join("kanban.md");
        let archive_path = config_dir.join("kanban-archive.md");

        if kanban_path.exists() {
            match board::rotate_done_items(
                &kanban_path,
                &archive_path,
                self.config.team_config.board.rotation_threshold,
            ) {
                Ok(rotated) if rotated > 0 => {
                    info!(rotated, "board rotation completed");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "board rotation failed");
                }
            }
        }

        Ok(())
    }

    fn maybe_recycle_cron_tasks(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let recycled = super::task_loop::recycle_cron_tasks(&board_dir)?;
        for (task_id, cron_expr) in recycled {
            self.emit_event(TeamEvent::task_recycled(task_id, &cron_expr));
            self.record_orchestrator_action(format!(
                "cron: recycled task #{task_id} (schedule: {cron_expr}) back to todo"
            ));
        }
        Ok(())
    }

    fn maybe_generate_retrospective(&mut self) -> Result<()> {
        let Some(stats) = super::retrospective::should_generate_retro(
            &self.config.project_root,
            self.retro_generated,
            self.config.team_config.retro_min_duration_secs,
        )?
        else {
            return Ok(());
        };

        let report_path =
            super::retrospective::generate_retrospective(&self.config.project_root, &stats)?;
        self.retro_generated = true;
        self.record_retro_generated();
        info!(path = %report_path.display(), "retrospective generated");
        Ok(())
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

fn ensure_kanban_available() -> Result<()> {
    let output = std::process::Command::new("kanban-md")
        .arg("--help")
        .output()
        .context(
            "daemon startup pre-flight failed while verifying board tooling: could not execute `kanban-md --help`",
        )?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        "unknown error".to_string()
    } else {
        stderr
    };
    bail!("daemon startup pre-flight failed: `kanban-md --help` failed: {detail}");
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
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, backdate_idle_grace, engineer_member, init_git_repo,
        manager_member, setup_fake_claude, write_board_task_file, write_open_task_file,
        write_owned_task_file, write_owned_task_file_with_context,
    };
    use crate::team::watcher::WatcherState;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn production_unwrap_expect_count(path: &Path) -> usize {
        let content = std::fs::read_to_string(path).unwrap();
        let test_split = content.split("\n#[cfg(test)]").next().unwrap_or(&content);
        test_split
            .lines()
            .filter(|line| line.contains(".unwrap(") || line.contains(".expect("))
            .count()
    }
    use std::sync::{LazyLock, Mutex};

    static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
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

    fn setup_fake_kanban(tmp: &tempfile::TempDir, script_name: &str) -> PathBuf {
        let fake_bin = tmp.path().join(format!("{script_name}-bin"));
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_kanban = fake_bin.join("kanban-md");
        std::fs::write(
            &fake_kanban,
            r#"#!/bin/bash
if [ "$1" = "--help" ]; then
  echo "kanban-md fake help"
  exit 0
fi
if [ "$1" = "init" ]; then
  shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "--dir" ]; then
      shift
      mkdir -p "$1/tasks"
      exit 0
    fi
    shift
  done
fi
echo "unsupported fake kanban invocation" >&2
exit 1
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_kanban, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_bin
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

    fn test_team_config(name: &str) -> TeamConfig {
        TeamConfig {
            name: name.to_string(),
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
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            retro_min_duration_secs: 60,
            roles: Vec::new(),
        }
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
    fn test_retry_count_increments_and_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.active_task_id("eng-2"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
    }

    #[test]
    fn test_retry_count_triggers_escalation_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        assert_eq!(daemon.increment_retry("eng-1"), 3);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn test_active_task_id_returns_none_for_unassigned() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn nonfatal_kanban_failures_are_relayed_to_known_members() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None)])
            .build();

        daemon.report_nonfatal_kanban_failure(
            "move task #42 to done",
            "kanban-md stderr goes here",
            ["manager"],
        );

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("move task #42 to done"));
        assert!(messages[0].body.contains("kanban-md stderr goes here"));
    }

    #[test]
    #[serial]
    fn poll_watchers_respawns_pane_dead_member_and_records_events() {
        let session = format!("batty-test-restart-dead-member-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "architect-restart";
        let project_slug = tmp
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
        let _ = std::fs::remove_dir_all(&fake_bin);
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_log = tmp.path().join("fake-claude.log");
        let fake_claude = fake_bin.join("claude");
        std::fs::write(
            &fake_claude,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 5\n",
                fake_log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.poll_watchers().unwrap();
        std::thread::sleep(Duration::from_millis(700));

        assert!(!crate::tmux::pane_dead(&pane_id).unwrap_or(true));
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let launch_state = load_launch_state(tmp.path());
        let identity = launch_state
            .get(member_name)
            .expect("missing restarted member launch state");
        assert_eq!(identity.agent, "claude-code");
        assert!(identity.session_id.is_some());

        let events = std::fs::read_to_string(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"event\":\"pane_death\""));
        assert!(events.contains("\"event\":\"pane_respawned\""));
        assert!(events.contains("\"event\":\"member_crashed\""));
        assert!(events.contains(&format!("\"role\":\"{member_name}\"")));
        assert!(events.contains("\"restart\":true"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn spawn_all_agents_corrects_mismatched_cwd_before_launch() {
        let session = format!("batty-test-spawn-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by spawned member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let current = (0..50)
            .find_map(|_| match crate::tmux::pane_current_path(&pane_id) {
                Ok(current) if !current.is_empty() => Some(current),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!("tmux pane current path never became available for target '{pane_id}'")
            });
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let corrected = events
            .iter()
            .find(|event| event.event == "cwd_corrected")
            .expect("expected cwd_corrected event during spawn");
        assert_eq!(corrected.role.as_deref(), Some(member_name));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(tmp.path().to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn restart_member_corrects_mismatched_cwd_after_respawn() {
        let session = format!("batty-test-restart-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-restart-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            wrong_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.restart_member(member_name).unwrap();
        std::thread::sleep(Duration::from_millis(700));

        let log = (0..20)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some(member_name)
        }));
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name)
                && event.reason.as_deref() == Some(tmp.path().to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_relaunches_context_exhausted_member_with_task_context() {
        let session = format!("batty-test-agent-restart-context-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-restart";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            191,
            "active-task",
            "in-progress",
            member_name,
            "eng-1-2/191",
            &worktree_path.display().to_string(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("Continuing Task #191") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("Previous session exhausted context"));
        assert!(log.contains("Branch: eng-1-2/191"));
        assert!(log.contains(&format!("Worktree: {}", worktree_path.display())));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let restart_events = events
            .iter()
            .filter(|event| event.event == "agent_restarted")
            .collect::<Vec<_>>();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].role.as_deref(), Some(member_name));
        assert_eq!(restart_events[0].task.as_deref(), Some("191"));
        assert_eq!(
            restart_events[0].reason.as_deref(),
            Some("context_exhausted")
        );
        assert_eq!(restart_events[0].restart_count, Some(1));
        assert!(events.iter().any(|event| {
            event.event == "message_routed"
                && event.from.as_deref() == Some("daemon")
                && event.to.as_deref() == Some(member_name)
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn context_exhaustion_relaunch_corrects_mismatched_cwd() {
        let session = "batty-test-context-cwd-correct";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "eng-context-cwd";
        let lead_name = "lead";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            wrong_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);

        daemon.handle_context_exhaustion(member_name).unwrap();
        let current = (0..50)
            .find_map(|_| match crate::tmux::pane_current_path(&pane_id) {
                Ok(path) if !path.is_empty() => Some(path),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .expect("expected relaunched pane to report a current working directory");
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name)
                && event.reason.as_deref() == Some(tmp.path().to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_second_exhaustion_escalates_instead_of_restarting() {
        let session = "batty-test-agent-restart-escalate";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-escalate";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_restart_cooldown_key(member_name),
            Instant::now(),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);
        write_event_log(
            tmp.path(),
            &[TeamEvent::agent_restarted(
                member_name,
                "191",
                "context_exhausted",
                1,
            )],
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        let pending = inbox::pending_messages(&root, lead_name).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #191"));
        assert!(pending[0].body.contains("split the task"));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(!log.contains("Continuing Task #191"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "agent_restarted")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "task_escalated")
                .count(),
            1
        );

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_respects_cooldown_before_first_restart() {
        let session = "batty-test-agent-restart-cooldown";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cooldown";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_restart_cooldown_key(member_name),
            Instant::now(),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        inbox::init_inbox(&root, member_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);

        daemon.handle_context_exhaustion(member_name).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(log.is_empty());
        assert!(
            inbox::pending_messages(&root, lead_name)
                .unwrap()
                .is_empty()
        );
        assert!(
            inbox::pending_messages(&root, member_name)
                .unwrap()
                .is_empty()
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.is_empty());

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn startup_preflight_reports_missing_kanban_binary() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let empty_bin = tmp.path().join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let _path = EnvVarGuard::set("PATH", empty_bin.to_string_lossy().as_ref());

        let error = ensure_kanban_available().unwrap_err();

        assert!(format!("{error:#}").contains("kanban-md"));
    }

    #[test]
    fn startup_preflight_initializes_missing_board_directory() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_board_init");
        let fake_bin = setup_fake_kanban(&tmp, "startup-board-init");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let board_path = board_dir(&repo);
        assert!(!board_path.exists());

        assert!(ensure_board_initialized(&repo).unwrap());
        assert!(board_path.join("tasks").is_dir());
        assert!(!ensure_board_initialized(&repo).unwrap());
    }

    #[test]
    #[serial]
    fn startup_preflight_respawns_dead_pane_and_bootstraps_board() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let session = format!("batty-test-startup-preflight-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_preflight");
        let fake_bin = setup_fake_kanban(&tmp, "startup-preflight");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "eng-1",
            "bash",
            &[],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();

        let architect_pane = crate::tmux::pane_id(&session).unwrap();
        let engineer_pane = crate::tmux::pane_id(&format!("{session}:eng-1")).unwrap();
        Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-t",
                &engineer_pane,
                "remain-on-exit",
                "on",
            ])
            .output()
            .unwrap();
        crate::tmux::send_keys(&engineer_pane, "exit", true).unwrap();
        for _ in 0..20 {
            if crate::tmux::pane_dead(&engineer_pane).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(crate::tmux::pane_dead(&engineer_pane).unwrap());

        let members = vec![
            architect_member("architect"),
            engineer_member("eng-1", Some("architect"), false),
        ];
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("startup-preflight"),
            session: session.clone(),
            members,
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.run_startup_preflight().unwrap();

        assert!(!crate::tmux::pane_dead(&engineer_pane).unwrap_or(true));
        assert!(board_dir(&repo).join("tasks").is_dir());

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("eng-1")
        }));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn mark_member_working_updates_state_and_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        watchers.insert(
            "architect".to_string(),
            SessionWatcher::new("%0", "architect", 300, None),
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .watchers(watchers)
            .build();

        daemon.mark_member_working("architect");

        assert_eq!(daemon.states.get("architect"), Some(&MemberState::Working));
        assert_eq!(
            daemon
                .watchers
                .get("architect")
                .map(|watcher| watcher.state),
            Some(WatcherState::Active)
        );
    }

    #[test]
    #[serial]
    fn pre_assignment_health_check_corrects_mismatched_cwd() {
        let session = format!("batty-test-health-check-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let wrong_dir = tmp.path().join("wrong");
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&wrong_dir).unwrap();
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(&expected_dir)
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let corrected = events
            .iter()
            .find(|event| event.event == "cwd_corrected")
            .expect("expected cwd_corrected event");
        assert_eq!(corrected.role.as_deref(), Some("eng-1"));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(expected_dir.to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    fn pre_assignment_health_check_cwd_matching_path_passes_silently() {
        let session = format!("batty-test-health-check-cwd-match-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(
            &session,
            "bash",
            &[],
            expected_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(
            events.iter().all(|event| event.event != "cwd_corrected"),
            "did not expect cwd_corrected event when pane cwd already matched"
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    fn startup_cwd_validation_corrects_all_agent_panes() {
        let session = format!("batty-test-startup-cwd-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup-cwd");
        let wrong_architect_dir = tmp.path().join("wrong-architect");
        let wrong_engineer_dir = tmp.path().join("wrong-engineer");
        std::fs::create_dir_all(&wrong_architect_dir).unwrap();
        std::fs::create_dir_all(&wrong_engineer_dir).unwrap();

        crate::tmux::create_session(
            &session,
            "bash",
            &[],
            wrong_architect_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        crate::tmux::create_window(
            &session,
            "eng-1",
            "bash",
            &[],
            wrong_engineer_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let architect_pane = crate::tmux::pane_id(&session).unwrap();
        let engineer_pane = crate::tmux::pane_id(&format!("{session}:eng-1")).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: TeamConfig {
                name: "test".to_string(),
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
                event_log_max_bytes: 10 * 1024 * 1024,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![architect, engineer],
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane.clone()),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.validate_member_panes_on_startup();

        let engineer_expected_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let architect_current = crate::tmux::pane_current_path(&architect_pane).unwrap();
        let engineer_current = crate::tmux::pane_current_path(&engineer_pane).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&architect_current)),
            normalized_assignment_dir(&repo)
        );
        assert_eq!(
            normalized_assignment_dir(Path::new(&engineer_current)),
            normalized_assignment_dir(&engineer_expected_dir)
        );

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some("architect")
                && event.reason.as_deref() == Some(repo.to_string_lossy().as_ref())
        }));
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some(engineer_expected_dir.to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
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
            watchers.insert(
                "scientist".to_string(),
                SessionWatcher::new(&pane_id, "scientist", 300, None),
            );
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
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let session = "batty-test-triage-live-delivery";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        watchers.insert(
            "lead".to_string(),
            SessionWatcher::new(&pane_id, "lead", 300, None),
        );
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .session(session)
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
            let pane = (0..20)
                .find_map(|_| {
                    let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                    if pane.contains("batty inbox lead") {
                        Some(pane)
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
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

        crate::tmux::kill_session(session).unwrap();
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
    fn maybe_auto_unblock_moves_blocked_task_to_todo_and_notifies_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        write_board_task_file(tmp.path(), 11, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 12, "dep-b", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            13,
            "blocked-task",
            "blocked",
            Some("eng-1"),
            &[11, 12],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let task = tasks.iter().find(|task| task.id == 13).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.blocked_on.is_none());
        assert!(task.blocked.is_none());

        let pending = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #13 (blocked-task)"));
        assert!(
            pending[0]
                .body
                .contains("automatically moved from `blocked` to `todo`")
        );
        assert!(pending[0].body.contains("[11, 12]"));

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("13")
        }));
    }

    #[test]
    fn maybe_auto_unblock_notifies_manager_when_task_is_unowned() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let inbox_root = inbox::inboxes_root(tmp.path());
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 21, "dep-a", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            22,
            "blocked-task",
            "blocked",
            None,
            &[21],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #22 (blocked-task)"));

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("manager")
                && event.task.as_deref() == Some("22")
        }));
    }

    #[test]
    fn maybe_auto_unblock_leaves_unresolved_or_dependency_free_tasks_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 31, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 32, "dep-b", "review", None, &[], None);
        write_board_task_file(
            tmp.path(),
            33,
            "blocked-partial",
            "blocked",
            None,
            &[31, 32],
            Some("waiting on dependencies"),
        );
        write_board_task_file(
            tmp.path(),
            34,
            "blocked-no-deps",
            "blocked",
            None,
            &[],
            Some("manual hold"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let partial = tasks.iter().find(|task| task.id == 33).unwrap();
        assert_eq!(partial.status, "blocked");
        assert_eq!(
            partial.blocked_on.as_deref(),
            Some("waiting on dependencies")
        );

        let no_deps = tasks.iter().find(|task| task.id == 34).unwrap();
        assert_eq!(no_deps.status, "blocked");
        assert_eq!(no_deps.blocked_on.as_deref(), Some("manual hold"));

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(pending.is_empty());

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.task.as_deref(), Some("33" | "34")))
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
    fn auto_retro_fires_when_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1", Some("45")),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .orchestrator_pane(false)
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        daemon.maybe_generate_retrospective().unwrap();

        assert!(daemon.retro_generated);
        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }

    #[test]
    fn auto_retro_does_not_fire_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1", Some("45")),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .orchestrator_pane(false)
            .build();
        daemon.event_sink = EventSink::new(&events_path).unwrap();

        daemon.maybe_generate_retrospective().unwrap();
        daemon.maybe_generate_retrospective().unwrap();

        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
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
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-review-task-{id}.md")),
            format!(
                "---\nid: {id}\ntitle: review-task-{id}\nstatus: review\npriority: high\nclass: standard\nclaimed_by: eng-1\nreview_owner: {review_owner}\n---\n\nTask description.\n"
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
}
